//! Market data from UEX (uexcorp.space), the community-run Star Citizen
//! trading database: what terminals pay and charge, and trade routes.
//!
//! Data is cached in the trade database, so tracking and the prompt share
//! it and it survives restarts. Prices refresh every 15 minutes, terminals
//! and ship sizes daily. If UEX can't be reached, cached data is used (its
//! age is always shown) and retries wait a few minutes, so being offline
//! never slows the tracker down.
//!
//! UEX doesn't use the game's own codes, so the terminal for a trade is
//! found by name: the location's name plus the shop type (Admin, Platinum
//! Bay...). `link-terminal` sets it by hand when that doesn't work.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::db::Db;
use crate::parser::{Side, TradeRequest};
use crate::{format, locations, ships};

pub const SCHEMA: &str = "
-- When each kind of UEX data was last downloaded (Unix seconds).
CREATE TABLE IF NOT EXISTS uex_meta (
    key        TEXT PRIMARY KEY,
    fetched_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS uex_terminals (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,          -- e.g. Admin - Port Tressler
    displayname TEXT,
    nickname    TEXT,
    station     TEXT,
    city        TEXT,
    outpost     TEXT,
    system      TEXT
);
CREATE TABLE IF NOT EXISTS uex_prices (
    id_terminal    INTEGER NOT NULL,
    terminal       TEXT NOT NULL,
    system         TEXT,
    commodity      TEXT NOT NULL,
    id_commodity   INTEGER,            -- UEX's ID, for logging trades to UEX
    price_buy      REAL,               -- per SCU, what you pay; 0 if not sold there
    price_sell     REAL,               -- per SCU, what you get; 0 if not bought there
    scu_buy        REAL,               -- stock available to buy
    updated        INTEGER,            -- when UEX last had a report (Unix seconds)
    PRIMARY KEY (id_terminal, commodity)
);
CREATE TABLE IF NOT EXISTS uex_vehicles (
    name TEXT PRIMARY KEY,             -- lowercase full name, e.g. anvil asgard
    scu  REAL
);
-- Terminals the user linked to a location code by hand.
CREATE TABLE IF NOT EXISTS uex_links (
    location_code TEXT PRIMARY KEY,
    id_terminal   INTEGER NOT NULL
);
";

/// Creates the cache tables. A price cache from before commodity IDs were
/// kept is dropped and downloaded again; it's only a cache.
pub fn create_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    let has_id: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('uex_prices') WHERE name = 'id_commodity'",
        [],
        |row| row.get(0),
    )?;
    if !has_id {
        conn.execute_batch("DROP TABLE uex_prices; DELETE FROM uex_meta WHERE key = 'prices';")?;
        conn.execute_batch(SCHEMA)?;
    }
    Ok(())
}

pub const DEFAULT_URL: &str = "https://api.uexcorp.uk/2.0";
const TIMEOUT: Duration = Duration::from_secs(15);
const PRICES_MAX_AGE_SECS: i64 = 15 * 60;
const REFERENCE_MAX_AGE_SECS: i64 = 24 * 60 * 60;
/// After a failed download, don't try again for this long.
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
/// Prices older than this are too stale to give advice on.
const STALE_AFTER_DAYS: i64 = 30;

static LAST_FAILURE: Mutex<Option<Instant>> = Mutex::new(None);

/// A terminal that trades commodities.
#[derive(Debug, Clone, PartialEq)]
pub struct Terminal {
    pub id: i64,
    /// e.g. `Admin - Port Tressler`
    pub name: String,
    /// e.g. `Port Tressler`, `TDD Area 18`; fits in tables.
    pub short_name: String,
    pub system: String,
    /// The station, city or outpost it's at, e.g. `Port Tressler`.
    pub place: String,
    /// Every name it goes by, lowercase, for matching.
    names: Vec<String>,
}

/// What one terminal charges or pays for one commodity.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketRow {
    pub id_terminal: i64,
    pub terminal: String,
    pub system: String,
    pub price_buy: f64,
    pub price_sell: f64,
    pub scu_buy: f64,
    pub updated: DateTime<Utc>,
}

/// The best price known for selling a commodity.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownPrice {
    pub id_terminal: i64,
    pub terminal: String,
    pub system: String,
    pub price: f64,
    pub updated: DateTime<Utc>,
}

/// Market context for a trade that just happened.
#[derive(Debug, Clone, PartialEq)]
pub enum MarketNote {
    /// After a sale: how the price compares with the best known nearby.
    SaleCompared {
        /// What you got per SCU.
        got: f64,
        scu: f64,
        best: Option<KnownPrice>,
        best_is_here: bool,
    },
    /// After a purchase: where it sells best.
    SellAdvice {
        /// What you paid per SCU.
        paid: f64,
        scu: f64,
        best: Option<KnownPrice>,
    },
}

/// Whether a market note is good news, for colouring it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tone {
    Good,
    Bad,
    Neutral,
}

impl MarketNote {
    /// One line for the console and overlay.
    pub fn describe(&self) -> (String, Tone) {
        let per_scu = |p: f64| format!("{}/SCU", format::auec(p));
        match self {
            MarketNote::SaleCompared { best: None, .. } => {
                ("UEX: no other known buyers nearby".into(), Tone::Neutral)
            }
            MarketNote::SaleCompared { got, scu, best: Some(best), best_is_here } => {
                // Within half a percent counts as the best price.
                if *best_is_here || *got >= best.price * 0.995 {
                    (
                        format!("UEX: best known price in {} ({})", best.system, per_scu(best.price)),
                        Tone::Good,
                    )
                } else {
                    (
                        format!(
                            "UEX: {} pays {} ({}, {}); {} less for this load",
                            best.terminal,
                            per_scu(best.price),
                            best.system,
                            format::age(best.updated),
                            format::auec((best.price - got) * scu)
                        ),
                        Tone::Bad,
                    )
                }
            }
            MarketNote::SellAdvice { best: None, .. } => {
                ("UEX: no known buyers for this".into(), Tone::Neutral)
            }
            MarketNote::SellAdvice { paid, scu, best: Some(best) } => {
                let profit = (best.price - paid) * scu;
                if profit > 0.0 {
                    (
                        format!(
                            "UEX: sells best at {} for {} ({}, {}): about {} for this load",
                            best.terminal,
                            per_scu(best.price),
                            best.system,
                            format::age(best.updated),
                            format::signed_auec(profit)
                        ),
                        Tone::Good,
                    )
                } else {
                    (
                        format!(
                            "UEX: best known buyer is {} at {}, below the {} you paid",
                            best.terminal,
                            per_scu(best.price),
                            per_scu(*paid)
                        ),
                        Tone::Bad,
                    )
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub commodity: String,
    pub origin: String,
    pub destination: String,
    pub destination_system: String,
    /// Per SCU at the origin.
    pub buy: f64,
    /// Per SCU at the destination.
    pub sell: f64,
    /// SCU on offer at the origin.
    pub stock: f64,
    /// Gigametres.
    pub distance: f64,
}

// ---------------------------------------------------------------------------
// Downloading and caching

/// An app token baked in when building a release for others:
/// `STAR_CARGO_UEX_BUILD_TOKEN` at build time. Never in the source code.
const BUILT_IN_TOKEN: Option<&str> = option_env!("STAR_CARGO_UEX_BUILD_TOKEN");

/// Where the app token in use came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TokenSource {
    /// The `STAR_CARGO_UEX_TOKEN` environment variable.
    Environment,
    /// Saved with `uex token`.
    Saved,
    /// Built into this copy of the program.
    BuiltIn,
}

/// The UEX app token to send, if any: the environment variable, else one
/// saved with `uex token`, else the one built in.
pub fn app_token(db: &Db) -> Option<(String, TokenSource)> {
    let env = std::env::var("STAR_CARGO_UEX_TOKEN").ok();
    let saved = db.setting("uex_token").ok().flatten();
    pick_token(env, saved, BUILT_IN_TOKEN)
}

fn pick_token(
    env: Option<String>,
    saved: Option<String>,
    built_in: Option<&str>,
) -> Option<(String, TokenSource)> {
    let usable = |t: &String| !t.trim().is_empty();
    env.filter(usable)
        .map(|t| (t, TokenSource::Environment))
        .or_else(|| saved.filter(usable).map(|t| (t, TokenSource::Saved)))
        .or_else(|| built_in.map(str::to_string).filter(usable).map(|t| (t, TokenSource::BuiltIn)))
}

/// Sent with every request, so a token can be locked to versions of this
/// program with UEX's Client Version Lock.
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// UEX's answer when too many requests were made with a token.
pub fn is_rate_limit(why: &str) -> bool {
    why.contains("requests_limit_reached") || why.contains("429")
}

pub fn base_url() -> String {
    std::env::var("STAR_CARGO_UEX_URL")
        .unwrap_or_else(|_| DEFAULT_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// GETs a UEX endpoint and returns its `data` rows.
fn fetch(db: &Db, path: &str) -> Result<Vec<Value>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();
    let mut request = agent
        .get(format!("{}/{path}", base_url()))
        .header("Accept", "application/json")
        .header("X-Client-Version", CLIENT_VERSION);
    if let Some((token, _)) = app_token(db) {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request.call().map_err(|e| e.to_string())?;
    let body = response
        .body_mut()
        .with_config()
        .limit(32 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let json: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    if json["status"] != "ok" {
        return Err(format!("UEX replied {}", json["status"]));
    }
    Ok(json["data"].as_array().cloned().unwrap_or_default())
}

fn now_secs() -> i64 {
    Utc::now().timestamp()
}

/// Downloads `path` into the cache with `store` unless the cached copy is
/// younger than `max_age_secs`. Failures are reported once and leave the old
/// copy in place.
fn ensure(
    db: &Db,
    key: &str,
    path: &str,
    max_age_secs: i64,
    store: fn(&Connection, &[Value]) -> rusqlite::Result<()>,
) {
    let conn = db.conn();
    let fetched_at: Option<i64> = conn
        .query_row("SELECT fetched_at FROM uex_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()
        .unwrap_or(None);
    if fetched_at.is_some_and(|at| now_secs() - at < max_age_secs) {
        return;
    }
    if LAST_FAILURE
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < RETRY_AFTER)
    {
        return;
    }

    let rows = match fetch(db, path) {
        Ok(rows) => rows,
        Err(e) => {
            let mut last = LAST_FAILURE.lock().unwrap();
            if last.is_none() && is_rate_limit(&e) {
                eprintln!("UEX's request limit was reached; using saved market data. Retrying in a few minutes.");
            } else if last.is_none() {
                eprintln!("Cannot reach UEX ({e}); using saved market data. Retrying in a few minutes.");
            }
            *last = Some(Instant::now());
            return;
        }
    };
    *LAST_FAILURE.lock().unwrap() = None;

    let saved = (|| {
        let tx = conn.unchecked_transaction()?;
        store(&tx, &rows)?;
        tx.execute(
            "INSERT INTO uex_meta (key, fetched_at) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET fetched_at = excluded.fetched_at",
            params![key, now_secs()],
        )?;
        tx.commit()
    })();
    if let Err(e) = saved {
        eprintln!("Cannot save UEX data: {e}");
    }
}

fn number(row: &Value, key: &str) -> f64 {
    match &row[key] {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn text(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap_or("").trim().to_string()
}

fn store_prices(conn: &Connection, rows: &[Value]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM uex_prices", [])?;
    let mut insert = conn.prepare(
        "INSERT OR REPLACE INTO uex_prices
            (id_terminal, terminal, system, commodity, id_commodity, price_buy, price_sell, scu_buy, updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for row in rows {
        insert.execute(params![
            number(row, "id_terminal") as i64,
            text(row, "terminal_name"),
            text(row, "star_system_name"),
            text(row, "commodity_name"),
            number(row, "id_commodity") as i64,
            number(row, "price_buy"),
            number(row, "price_sell"),
            number(row, "scu_buy"),
            number(row, "date_modified") as i64,
        ])?;
    }
    Ok(())
}

fn store_terminals(conn: &Connection, rows: &[Value]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM uex_terminals", [])?;
    let mut insert = conn.prepare(
        "INSERT OR REPLACE INTO uex_terminals
            (id, name, displayname, nickname, station, city, outpost, system)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for row in rows {
        insert.execute(params![
            number(row, "id") as i64,
            text(row, "name"),
            text(row, "displayname"),
            text(row, "nickname"),
            text(row, "space_station_name"),
            text(row, "city_name"),
            text(row, "outpost_name"),
            text(row, "star_system_name"),
        ])?;
    }
    Ok(())
}

fn store_vehicles(conn: &Connection, rows: &[Value]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM uex_vehicles", [])?;
    let mut insert = conn.prepare("INSERT OR REPLACE INTO uex_vehicles (name, scu) VALUES (?1, ?2)")?;
    for row in rows {
        insert.execute(params![text(row, "name_full").to_lowercase(), number(row, "scu")])?;
    }
    Ok(())
}

fn ensure_prices(db: &Db) {
    ensure(db, "prices", "commodities_prices_all", PRICES_MAX_AGE_SECS, store_prices);
}

fn ensure_terminals(db: &Db) {
    ensure(db, "terminals", "terminals?type=commodity", REFERENCE_MAX_AGE_SECS, store_terminals);
}

fn ensure_vehicles(db: &Db) {
    ensure(db, "vehicles", "vehicles", REFERENCE_MAX_AGE_SECS, store_vehicles);
}

// ---------------------------------------------------------------------------
// Prices

fn from_unix(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).unwrap_or_default()
}

/// Every terminal's prices for a commodity (exact name, any case).
pub fn market(db: &Db, commodity: &str) -> Vec<MarketRow> {
    ensure_prices(db);
    // The price download only has a short terminal name and no system;
    // the terminal list has both.
    ensure_terminals(db);
    let query = || -> rusqlite::Result<Vec<MarketRow>> {
        // The price download's short names ("TDD Area 18") fit tables better.
        let mut stmt = db.conn().prepare(
            "SELECT p.id_terminal, COALESCE(NULLIF(p.terminal, ''), t.name),
                    COALESCE(NULLIF(t.system, ''), p.system),
                    p.price_buy, p.price_sell, p.scu_buy, p.updated
             FROM uex_prices p LEFT JOIN uex_terminals t ON t.id = p.id_terminal
             WHERE lower(p.commodity) = lower(?1)",
        )?;
        let rows = stmt.query_map([commodity], |row| {
            Ok(MarketRow {
                id_terminal: row.get(0)?,
                terminal: row.get(1)?,
                system: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                price_buy: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                price_sell: row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                scu_buy: row.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                updated: from_unix(row.get::<_, Option<i64>>(6)?.unwrap_or(0)),
            })
        })?;
        rows.collect()
    };
    query().unwrap_or_else(|e| {
        eprintln!("Cannot read UEX prices: {e}");
        Vec::new()
    })
}

/// UEX's ID for a commodity (exact name, any case).
pub fn commodity_id(db: &Db, commodity: &str) -> Option<i64> {
    ensure_prices(db);
    db.conn()
        .query_row(
            "SELECT id_commodity FROM uex_prices
             WHERE lower(commodity) = lower(?1) AND id_commodity > 0 LIMIT 1",
            [commodity],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None)
}

/// Every commodity UEX has prices for, for `prices` and completion.
pub fn commodity_names(db: &Db) -> Vec<String> {
    ensure_prices(db);
    let query = || -> rusqlite::Result<Vec<String>> {
        let mut stmt = db
            .conn()
            .prepare("SELECT DISTINCT commodity FROM uex_prices ORDER BY commodity")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect()
    };
    query().unwrap_or_default()
}

fn is_recent(updated: DateTime<Utc>) -> bool {
    Utc::now() - updated < chrono::TimeDelta::days(STALE_AFTER_DAYS)
}

/// The best known place to sell a commodity, in `prefer_system` if anywhere
/// there buys it, otherwise anywhere.
pub fn best_sell(db: &Db, commodity: &str, prefer_system: Option<&str>) -> Option<KnownPrice> {
    let buyers: Vec<MarketRow> = market(db, commodity)
        .into_iter()
        .filter(|row| row.price_sell > 0.0 && is_recent(row.updated))
        .collect();
    let in_system: Vec<&MarketRow> = buyers
        .iter()
        .filter(|row| prefer_system.is_some_and(|s| row.system.eq_ignore_ascii_case(s)))
        .collect();
    let candidates: Vec<&MarketRow> = if in_system.is_empty() {
        buyers.iter().collect()
    } else {
        in_system
    };
    candidates
        .into_iter()
        .max_by(|a, b| a.price_sell.total_cmp(&b.price_sell))
        .map(|row| KnownPrice {
            id_terminal: row.id_terminal,
            terminal: row.terminal.clone(),
            system: row.system.clone(),
            price: row.price_sell,
            updated: row.updated,
        })
}

/// Market context for a completed trade, or `None` if UEX knows nothing
/// about the commodity (or can't be reached and nothing is saved).
pub fn market_note(db: &Db, trade: &TradeRequest, commodity: &str) -> Option<MarketNote> {
    if trade.scu <= 0.0 || market(db, commodity).is_empty() {
        return None;
    }
    let here = terminal_for(db, trade);
    let system = here.as_ref().map(|t| t.system.as_str());
    let best = best_sell(db, commodity, system);
    let per_scu = trade.total_price / trade.scu;
    Some(match trade.side {
        Side::Sell => MarketNote::SaleCompared {
            got: per_scu,
            scu: trade.scu,
            best_is_here: matches!((&best, &here), (Some(b), Some(h)) if b.id_terminal == h.id),
            best,
        },
        Side::Buy => MarketNote::SellAdvice {
            paid: per_scu,
            scu: trade.scu,
            best,
        },
    })
}

// ---------------------------------------------------------------------------
// Terminals

/// Every commodity terminal UEX knows.
pub fn terminals(db: &Db) -> Vec<Terminal> {
    ensure_terminals(db);
    let query = || -> rusqlite::Result<Vec<Terminal>> {
        let mut stmt = db.conn().prepare(
            "SELECT id, name, displayname, nickname, station, city, outpost, system
             FROM uex_terminals ORDER BY system, name",
        )?;
        let rows = stmt.query_map([], |row| {
            let get = |i: usize| -> rusqlite::Result<String> {
                Ok(row.get::<_, Option<String>>(i)?.unwrap_or_default())
            };
            let (name, displayname, nickname) = (get(1)?, get(2)?, get(3)?);
            let (station, city, outpost) = (get(4)?, get(5)?, get(6)?);
            let place = [&station, &city, &outpost, &displayname]
                .into_iter()
                .find(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| name.clone());
            let names = [&name, &displayname, &nickname, &station, &city, &outpost]
                .into_iter()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_lowercase())
                .collect();
            Ok(Terminal {
                id: row.get(0)?,
                short_name: if nickname.is_empty() { name.clone() } else { nickname.clone() },
                name,
                system: get(7)?,
                place,
                names,
            })
        })?;
        rows.collect()
    };
    query().unwrap_or_else(|e| {
        eprintln!("Cannot read UEX terminals: {e}");
        Vec::new()
    })
}

/// The terminals at a location code: the one linked by hand, or those whose
/// station, city or outpost has the location's name.
pub fn terminals_at(db: &Db, location_code: &str) -> Vec<Terminal> {
    let all = terminals(db);
    if let Some(id) = linked_terminal(db, location_code) {
        return all.into_iter().filter(|t| t.id == id).collect();
    }
    let place = locations::display_name(db, location_code).to_lowercase();
    all.into_iter()
        .filter(|t| {
            t.names
                .iter()
                .any(|n| *n == place || n.starts_with(&format!("{place} ")))
        })
        .collect()
}

/// The UEX terminal a trade happened at, if it can be worked out.
pub fn terminal_for(db: &Db, trade: &TradeRequest) -> Option<Terminal> {
    let candidates = terminals_at(db, trade.location.as_deref()?);
    pick_by_shop(candidates, &trade.shop_name)
}

/// Several terminals can share a station (Admin, Platinum Bay...); the
/// game's shop name says which kind it was.
fn pick_by_shop(candidates: Vec<Terminal>, shop_name: &str) -> Option<Terminal> {
    if candidates.len() <= 1 {
        return candidates.into_iter().next();
    }
    let shop = shop_name.to_lowercase();
    let kind = ["admin", "platinum", "tdd"].into_iter().find(|k| shop.contains(k));
    let starts = |t: &Terminal, prefix: &str| t.name.to_lowercase().starts_with(prefix);
    kind.and_then(|k| candidates.iter().find(|t| starts(t, k)).cloned())
        .or_else(|| candidates.iter().find(|t| starts(t, "admin")).cloned())
        .or_else(|| candidates.into_iter().next())
}

/// Commodity terminals whose name contains `text` (any case).
pub fn search_terminals(db: &Db, text: &str) -> Vec<Terminal> {
    let text = text.to_lowercase();
    terminals(db)
        .into_iter()
        .filter(|t| t.names.iter().any(|n| n.contains(&text)) || t.system.to_lowercase() == text)
        .collect()
}

fn linked_terminal(db: &Db, location_code: &str) -> Option<i64> {
    db.conn()
        .query_row(
            "SELECT id_terminal FROM uex_links WHERE location_code = ?1",
            [location_code],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None)
}

/// Links a location code to a terminal by hand. Returns the terminal, or
/// `None` if UEX has no commodity terminal with that ID.
pub fn link_terminal(db: &Db, location_code: &str, id: i64) -> rusqlite::Result<Option<Terminal>> {
    let Some(terminal) = terminals(db).into_iter().find(|t| t.id == id) else {
        return Ok(None);
    };
    db.conn().execute(
        "INSERT INTO uex_links (location_code, id_terminal) VALUES (?1, ?2)
         ON CONFLICT (location_code) DO UPDATE SET id_terminal = excluded.id_terminal",
        params![location_code, id],
    )?;
    Ok(Some(terminal))
}

// ---------------------------------------------------------------------------
// Ships and routes

/// Cargo capacity of a ship type, e.g. `ANVL_Asgard` -> 180, if UEX lists it.
pub fn capacity(db: &Db, class: &str) -> Option<f64> {
    ensure_vehicles(db);
    let name = ships::class_name(class).to_lowercase();
    // UEX writes "Mk II" where the game writes "Mk2".
    let roman = name.replace(" mk2", " mk ii").replace(" mk1", " mk i");
    [name, roman].iter().find_map(|n| {
        db.conn()
            .query_row("SELECT scu FROM uex_vehicles WHERE name = ?1", [n], |row| row.get::<_, f64>(0))
            .optional()
            .unwrap_or(None)
            .filter(|scu| *scu > 0.0)
    })
}

/// Profitable routes starting at any of `origins`, straight from UEX (not
/// cached, as they're only asked for now and then).
pub fn routes_from(db: &Db, origins: &[Terminal]) -> Result<Vec<Route>, String> {
    let all = terminals(db);
    let short_name = |id: i64, fallback: String| {
        all.iter().find(|t| t.id == id).map_or(fallback, |t| t.short_name.clone())
    };
    let mut routes = Vec::new();
    for origin in origins {
        for row in fetch(db, &format!("commodities_routes?id_terminal_origin={}", origin.id))? {
            let route = Route {
                commodity: text(&row, "commodity_name"),
                origin: origin.short_name.clone(),
                destination: short_name(
                    number(&row, "id_terminal_destination") as i64,
                    text(&row, "destination_terminal_name"),
                ),
                destination_system: text(&row, "destination_star_system_name"),
                buy: number(&row, "price_origin"),
                sell: number(&row, "price_destination"),
                stock: number(&row, "scu_origin"),
                distance: number(&row, "distance"),
            };
            if route.buy > 0.0 && route.sell > route.buy {
                routes.push(route);
            }
        }
    }
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(id: i64, name: &str) -> Terminal {
        Terminal {
            id,
            name: name.into(),
            short_name: name.into(),
            system: "Stanton".into(),
            place: "Port Tressler".into(),
            names: vec![name.to_lowercase(), "port tressler".into()],
        }
    }

    #[test]
    fn picks_the_shop_type_used() {
        let both = || vec![terminal(66, "Admin - Port Tressler"), terminal(262, "Platinum Bay - Port Tressler")];
        assert_eq!(pick_by_shop(both(), "SCShop_Admin_lt_base_g").unwrap().id, 66);
        assert_eq!(pick_by_shop(both(), "SCShop_PlatinumBay_Tressler").unwrap().id, 262);
        // Unknown shop type: Admin is the usual commodity terminal.
        assert_eq!(pick_by_shop(both(), "SCShop_Something").unwrap().id, 66);
        assert_eq!(pick_by_shop(vec![terminal(778, "Levski")], "SCShop_Levski_CargoOffice_Commodities").unwrap().id, 778);
        assert_eq!(pick_by_shop(Vec::new(), "SCShop_Admin"), None);
    }

    #[test]
    fn picks_which_app_token_to_use() {
        let s = |t: &str| Some(t.to_string());
        // The environment variable, then a saved token, then the built-in one.
        assert_eq!(pick_token(s("env"), s("saved"), Some("built")).unwrap().1, TokenSource::Environment);
        assert_eq!(pick_token(None, s("saved"), Some("built")).unwrap().1, TokenSource::Saved);
        assert_eq!(pick_token(None, None, Some("built")).unwrap(), ("built".to_string(), TokenSource::BuiltIn));
        // Blank ones don't count.
        assert_eq!(pick_token(s(" "), None, Some("built")).unwrap().1, TokenSource::BuiltIn);
        assert_eq!(pick_token(None, None, None), None);
        assert_eq!(pick_token(None, None, Some("")), None);
    }

    #[test]
    fn recognises_the_rate_limit() {
        assert!(is_rate_limit("UEX replied \"requests_limit_reached\""));
        assert!(is_rate_limit("requests_limit_reached"));
        assert!(is_rate_limit("http status: 429"));
        assert!(!is_rate_limit("terminal_not_found"));
    }

    #[test]
    fn reads_numbers_and_text_from_json() {
        let row: Value = serde_json::json!({"a": 12.5, "b": "7", "c": null, "d": " Levski "});
        assert_eq!(number(&row, "a"), 12.5);
        assert_eq!(number(&row, "b"), 7.0);
        assert_eq!(number(&row, "c"), 0.0);
        assert_eq!(text(&row, "d"), "Levski");
        assert_eq!(text(&row, "missing"), "");
    }

    #[test]
    fn caches_and_reads_prices() {
        let db = Db::open_in_memory_for_tests();
        // As UEX sends them: prices with short terminal names and no system...
        let price = |id: i64, short: &str, buy: f64, sell: f64| {
            serde_json::json!({"id_terminal": id, "terminal_name": short, "commodity_name": "Quartz",
                "price_buy": buy, "price_sell": sell, "scu_buy": 900, "date_modified": now_secs()})
        };
        let prices = vec![
            price(778, "Levski", 0.0, 4100.0),
            price(763, "Pyro Gateway (Nyx)", 0.0, 3900.0),
            price(66, "Port Tressler", 3560.0, 0.0),
            price(13, "Baijini Point", 0.0, 3800.0),
        ];
        // ...and terminals with full names and systems.
        let terminal = |id: i64, name: &str, system: &str| {
            serde_json::json!({"id": id, "name": name, "star_system_name": system})
        };
        // The price download's short names are what's shown.
        let shown = |id: i64| market(&db, "Quartz").into_iter().find(|r| r.id_terminal == id).unwrap().terminal;
        let terminals = vec![
            terminal(778, "Levski", "Nyx"),
            terminal(763, "Admin - Pyro Gateway (Nyx)", "Nyx"),
            terminal(66, "Admin - Port Tressler", "Stanton"),
            terminal(13, "Admin - Baijini Point", "Stanton"),
        ];
        store_prices(db.conn(), &prices).unwrap();
        store_terminals(db.conn(), &terminals).unwrap();
        // Pretend they were just downloaded so nothing is fetched.
        for key in ["prices", "terminals"] {
            db.conn().execute("INSERT INTO uex_meta VALUES (?1, ?2)", params![key, now_secs()]).unwrap();
        }

        assert_eq!(market(&db, "quartz").len(), 4);
        assert_eq!(shown(13), "Baijini Point");
        assert_eq!(market(&db, "quartz")[0].system.is_empty(), false);
        assert_eq!(best_sell(&db, "Quartz", Some("Stanton")).unwrap().terminal, "Baijini Point");
        assert_eq!(best_sell(&db, "Quartz", None).unwrap().terminal, "Levski");
        // Nothing buys it in Pyro, so fall back to anywhere.
        assert_eq!(best_sell(&db, "Quartz", Some("Pyro")).unwrap().price, 4100.0);
        assert!(best_sell(&db, "Gold", None).is_none());
        assert_eq!(commodity_names(&db), vec!["Quartz"]);
    }
}
