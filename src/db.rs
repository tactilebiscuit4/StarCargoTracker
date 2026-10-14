//! SQLite storage for recorded trades.

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::parser::{Side, TradeRequest};
use crate::profit::WriteOff;
use crate::tracker::Outcome;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS trades (
    id             INTEGER PRIMARY KEY,
    timestamp      TEXT NOT NULL,  -- UTC, e.g. 2026-10-04T06:20:30.252Z
    side           TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
    status         TEXT NOT NULL CHECK (status IN ('confirmed', 'failed')),
    failure_reason TEXT,
    shop_id        TEXT NOT NULL,
    shop_name      TEXT NOT NULL,
    kiosk_id       TEXT NOT NULL,
    resource_guid  TEXT NOT NULL,
    scu            REAL NOT NULL,
    total_price    REAL NOT NULL,
    location       TEXT,           -- game location code, e.g. RR_ARC_LEO
    ship_id        TEXT,           -- entity ID of the ship auto-loaded to/from
    -- Set when the user says the trade didn't really happen. The row is kept
    -- so re-reading the log doesn't record it again.
    cancelled_at   TEXT,
    -- Re-reading the same log line must not record the trade twice.
    UNIQUE (timestamp, side, kiosk_id, resource_guid)
);
CREATE INDEX IF NOT EXISTS trades_timestamp ON trades (timestamp);
CREATE TABLE IF NOT EXISTS commodities (
    guid TEXT PRIMARY KEY,
    name TEXT NOT NULL
);
-- Cargo the user recorded as lost.
CREATE TABLE IF NOT EXISTS write_offs (
    id            INTEGER PRIMARY KEY,
    timestamp     TEXT NOT NULL,
    resource_guid TEXT NOT NULL,
    scu           REAL NOT NULL,
    note          TEXT,
    ship_id       TEXT             -- set when written off because this ship was destroyed
);
-- Ship destructions already handled, so re-reading a log doesn't write the
-- cargo off again (even if the user deleted the write-off).
CREATE TABLE IF NOT EXISTS ship_losses (
    timestamp TEXT NOT NULL,
    ship_id   TEXT NOT NULL,
    ship_name TEXT NOT NULL,
    PRIMARY KEY (timestamp, ship_id)
);
-- The player's ships, keyed by the game's entity ID.
CREATE TABLE IF NOT EXISTS ships (
    ship_id   TEXT PRIMARY KEY,
    class     TEXT,                -- from the log, e.g. ANVL_Asgard
    name      TEXT,                -- set by the user
    last_seen TEXT,                -- when the log last named this ship
    -- After this ship was destroyed, the ship its name passed to (its
    -- likely claimed replacement), so the name passes on only once.
    renamed_to TEXT
);
-- Settings, e.g. for UEX account syncing.
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- Names the user has given to location codes.
CREATE TABLE IF NOT EXISTS location_names (
    code TEXT PRIMARY KEY,
    name TEXT NOT NULL
);
";

const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";

#[derive(Debug, Clone, PartialEq)]
pub struct Ship {
    pub ship_id: String,
    /// e.g. `ANVL_Asgard`; unknown until the log names the ship.
    pub class: Option<String>,
    pub name: Option<String>,
    pub last_seen: Option<DateTime<Utc>>,
    /// Completed trades loaded to or from this ship.
    pub trades: i64,
    /// When it was destroyed, if the log hasn't seen it since. Claimed
    /// ships come back with a new ID, so a destroyed ID is gone for good.
    pub destroyed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Totals {
    pub spent: f64,
    pub earned: f64,
    pub confirmed: i64,
    pub failed: i64,
    pub cancelled: i64,
}

/// A trade as stored, with its ID for `cancel-trade`.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredTrade {
    pub id: i64,
    pub outcome: Outcome,
    pub cancelled: bool,
}

/// Columns read by `outcome_from_row`, then the ID and cancellation.
const STORED_TRADE_COLUMNS: &str = "timestamp, side, status, failure_reason, shop_id, shop_name,
    kiosk_id, resource_guid, scu, total_price, location, ship_id, id, cancelled_at";

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // The tracker and the `cargo>` prompt use separate connections; wait
        // for the other's write to finish instead of failing.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init(conn)
    }

    #[cfg(test)]
    fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    #[cfg(test)]
    pub fn open_in_memory_for_tests() -> Self {
        Self::open_in_memory().unwrap()
    }

    /// For the UEX cache, which keeps its own tables (see `uex::SCHEMA`).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        crate::uex::create_tables(&conn)?;
        conn.execute_batch(crate::uex_sync::SCHEMA)?;
        // Columns added after the first release, missing from older databases.
        add_column_if_missing(&conn, "trades", "location")?;
        add_column_if_missing(&conn, "trades", "ship_id")?;
        add_column_if_missing(&conn, "write_offs", "ship_id")?;
        add_column_if_missing(&conn, "trades", "cancelled_at")?;
        add_column_if_missing(&conn, "ships", "renamed_to")?;
        Ok(Self { conn })
    }

    /// Saves a trade. Returns its new ID, or `None` if it was already recorded.
    pub fn record(&self, outcome: &Outcome) -> rusqlite::Result<Option<i64>> {
        let (trade, status, reason) = match outcome {
            Outcome::Confirmed(t) => (t, "confirmed", None),
            Outcome::Failed(t, reason) => (t, "failed", Some(reason.as_str())),
        };
        let timestamp = trade.timestamp.format(TIMESTAMP_FORMAT).to_string();
        let side = side_to_str(trade.side);
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO trades
                (timestamp, side, status, failure_reason, shop_id, shop_name,
                 kiosk_id, resource_guid, scu, total_price, location, ship_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                timestamp,
                side,
                status,
                reason,
                trade.shop_id,
                trade.shop_name,
                trade.kiosk_id,
                trade.resource_guid,
                trade.scu,
                trade.total_price,
                trade.location,
                trade.ship_id,
            ],
        )?;
        if inserted == 0 && (trade.location.is_some() || trade.ship_id.is_some()) {
            // Fill in details for trades recorded before they were tracked.
            self.conn.execute(
                "UPDATE trades SET location = COALESCE(location, ?1), ship_id = COALESCE(ship_id, ?2)
                 WHERE timestamp = ?3 AND side = ?4 AND kiosk_id = ?5 AND resource_guid = ?6",
                params![
                    trade.location,
                    trade.ship_id,
                    timestamp,
                    side,
                    trade.kiosk_id,
                    trade.resource_guid
                ],
            )?;
        }
        Ok((inserted > 0).then(|| self.conn.last_insert_rowid()))
    }

    /// The most recent `limit` trades, oldest first. Trades that failed in
    /// the game are only included if `include_failed`.
    pub fn recent(&self, limit: usize, include_failed: bool) -> rusqlite::Result<Vec<StoredTrade>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {STORED_TRADE_COLUMNS}
             FROM (SELECT * FROM trades WHERE ?2 OR status != 'failed'
                   ORDER BY timestamp DESC, id DESC LIMIT ?1)
             ORDER BY timestamp, id"
        ))?;
        let rows = stmt.query_map(params![limit as i64, include_failed], stored_trade_from_row)?;
        rows.collect()
    }

    pub fn setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| row.get(0))
            .optional()
    }

    /// Saves a setting, or removes it with `None`.
    pub fn set_setting(&self, key: &str, value: Option<&str>) -> rusqlite::Result<()> {
        match value {
            Some(value) => self.conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [key, value],
            )?,
            None => self.conn.execute("DELETE FROM settings WHERE key = ?1", [key])?,
        };
        Ok(())
    }

    /// Every completed trade, cancelled or not, oldest first, for syncing.
    pub fn completed_trades_with_ids(&self) -> rusqlite::Result<Vec<StoredTrade>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {STORED_TRADE_COLUMNS} FROM trades WHERE status = 'confirmed'
             ORDER BY timestamp, id"
        ))?;
        let rows = stmt.query_map([], stored_trade_from_row)?;
        rows.collect()
    }

    /// The location code of the most recent trade, if any trade has one.
    pub fn last_location(&self) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT location FROM trades WHERE location IS NOT NULL
                 ORDER BY timestamp DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn trade(&self, id: i64) -> rusqlite::Result<Option<StoredTrade>> {
        self.conn
            .query_row(
                &format!("SELECT {STORED_TRADE_COLUMNS} FROM trades WHERE id = ?1"),
                [id],
                stored_trade_from_row,
            )
            .optional()
    }

    /// Marks a trade as cancelled (or not). Returns `false` if it already was.
    pub fn set_cancelled(&self, id: i64, cancelled: bool) -> rusqlite::Result<bool> {
        let changed = if cancelled {
            self.conn.execute(
                "UPDATE trades SET cancelled_at = ?2 WHERE id = ?1 AND cancelled_at IS NULL",
                params![id, Utc::now().format(TIMESTAMP_FORMAT).to_string()],
            )?
        } else {
            self.conn.execute(
                "UPDATE trades SET cancelled_at = NULL WHERE id = ?1 AND cancelled_at IS NOT NULL",
                [id],
            )?
        };
        Ok(changed > 0)
    }

    /// Every completed, not cancelled trade, oldest first.
    pub fn completed_trades(&self) -> rusqlite::Result<Vec<TradeRequest>> {
        let mut stmt = self.conn.prepare(
            "SELECT timestamp, side, status, failure_reason, shop_id, shop_name,
                    kiosk_id, resource_guid, scu, total_price, location, ship_id
             FROM trades WHERE status = 'confirmed' AND cancelled_at IS NULL
             ORDER BY timestamp, id",
        )?;
        let rows = stmt.query_map([], |row| {
            outcome_from_row(row).map(|outcome| match outcome {
                Outcome::Confirmed(t) | Outcome::Failed(t, _) => t,
            })
        })?;
        rows.collect()
    }

    /// Returns the new write-off's id.
    pub fn add_write_off(
        &self,
        timestamp: DateTime<Utc>,
        resource_guid: &str,
        scu: f64,
        note: Option<&str>,
        ship_id: Option<&str>,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO write_offs (timestamp, resource_guid, scu, note, ship_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                timestamp.format(TIMESTAMP_FORMAT).to_string(),
                resource_guid,
                scu,
                note,
                ship_id
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Records that a ship destruction was handled. Returns `false` if it
    /// already had been.
    pub fn record_ship_loss(
        &self,
        timestamp: DateTime<Utc>,
        ship_id: &str,
        ship_name: &str,
    ) -> rusqlite::Result<bool> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO ship_losses (timestamp, ship_id, ship_name) VALUES (?1, ?2, ?3)",
            params![timestamp.format(TIMESTAMP_FORMAT).to_string(), ship_id, ship_name],
        )?;
        Ok(inserted > 0)
    }

    /// Every write-off, oldest first.
    pub fn write_offs(&self) -> rusqlite::Result<Vec<WriteOff>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, resource_guid, scu, note, ship_id
             FROM write_offs ORDER BY timestamp, id",
        )?;
        let rows = stmt.query_map([], |row| {
            let timestamp: String = row.get(1)?;
            Ok(WriteOff {
                id: row.get(0)?,
                timestamp: DateTime::parse_from_rfc3339(&timestamp)
                    .map_err(|e| invalid(1, e))?
                    .with_timezone(&Utc),
                resource_guid: row.get(2)?,
                scu: row.get(3)?,
                note: row.get(4)?,
                ship_id: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    /// Returns `false` if there was no write-off with that id.
    pub fn delete_write_off(&self, id: i64) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("DELETE FROM write_offs WHERE id = ?1", [id])? > 0)
    }

    /// Records a ship's type from the log.
    ///
    /// The first time a ship's type is known, if a named ship of the same
    /// type was destroyed earlier, the name passes to this ship: it's most
    /// likely the claimed replacement, which comes back with a new ID.
    /// Returns the name if that happened.
    pub fn save_ship_seen(
        &self,
        ship_id: &str,
        class: &str,
        seen_at: DateTime<Utc>,
    ) -> rusqlite::Result<Option<String>> {
        let seen = seen_at.format(TIMESTAMP_FORMAT).to_string();
        let (type_was_known, named): (bool, bool) = self
            .conn
            .query_row(
                "SELECT class IS NOT NULL, name IS NOT NULL FROM ships WHERE ship_id = ?1",
                [ship_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((false, false));

        self.conn.execute(
            "INSERT INTO ships (ship_id, class, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT (ship_id) DO UPDATE SET
                 class = excluded.class,
                 last_seen = MAX(COALESCE(last_seen, ''), excluded.last_seen)",
            params![ship_id, class, seen],
        )?;
        if type_was_known || named {
            return Ok(None);
        }

        let predecessor: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT s.ship_id, s.name FROM ships s
                 JOIN ship_losses l ON l.ship_id = s.ship_id
                 WHERE s.class = ?1 AND s.name IS NOT NULL AND s.renamed_to IS NULL
                   AND s.ship_id != ?2 AND l.timestamp <= ?3
                 ORDER BY l.timestamp DESC LIMIT 1",
                params![class, ship_id, seen],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((old_id, name)) = predecessor else {
            return Ok(None);
        };
        self.conn.execute("UPDATE ships SET name = ?1 WHERE ship_id = ?2", [&name, ship_id])?;
        self.conn.execute(
            "UPDATE ships SET renamed_to = ?1 WHERE ship_id = ?2",
            [ship_id, &old_id],
        )?;
        Ok(Some(name))
    }

    pub fn save_ship_name(&self, ship_id: &str, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO ships (ship_id, name) VALUES (?1, ?2)
             ON CONFLICT (ship_id) DO UPDATE SET name = excluded.name",
            [ship_id, name],
        )?;
        Ok(())
    }

    /// Every ship the log has named or a trade has used, most recently seen first.
    pub fn ships(&self) -> rusqlite::Result<Vec<Ship>> {
        // Last seen is the later of the log naming the ship and its last trade.
        let mut stmt = self.conn.prepare(
            "SELECT ship_id, class, name, NULLIF(MAX(named, traded), '') AS last_seen, trades,
                    destroyed_at
             FROM (
                SELECT ids.ship_id, s.class, s.name,
                       COALESCE(s.last_seen, '') AS named,
                       COALESCE((SELECT MAX(t.timestamp) FROM trades t
                                 WHERE t.ship_id = ids.ship_id), '') AS traded,
                       (SELECT COUNT(*) FROM trades t
                        WHERE t.ship_id = ids.ship_id AND t.status = 'confirmed'
                          AND t.cancelled_at IS NULL) AS trades,
                       (SELECT MAX(l.timestamp) FROM ship_losses l
                        WHERE l.ship_id = ids.ship_id) AS destroyed_at
                FROM (SELECT ship_id FROM ships
                      UNION SELECT ship_id FROM trades WHERE ship_id IS NOT NULL) ids
                LEFT JOIN ships s ON s.ship_id = ids.ship_id
             )
             ORDER BY last_seen DESC NULLS LAST, ship_id",
        )?;
        let rows = stmt.query_map([], ship_from_row)?;
        rows.collect()
    }

    /// Total SCU of each commodity bought onto and sold from a ship, as
    /// (commodity GUID, SCU bought, SCU sold), most traded first.
    pub fn ship_cargo_traded(&self, ship_id: &str) -> rusqlite::Result<Vec<(String, f64, f64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT resource_guid,
                    COALESCE(SUM(CASE WHEN side = 'buy'  THEN scu END), 0),
                    COALESCE(SUM(CASE WHEN side = 'sell' THEN scu END), 0)
             FROM trades
             WHERE ship_id = ?1 AND status = 'confirmed' AND cancelled_at IS NULL
             GROUP BY resource_guid
             ORDER BY SUM(scu) DESC, resource_guid",
        )?;
        let rows = stmt.query_map([ship_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect()
    }

    pub fn ship(&self, ship_id: &str) -> rusqlite::Result<Option<Ship>> {
        Ok(self.ships()?.into_iter().find(|ship| ship.ship_id == ship_id))
    }

    pub fn location_name(&self, code: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT name FROM location_names WHERE code = ?1", [code], |row| row.get(0))
            .optional()
    }

    pub fn save_location_name(&self, code: &str, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO location_names (code, name) VALUES (?1, ?2)
             ON CONFLICT (code) DO UPDATE SET name = excluded.name",
            [code, name],
        )?;
        Ok(())
    }

    /// Every location code seen on a trade, with how many trades there
    /// completed and how many failed in the game.
    pub fn trade_locations(&self) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT location, SUM(status != 'failed'), SUM(status = 'failed') FROM trades
             WHERE location IS NOT NULL
             GROUP BY location ORDER BY location",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect()
    }

    pub fn commodity_name(&self, guid: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT name FROM commodities WHERE guid = ?1", [guid], |row| row.get(0))
            .optional()
    }

    pub fn save_commodity_name(&self, guid: &str, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO commodities (guid, name) VALUES (?1, ?2)
             ON CONFLICT (guid) DO UPDATE SET name = excluded.name",
            [guid, name],
        )?;
        Ok(())
    }

    pub fn totals(&self) -> rusqlite::Result<Totals> {
        self.conn.query_row(
            "SELECT
                COALESCE(SUM(CASE WHEN counted AND side = 'buy'  THEN total_price END), 0),
                COALESCE(SUM(CASE WHEN counted AND side = 'sell' THEN total_price END), 0),
                COALESCE(SUM(counted), 0),
                COALESCE(SUM(status = 'failed'), 0),
                COALESCE(SUM(status = 'confirmed' AND cancelled_at IS NOT NULL), 0)
             FROM (SELECT *, status = 'confirmed' AND cancelled_at IS NULL AS counted FROM trades)",
            [],
            |row| {
                Ok(Totals {
                    spent: row.get(0)?,
                    earned: row.get(1)?,
                    confirmed: row.get(2)?,
                    failed: row.get(3)?,
                    cancelled: row.get(4)?,
                })
            },
        )
    }
}

fn stored_trade_from_row(row: &Row) -> rusqlite::Result<StoredTrade> {
    let cancelled_at: Option<String> = row.get(13)?;
    Ok(StoredTrade {
        outcome: outcome_from_row(row)?,
        id: row.get(12)?,
        cancelled: cancelled_at.is_some(),
    })
}

fn outcome_from_row(row: &Row) -> rusqlite::Result<Outcome> {
    let timestamp: String = row.get(0)?;
    let side: String = row.get(1)?;
    let status: String = row.get(2)?;
    let reason: Option<String> = row.get(3)?;

    let trade = TradeRequest {
        timestamp: DateTime::parse_from_rfc3339(&timestamp)
            .map_err(|e| invalid(0, e))?
            .with_timezone(&Utc),
        side: side_from_str(&side).ok_or_else(|| invalid(1, format!("unknown side {side}")))?,
        shop_id: row.get(4)?,
        shop_name: row.get(5)?,
        kiosk_id: row.get(6)?,
        resource_guid: row.get(7)?,
        scu: row.get(8)?,
        total_price: row.get(9)?,
        location: row.get(10)?,
        ship_id: row.get(11)?,
    };
    Ok(match status.as_str() {
        "failed" => Outcome::Failed(trade, reason.unwrap_or_default()),
        _ => Outcome::Confirmed(trade),
    })
}

fn ship_from_row(row: &Row) -> rusqlite::Result<Ship> {
    let time = |column: usize| -> rusqlite::Result<Option<DateTime<Utc>>> {
        let text: Option<String> = row.get(column)?;
        text.map(|text| {
            DateTime::parse_from_rfc3339(&text)
                .map(|t| t.with_timezone(&Utc))
                .map_err(|e| invalid(column, e))
        })
        .transpose()
    };
    let last_seen = time(3)?;
    // Seen again after "destroyed" means the detection was wrong.
    let destroyed_at = time(5)?.filter(|destroyed| last_seen.is_none_or(|seen| seen <= *destroyed));
    Ok(Ship {
        ship_id: row.get(0)?,
        class: row.get(1)?,
        name: row.get(2)?,
        last_seen,
        trades: row.get(4)?,
        destroyed_at,
    })
}

/// Adds a nullable TEXT column to databases created before it existed.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<()> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |row| row.get(0),
    )?;
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} TEXT"), [])?;
    }
    Ok(())
}

fn invalid(column: usize, err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, err.into())
}

fn side_to_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn side_from_str(s: &str) -> Option<Side> {
    match s {
        "buy" => Some(Side::Buy),
        "sell" => Some(Side::Sell),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(side: Side, timestamp: &str, total_price: f64) -> TradeRequest {
        TradeRequest {
            timestamp: DateTime::parse_from_rfc3339(timestamp).unwrap().with_timezone(&Utc),
            side,
            shop_id: "1".into(),
            shop_name: "SCShop_Test".into(),
            kiosk_id: "2".into(),
            resource_guid: "98aa6a70-80e7-41d7-9128-9692053d57a2".into(),
            scu: 120.0,
            total_price,
            location: None,
            ship_id: None,
        }
    }

    #[test]
    fn tracks_ships_from_log_trades_and_user() {
        let db = Db::open_in_memory().unwrap();
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);

        db.save_ship_seen("869668647419", "ANVL_Asgard", at("2026-10-12T02:13:47.985Z")).unwrap();
        // An older sighting doesn't move last_seen backwards.
        db.save_ship_seen("869668647419", "ANVL_Asgard", at("2026-10-11T02:48:59.020Z")).unwrap();
        db.save_ship_name("869668647419", "Big Bertha").unwrap();

        // A ship only known from a trade still shows up.
        let mut buy = trade(Side::Buy, "2026-10-04T06:20:30.252Z", 1036801.0);
        buy.ship_id = Some("856218865910".into());
        db.record(&Outcome::Confirmed(buy)).unwrap();

        let ships = db.ships().unwrap();
        assert_eq!(ships.len(), 2);
        assert_eq!(ships[0].ship_id, "869668647419");
        assert_eq!(ships[0].class.as_deref(), Some("ANVL_Asgard"));
        assert_eq!(ships[0].name.as_deref(), Some("Big Bertha"));
        assert_eq!(ships[0].last_seen, Some(at("2026-10-12T02:13:47.985Z")));
        assert_eq!(ships[1].ship_id, "856218865910");
        assert_eq!(ships[1].class, None);
        assert_eq!(ships[1].trades, 1);
        // Only known from a trade, so last seen is that trade.
        assert_eq!(ships[1].last_seen, Some(at("2026-10-04T06:20:30.252Z")));
    }

    #[test]
    fn claimed_replacement_takes_the_destroyed_ships_name() {
        let db = Db::open_in_memory().unwrap();
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let ship = |id: &str| db.ships().unwrap().into_iter().find(|s| s.ship_id == id).unwrap();

        // An Asgard you've had all along, and a named one that gets destroyed.
        db.save_ship_seen("other", "ANVL_Asgard", at("2026-10-11T01:00:00.000Z")).unwrap();
        db.save_ship_seen("old", "ANVL_Asgard", at("2026-10-11T02:00:00.000Z")).unwrap();
        db.save_ship_name("old", "Big Bertha").unwrap();
        db.record_ship_loss(at("2026-10-12T02:13:47.985Z"), "old", "ANVL_Asgard").unwrap();
        assert!(ship("old").destroyed_at.is_some());
        assert!(ship("other").destroyed_at.is_none());

        // Seeing the existing Asgard again doesn't rename it.
        assert_eq!(db.save_ship_seen("other", "ANVL_Asgard", at("2026-10-12T03:00:00.000Z")).unwrap(), None);
        // A different type doesn't take the name.
        assert_eq!(db.save_ship_seen("drake", "DRAK_Cutlass_Black", at("2026-10-12T03:00:00.000Z")).unwrap(), None);
        // The first new Asgard does, but only once.
        assert_eq!(
            db.save_ship_seen("new", "ANVL_Asgard", at("2026-10-13T01:51:37.827Z")).unwrap().as_deref(),
            Some("Big Bertha")
        );
        assert_eq!(ship("new").name.as_deref(), Some("Big Bertha"));
        assert_eq!(db.save_ship_seen("newer", "ANVL_Asgard", at("2026-10-13T02:00:00.000Z")).unwrap(), None);
        assert_eq!(ship("newer").name, None);
    }

    #[test]
    fn ship_seen_after_destruction_is_not_destroyed() {
        let db = Db::open_in_memory().unwrap();
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        db.record_ship_loss(at("2026-10-12T02:13:47.985Z"), "id", "ANVL_Asgard").unwrap();
        db.save_ship_seen("id", "ANVL_Asgard", at("2026-10-12T02:13:47.985Z")).unwrap();
        assert!(db.ships().unwrap()[0].destroyed_at.is_some());
        db.save_ship_seen("id", "ANVL_Asgard", at("2026-10-12T04:00:00.000Z")).unwrap();
        assert!(db.ships().unwrap()[0].destroyed_at.is_none());
    }

    #[test]
    fn sums_cargo_traded_per_ship() {
        let db = Db::open_in_memory().unwrap();
        let on_ship = |side, ts, scu| {
            let mut t = trade(side, ts, 100.0);
            t.ship_id = Some("856218865910".into());
            t.scu = scu;
            Outcome::Confirmed(t)
        };
        db.record(&on_ship(Side::Buy, "2026-10-04T06:20:30.252Z", 120.0)).unwrap();
        db.record(&on_ship(Side::Buy, "2026-10-04T06:21:30.252Z", 30.0)).unwrap();
        let sell = db.record(&on_ship(Side::Sell, "2026-10-04T06:43:49.495Z", 120.0)).unwrap().unwrap();
        db.record(&on_ship(Side::Sell, "2026-10-04T06:44:49.495Z", 30.0)).unwrap();
        db.set_cancelled(sell, true).unwrap();

        assert_eq!(
            db.ship_cargo_traded("856218865910").unwrap(),
            vec![("98aa6a70-80e7-41d7-9128-9692053d57a2".to_string(), 150.0, 30.0)]
        );
        assert!(db.ship_cargo_traded("someone-else").unwrap().is_empty());
    }

    #[test]
    fn handles_each_ship_loss_once() {
        let db = Db::open_in_memory().unwrap();
        let at = DateTime::parse_from_rfc3339("2026-10-12T02:13:47.985Z").unwrap().with_timezone(&Utc);
        assert!(db.record_ship_loss(at, "869668647419", "ANVL Asgard").unwrap());
        assert!(!db.record_ship_loss(at, "869668647419", "ANVL Asgard").unwrap());
    }

    #[test]
    fn stores_and_deletes_write_offs() {
        let db = Db::open_in_memory().unwrap();
        let at = DateTime::parse_from_rfc3339("2026-10-12T02:00:00.000Z").unwrap().with_timezone(&Utc);
        let id = db
            .add_write_off(at, "7bbd3197", 12.5, Some("ship destroyed"), Some("869668647419"))
            .unwrap();

        let stored = db.write_offs().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, id);
        assert_eq!(stored[0].timestamp, at);
        assert_eq!(stored[0].scu, 12.5);
        assert_eq!(stored[0].note.as_deref(), Some("ship destroyed"));
        assert_eq!(stored[0].ship_id.as_deref(), Some("869668647419"));

        assert!(db.delete_write_off(id).unwrap());
        assert!(!db.delete_write_off(id).unwrap());
        assert!(db.write_offs().unwrap().is_empty());
    }

    #[test]
    fn backfills_missing_location() {
        let db = Db::open_in_memory().unwrap();
        let mut sell = trade(Side::Sell, "2026-10-04T06:43:49.495Z", 1470960.0);
        db.record(&Outcome::Confirmed(sell.clone())).unwrap();

        sell.location = Some("RR_JP_NyxPyro".into());
        assert_eq!(db.record(&Outcome::Confirmed(sell.clone())).unwrap(), None);
        assert_eq!(db.recent(1, true).unwrap()[0].outcome, Outcome::Confirmed(sell));
        assert_eq!(db.trade_locations().unwrap(), vec![("RR_JP_NyxPyro".to_string(), 1, 0)]);
    }

    #[test]
    fn counts_failed_trades_per_location_separately() {
        let db = Db::open_in_memory().unwrap();
        let at = |ts: &str| {
            let mut t = trade(Side::Buy, ts, 1.0);
            t.location = Some("RR_ARC_LEO".into());
            t
        };
        db.record(&Outcome::Confirmed(at("2026-10-11T04:20:00.000Z"))).unwrap();
        for ts in ["2026-10-11T04:24:10.855Z", "2026-10-11T04:25:50.807Z"] {
            db.record(&Outcome::Failed(at(ts), "CargoCreationFailed".into())).unwrap();
        }
        assert_eq!(db.trade_locations().unwrap(), vec![("RR_ARC_LEO".to_string(), 1, 2)]);
    }

    #[test]
    fn adds_location_column_to_old_databases() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE trades (
                id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL, side TEXT NOT NULL,
                status TEXT NOT NULL, failure_reason TEXT, shop_id TEXT NOT NULL,
                shop_name TEXT NOT NULL, kiosk_id TEXT NOT NULL, resource_guid TEXT NOT NULL,
                scu REAL NOT NULL, total_price REAL NOT NULL,
                UNIQUE (timestamp, side, kiosk_id, resource_guid))",
        )
        .unwrap();
        let db = Db::init(conn).unwrap();
        let mut buy = trade(Side::Buy, "2026-10-04T06:20:30.252Z", 1036801.0);
        buy.location = Some("Nyx_Levski".into());
        assert!(db.record(&Outcome::Confirmed(buy)).unwrap().is_some());
    }

    #[test]
    fn ignores_duplicates() {
        let db = Db::open_in_memory().unwrap();
        let buy = Outcome::Confirmed(trade(Side::Buy, "2026-10-04T06:20:30.252Z", 1036801.0));
        let id = db.record(&buy).unwrap().expect("new trade");
        assert_eq!(db.record(&buy).unwrap(), None);
        assert_eq!(
            db.recent(10, true).unwrap(),
            vec![StoredTrade { id, outcome: buy, cancelled: false }]
        );
    }

    #[test]
    fn stores_settings() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.setting("uex_sync").unwrap(), None);
        db.set_setting("uex_sync", Some("on")).unwrap();
        db.set_setting("uex_sync", Some("off")).unwrap();
        assert_eq!(db.setting("uex_sync").unwrap().as_deref(), Some("off"));
        db.set_setting("uex_sync", None).unwrap();
        assert_eq!(db.setting("uex_sync").unwrap(), None);
    }

    #[test]
    fn recent_can_leave_out_failed_trades() {
        let db = Db::open_in_memory().unwrap();
        db.record(&Outcome::Confirmed(trade(Side::Buy, "2026-10-04T06:20:30.252Z", 1.0))).unwrap();
        db.record(&Outcome::Failed(
            trade(Side::Buy, "2026-10-11T04:24:10.855Z", 2.0),
            "CargoCreationFailed".into(),
        ))
        .unwrap();
        db.record(&Outcome::Failed(
            trade(Side::Buy, "2026-10-11T04:25:10.855Z", 3.0),
            "CargoCreationFailed".into(),
        ))
        .unwrap();

        assert_eq!(db.recent(10, true).unwrap().len(), 3);
        // The limit counts only trades shown, so older completed ones aren't pushed out.
        let shown = db.recent(1, false).unwrap();
        assert_eq!(shown.len(), 1);
        assert!(matches!(shown[0].outcome, Outcome::Confirmed(_)));
    }

    #[test]
    fn cancelled_trades_are_kept_but_not_counted() {
        let db = Db::open_in_memory().unwrap();
        let buy = Outcome::Confirmed(trade(Side::Buy, "2026-10-04T06:20:30.252Z", 1036801.0));
        let sell = Outcome::Confirmed(trade(Side::Sell, "2026-10-04T06:43:49.495Z", 1470960.0));
        db.record(&buy).unwrap();
        let sell_id = db.record(&sell).unwrap().unwrap();

        assert!(db.set_cancelled(sell_id, true).unwrap());
        assert!(!db.set_cancelled(sell_id, true).unwrap());
        assert!(db.trade(sell_id).unwrap().unwrap().cancelled);
        assert_eq!(db.completed_trades().unwrap().len(), 1);
        let totals = db.totals().unwrap();
        assert_eq!((totals.earned, totals.confirmed, totals.cancelled), (0.0, 1, 1));

        // Re-reading the log doesn't bring it back.
        assert_eq!(db.record(&sell).unwrap(), None);
        assert_eq!(db.completed_trades().unwrap().len(), 1);

        assert!(db.set_cancelled(sell_id, false).unwrap());
        assert_eq!(db.completed_trades().unwrap().len(), 2);
    }

    #[test]
    fn totals_skip_failed_trades() {
        let db = Db::open_in_memory().unwrap();
        db.record(&Outcome::Confirmed(trade(Side::Buy, "2026-10-04T06:20:30.252Z", 1036801.0))).unwrap();
        db.record(&Outcome::Confirmed(trade(Side::Sell, "2026-10-04T06:43:49.495Z", 1470960.0))).unwrap();
        db.record(&Outcome::Failed(
            trade(Side::Buy, "2026-10-11T04:24:10.855Z", 251329.0),
            "CargoCreationFailed".into(),
        ))
        .unwrap();

        assert_eq!(
            db.totals().unwrap(),
            Totals { spent: 1036801.0, earned: 1470960.0, confirmed: 2, failed: 1, cancelled: 0 }
        );
    }

    #[test]
    fn stores_commodity_names() {
        let db = Db::open_in_memory().unwrap();
        let guid = "98aa6a70-80e7-41d7-9128-9692053d57a2";
        assert_eq!(db.commodity_name(guid).unwrap(), None);
        db.save_commodity_name(guid, "Construction Materials").unwrap();
        assert_eq!(db.commodity_name(guid).unwrap().as_deref(), Some("Construction Materials"));
    }

    #[test]
    fn recent_returns_latest_oldest_first() {
        let db = Db::open_in_memory().unwrap();
        for (i, ts) in ["2026-10-01T00:00:00.000Z", "2026-10-02T00:00:00.000Z", "2026-10-03T00:00:00.000Z"]
            .iter()
            .enumerate()
        {
            db.record(&Outcome::Confirmed(trade(Side::Buy, ts, i as f64))).unwrap();
        }
        let prices: Vec<f64> = db
            .recent(2, true)
            .unwrap()
            .iter()
            .map(|stored| match &stored.outcome {
                Outcome::Confirmed(t) | Outcome::Failed(t, _) => t.total_price,
            })
            .collect();
        assert_eq!(prices, vec![1.0, 2.0]);
    }
}
