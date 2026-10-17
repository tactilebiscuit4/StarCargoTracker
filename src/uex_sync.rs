//! Sends your trades to your UEX account: each completed trade becomes a
//! wallet entry (debit for a buy, credit for a sale) and a trade-history
//! entry. Cancelling a trade reverses its wallet entry and removes its
//! history entry; restoring it sends them again.
//!
//! Needs your UEX secret key (from your UEX profile) and an app token (from
//! UEX's My Apps page). It starts in UEX's test mode, where nothing real is
//! recorded; `uex live` switches to real entries, and anything sent in test
//! mode is sent again for real. Every completed trade is sent, past ones
//! included; failed trades never are.
//!
//! What's been sent is recorded per trade, so nothing is sent twice. Sending
//! happens in small batches to stay well inside UEX's limits, waits while
//! UEX can't be reached, and skips entries UEX rejected until `uex push`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::db::{Db, StoredTrade};
use crate::names::Names;
use crate::parser::Side;
use crate::tracker::Outcome;
use crate::{format, locations, uex};

pub const SCHEMA: &str = "
-- What has been sent to the user's UEX account, per trade, place
-- ('wallet' or 'history') and mode (live 1, test 0).
CREATE TABLE IF NOT EXISTS uex_sync (
    trade_id  INTEGER NOT NULL,
    target    TEXT NOT NULL CHECK (target IN ('wallet', 'history')),
    live      INTEGER NOT NULL,
    remote_id TEXT,                   -- wallet transaction hash or UEX trade ID
    reversed  INTEGER NOT NULL DEFAULT 0, -- 1 once undone after a cancel
    error     TEXT,                   -- why UEX rejected it, if it did
    PRIMARY KEY (trade_id, target, live)
);
";

const TIMEOUT: Duration = Duration::from_secs(15);
/// UEX allows 120 requests a minute; one batch stays far below that.
const BATCH: usize = 20;
const PAUSE: Duration = Duration::from_millis(600);
/// After failing to reach UEX, wait this long before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(2 * 60);

static LAST_OFFLINE: Mutex<Option<Instant>> = Mutex::new(None);

/// Shown when UEX says too many requests were made with the app token.
const RATE_LIMITED: &str = "UEX's request limit was reached";

/// Settings, from the database (or environment variables for the keys).
pub struct Config {
    pub secret: Option<String>,
    pub token: Option<String>,
    pub token_source: Option<uex::TokenSource>,
    pub enabled: bool,
    pub live: bool,
    pub wallet: bool,
    pub history: bool,
    /// Only trades after this are sent, unless `include_past`; set when the
    /// secret key is saved or syncing first switched on.
    pub since: Option<DateTime<Utc>>,
    /// The user chose to send trades from before `since` too (`uex send-past`).
    pub include_past: bool,
}

impl Config {
    /// Whether a trade is one to send, given the start point.
    fn wants(&self, trade: &StoredTrade) -> bool {
        self.include_past || self.since.is_some_and(|since| trade_of(trade).timestamp >= since)
    }
}

/// Records now as the point from which trades are sent, unless one is set.
pub fn mark_start(db: &Db) {
    if config(db).since.is_none() {
        let now = Utc::now().to_rfc3339();
        if let Err(e) = db.set_setting("uex_since", Some(&now)) {
            eprintln!("Cannot save setting: {e}");
        }
    }
}

/// Completed trades from before the start point that won't be sent unless
/// the user asks (`uex send-past`).
pub fn held_back(db: &Db) -> usize {
    let config = config(db);
    if config.include_past {
        return 0;
    }
    db.completed_trades_with_ids()
        .unwrap_or_default()
        .iter()
        .filter(|t| !t.cancelled && !config.wants(t))
        .count()
}

pub fn config(db: &Db) -> Config {
    let setting = |key: &str| db.setting(key).ok().flatten();
    let from_env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let token = uex::app_token(db);
    Config {
        secret: from_env("STAR_CARGO_UEX_SECRET").or_else(|| setting("uex_secret")),
        token_source: token.as_ref().map(|(_, source)| *source),
        token: token.map(|(token, _)| token),
        enabled: setting("uex_sync").as_deref() == Some("on"),
        live: setting("uex_live").as_deref() == Some("1"),
        wallet: setting("uex_wallet").as_deref() != Some("off"),
        history: setting("uex_history").as_deref() != Some("off"),
        since: setting("uex_since")
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&Utc)),
        include_past: setting("uex_include_past").as_deref() == Some("1"),
    }
}

/// What a sync run did.
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub sent: usize,
    pub reversed: usize,
    /// IDs of the trades sent and reversed this run, oldest first.
    pub sent_trades: Vec<i64>,
    pub reversed_trades: Vec<i64>,
    /// Sent in test mode, so nothing was really recorded.
    pub test: bool,
    /// Entries UEX rejected this run, with why.
    pub rejected: Vec<String>,
    /// Set if UEX couldn't be reached.
    pub offline: Option<String>,
    /// More to send; the next run carries on.
    pub more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    Wallet,
    History,
}

impl Target {
    fn key(self) -> &'static str {
        match self {
            Target::Wallet => "wallet",
            Target::History => "history",
        }
    }
}

enum SendError {
    /// UEX couldn't be reached; try again later.
    Offline(String),
    /// UEX said no, e.g. `terminal_not_found`.
    Rejected(String),
}

/// One row of `uex_sync`.
struct Sent {
    remote_id: Option<String>,
    reversed: bool,
    error: Option<String>,
}

/// Sends what hasn't been sent yet (and reverses cancelled trades), up to one
/// batch. `force` retries entries UEX rejected before and ignores a recent
/// failure to connect.
pub fn sync(db: &Db, force: bool) -> Report {
    let mut report = Report::default();
    if !config(db).enabled || config(db).secret.is_none() {
        return report;
    }
    // E.g. a secret key from the environment, never saved with `uex secret`.
    mark_start(db);
    let config = config(db);
    report.test = !config.live;
    if !force && LAST_OFFLINE.lock().unwrap().is_some_and(|at| at.elapsed() < RETRY_AFTER) {
        report.offline = Some("waiting to retry".into());
        return report;
    }

    let trades = match db.completed_trades_with_ids() {
        Ok(trades) => trades,
        Err(e) => {
            report.rejected.push(format!("cannot read trades: {e}"));
            return report;
        }
    };
    let mut names = Names::new(db);
    let mut requests = 0;
    let targets = [(Target::Wallet, config.wallet), (Target::History, config.history)];

    for trade in &trades {
        for (target, wanted) in targets {
            if !wanted {
                continue;
            }
            let sent = load(db, trade.id, target, config.live);
            let is_present = sent.as_ref().is_some_and(|s| s.remote_id.is_some() && !s.reversed);
            let rejected_before = sent.as_ref().is_some_and(|s| s.error.is_some());
            // Rejected before: leave it until `uex push`, rather than asking again every run.
            if rejected_before && !force {
                continue;
            }
            let action = match (trade.cancelled, is_present) {
                // Trades from before the start point aren't sent unless asked
                // for, but one already sent is still reversed if cancelled.
                (false, false) if config.wants(trade) => Action::Send,
                (true, true) => Action::Reverse,
                _ => continue,
            };

            if requests == BATCH {
                report.more = true;
                return report;
            }
            if requests > 0 {
                std::thread::sleep(PAUSE);
            }
            requests += 1;

            let result = match action {
                Action::Send => send(db, &config, &mut names, trade, target),
                Action::Reverse => reverse(db, &config, &mut names, trade, target, sent.as_ref()),
            };
            match result {
                Ok(remote_id) => {
                    record(db, trade.id, target, config.live, remote_id, action == Action::Reverse, None);
                    let (count, ids) = match action {
                        Action::Send => (&mut report.sent, &mut report.sent_trades),
                        Action::Reverse => (&mut report.reversed, &mut report.reversed_trades),
                    };
                    *count += 1;
                    // A trade can go to both the wallet and the trade history.
                    if !ids.contains(&trade.id) {
                        ids.push(trade.id);
                    }
                }
                // A problem with the account, not this trade: stop and say so.
                Err(SendError::Rejected(why)) if is_account_error(&why) => {
                    report.rejected.push(account_problem(&why));
                    return report;
                }
                Err(SendError::Rejected(why)) => {
                    let remote = sent.as_ref().and_then(|s| s.remote_id.clone());
                    let reversed = sent.as_ref().is_some_and(|s| s.reversed);
                    record(db, trade.id, target, config.live, remote, reversed, Some(&why));
                    report.rejected.push(format!("#{} {}: {why}", trade.id, target.key()));
                }
                Err(SendError::Offline(why)) => {
                    *LAST_OFFLINE.lock().unwrap() = Some(Instant::now());
                    report.offline = Some(why);
                    return report;
                }
            }
        }
    }
    *LAST_OFFLINE.lock().unwrap() = None;
    report
}

/// UEX errors about the account or keys, which no trade can get past.
fn is_account_error(why: &str) -> bool {
    matches!(
        why,
        "missing_secret_key"
            | "invalid_secret_key"
            | "user_not_found"
            | "user_not_allowed"
            | "user_not_verified"
            // Not in UEX's documentation, but what it says without a valid
            // app token or key.
            | "not_allowed"
            | "HTTP 401"
            | "HTTP 403"
    )
}

fn account_problem(why: &str) -> String {
    let fix = match why {
        "missing_secret_key" | "invalid_secret_key" | "user_not_found" => {
            "check your secret key (uex secret KEY, from your UEX profile)"
        }
        "user_not_verified" => "verify your RSI handle on your UEX profile",
        "user_not_allowed" => "UEX has disabled this account",
        _ => "check your app token (uex token TOKEN, from uexcorp.space/api/apps) and secret key",
    };
    format!("UEX won't accept entries for your account ({why}): {fix}. Nothing was sent.")
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Send,
    Reverse,
}

fn load(db: &Db, trade_id: i64, target: Target, live: bool) -> Option<Sent> {
    db.conn()
        .query_row(
            "SELECT remote_id, reversed, error FROM uex_sync
             WHERE trade_id = ?1 AND target = ?2 AND live = ?3",
            params![trade_id, target.key(), live],
            |row| {
                Ok(Sent {
                    remote_id: row.get(0)?,
                    reversed: row.get::<_, i64>(1)? != 0,
                    error: row.get(2)?,
                })
            },
        )
        .optional()
        .unwrap_or(None)
}

fn record(
    db: &Db,
    trade_id: i64,
    target: Target,
    live: bool,
    remote_id: Option<String>,
    reversed: bool,
    error: Option<&str>,
) {
    let saved = db.conn().execute(
        "INSERT INTO uex_sync (trade_id, target, live, remote_id, reversed, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (trade_id, target, live) DO UPDATE SET
             remote_id = excluded.remote_id, reversed = excluded.reversed, error = excluded.error",
        params![trade_id, target.key(), live, remote_id, reversed, error],
    );
    if let Err(e) = saved {
        eprintln!("Cannot record what was sent to UEX: {e}");
    }
}

/// The trade itself, which `completed_trades_with_ids` guarantees.
fn trade_of(stored: &StoredTrade) -> &crate::parser::TradeRequest {
    match &stored.outcome {
        Outcome::Confirmed(t) | Outcome::Failed(t, _) => t,
    }
}

/// e.g. `Bought 16 SCU Quartz at Port Tressler (tracker #21)`
fn describe(db: &Db, names: &mut Names, stored: &StoredTrade) -> String {
    let trade = trade_of(stored);
    format!(
        "{} {} SCU {} at {} (tracker #{})",
        match trade.side {
            Side::Buy => "Bought",
            Side::Sell => "Sold",
        },
        format::scu(trade.scu),
        names.get(&trade.resource_guid),
        locations::trade_place(db, trade),
        stored.id
    )
}

fn send(
    db: &Db,
    config: &Config,
    names: &mut Names,
    stored: &StoredTrade,
    target: Target,
) -> Result<Option<String>, SendError> {
    let trade = trade_of(stored);
    let operation = |buy: &'static str, sell: &'static str| match trade.side {
        Side::Buy => buy,
        Side::Sell => sell,
    };
    match target {
        Target::Wallet => {
            let body = json!({
                "is_production": i32::from(config.live),
                "description": describe(db, names, stored),
                "operation": operation("debit", "credit"),
                "value": trade.total_price,
            });
            let reply = request(config, "POST", "wallet_add", Some(body))?;
            Ok(remote_id(&reply, "transaction_hash"))
        }
        Target::History => {
            let commodity = names.get(&trade.resource_guid);
            let terminal = uex::terminal_for(db, trade).ok_or_else(|| {
                SendError::Rejected(format!(
                    "no UEX terminal for {} (link one with link-terminal, then uex push)",
                    locations::trade_place(db, trade)
                ))
            })?;
            let id_commodity = uex::commodity_id(db, &commodity)
                .ok_or_else(|| SendError::Rejected(format!("UEX has no commodity named {commodity}")))?;
            // UEX takes whole SCU.
            let scu = trade.scu.round().max(1.0);
            let body = json!({
                "is_production": i32::from(config.live),
                "id_terminal": terminal.id,
                "id_commodity": id_commodity,
                "operation": operation("buy", "sell"),
                "scu": scu as i64,
                "price": trade.total_price / scu,
            });
            let reply = request(config, "POST", "user_trades_add", Some(body))?;
            Ok(remote_id(&reply, "id_user_trade"))
        }
    }
}

/// Undoes what was sent for a cancelled trade.
fn reverse(
    db: &Db,
    config: &Config,
    names: &mut Names,
    stored: &StoredTrade,
    target: Target,
    sent: Option<&Sent>,
) -> Result<Option<String>, SendError> {
    let trade = trade_of(stored);
    let remote = sent.and_then(|s| s.remote_id.clone());
    match target {
        // UEX's API can't delete wallet entries (only the site can, by hand),
        // so add the opposite one.
        Target::Wallet => {
            let body = json!({
                "is_production": i32::from(config.live),
                "description": format!("Cancelled: {}", describe(db, names, stored)),
                "operation": match trade.side {
                    Side::Buy => "credit",
                    Side::Sell => "debit",
                },
                "value": trade.total_price,
            });
            request(config, "POST", "wallet_add", Some(body))?;
            Ok(remote)
        }
        Target::History => {
            let id = remote.clone().unwrap_or_default();
            if id.is_empty() {
                // UEX gave no ID to remove it by.
                return Ok(remote);
            }
            let path = format!("user_trades_remove?id={id}&is_production={}", i32::from(config.live));
            match request(config, "DELETE", &path, None) {
                Ok(_) => Ok(remote),
                // Already gone from UEX is as good as removed.
                Err(SendError::Rejected(why)) if why == "trade_not_found" => Ok(remote),
                Err(e) => Err(e),
            }
        }
    }
}

/// The ID UEX gave a new entry, wherever in the reply it is.
fn remote_id(reply: &Value, key: &str) -> Option<String> {
    let value = if reply["data"][key].is_null() { &reply[key] } else { &reply["data"][key] };
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        // Sent, but with no ID to undo it by later.
        _ => Some(String::new()),
    }
}

/// Calls a UEX account endpoint with the user's keys.
fn request(config: &Config, method: &str, path: &str, body: Option<Value>) -> Result<Value, SendError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{}/{path}", uex::base_url());
    let secret = config.secret.as_deref().unwrap_or_default();
    let bearer = config.token.as_deref().map(|t| format!("Bearer {t}"));

    let response = match method {
        "POST" => {
            let mut req = agent.post(&url).header("secret-key", secret).header("Accept", "application/json")
                .header("X-Client-Version", uex::CLIENT_VERSION);
            if let Some(bearer) = &bearer {
                req = req.header("Authorization", bearer);
            }
            req.content_type("application/json")
                .send(body.unwrap_or(Value::Null).to_string())
        }
        "DELETE" => {
            let mut req = agent.delete(&url).header("secret-key", secret).header("Accept", "application/json")
                .header("X-Client-Version", uex::CLIENT_VERSION);
            if let Some(bearer) = &bearer {
                req = req.header("Authorization", bearer);
            }
            req.call()
        }
        _ => {
            let mut req = agent.get(&url).header("secret-key", secret).header("Accept", "application/json")
                .header("X-Client-Version", uex::CLIENT_VERSION);
            if let Some(bearer) = &bearer {
                req = req.header("Authorization", bearer);
            }
            req.call()
        }
    };
    let mut response = response.map_err(|e| SendError::Offline(e.to_string()))?;
    let status = response.status().as_u16();
    // Too many requests: wait and try again later, like being offline.
    if status == 429 {
        return Err(SendError::Offline(RATE_LIMITED.into()));
    }
    let text = response.body_mut().read_to_string().unwrap_or_default();
    let json: Value = match serde_json::from_str(&text) {
        Ok(json) => json,
        Err(_) if status >= 500 => return Err(SendError::Offline(format!("HTTP {status}"))),
        Err(_) => return Err(SendError::Rejected(format!("HTTP {status}"))),
    };
    match json["status"].as_str() {
        Some("ok") => Ok(json),
        Some(why) if uex::is_rate_limit(why) => Err(SendError::Offline(RATE_LIMITED.into())),
        Some(why) => Err(SendError::Rejected(crate::format::clean(why))),
        None if status >= 500 => Err(SendError::Offline(format!("HTTP {status}"))),
        None => Err(SendError::Rejected(format!("HTTP {status}"))),
    }
}

/// Wakes the background sender; see `start_background`.
static NUDGE: OnceLock<Sender<()>> = OnceLock::new();
/// How often the background sender checks for unsent trades.
const INTERVAL: Duration = Duration::from_secs(30);

/// Sends in the background while tracking: at once when nudged (a new
/// trade, a cancel, syncing switched on), otherwise every 30 seconds.
/// `on_report` is called when something was sent or went wrong.
pub fn start_background(on_report: impl Fn(&Report) + Send + 'static) {
    let (nudges, woken) = mpsc::channel();
    if NUDGE.set(nudges).is_err() {
        return; // Already running.
    }
    thread::spawn(move || {
        let db = match crate::open_db() {
            Ok(db) => db,
            Err(e) => {
                eprintln!("Cannot open trade database for UEX syncing: {e}");
                return;
            }
        };
        let mut wait = Duration::ZERO;
        let mut was_offline = false;
        loop {
            let _ = woken.recv_timeout(wait);
            while woken.try_recv().is_ok() {}

            let report = sync(&db, FORCE_NEXT.swap(false, Ordering::SeqCst));
            // Mention losing UEX once, not every retry.
            let newly_offline = report.offline.as_deref().is_some_and(|o| o != "waiting to retry") && !was_offline;
            was_offline = report.offline.is_some();
            if report.sent + report.reversed > 0 || !report.rejected.is_empty() || newly_offline {
                on_report(&report);
            }
            // Carry on soon if there's more, pacing for UEX's limits.
            wait = if report.more { Duration::from_secs(5) } else { INTERVAL };
        }
    });
}

/// Whether the background sender is running (i.e. we're tracking).
pub fn background_running() -> bool {
    NUDGE.get().is_some()
}

/// Asks the background sender to check now.
pub fn nudge() {
    if let Some(nudge) = NUDGE.get() {
        let _ = nudge.send(());
    }
}

/// Set by `uex push`: the background sender's next run retries rejected entries.
static FORCE_NEXT: AtomicBool = AtomicBool::new(false);

/// Asks the background sender to check now, retrying rejected entries.
pub fn nudge_force() {
    FORCE_NEXT.store(true, Ordering::SeqCst);
    nudge();
}

/// Your UEX wallet balance, to check things add up.
pub fn wallet_balance(db: &Db) -> Result<f64, String> {
    let config = config(db);
    if config.secret.is_none() {
        return Err("no secret key set".into());
    }
    match request(&config, "GET", "wallet_balance", None) {
        Ok(reply) => reply["data"]["balance"]
            .as_f64()
            .or_else(|| reply["data"]["balance"].as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| "no balance in UEX's reply".into()),
        Err(SendError::Offline(e)) => Err(format!("can't reach UEX ({e})")),
        Err(SendError::Rejected(why)) => Err(why),
    }
}

/// How many of your completed trades are sent to each place in the current
/// mode, as (target, sent, waiting, rejected).
pub fn counts(db: &Db) -> Vec<(&'static str, usize, usize, usize)> {
    let config = config(db);
    let trades = db.completed_trades_with_ids().unwrap_or_default();
    [Target::Wallet, Target::History]
        .into_iter()
        .map(|target| {
            let (mut sent, mut waiting, mut rejected) = (0, 0, 0);
            for trade in trades.iter().filter(|t| !t.cancelled && config.wants(t)) {
                match load(db, trade.id, target, config.live) {
                    Some(s) if s.remote_id.is_some() && !s.reversed => sent += 1,
                    Some(s) if s.error.is_some() => rejected += 1,
                    _ => waiting += 1,
                }
            }
            (target.key(), sent, waiting, rejected)
        })
        .collect()
}

/// Recent rejections, newest first, to show in `uex`.
pub fn rejections(db: &Db, limit: usize) -> Vec<String> {
    let config = config(db);
    let query = || -> rusqlite::Result<Vec<String>> {
        let mut stmt = db.conn().prepare(
            "SELECT trade_id, target, error FROM uex_sync
             WHERE live = ?1 AND error IS NOT NULL AND (remote_id IS NULL OR reversed = 1)
             ORDER BY trade_id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![config.live, limit as i64], |row| {
            Ok(format!("#{} {}: {}", row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        rows.collect()
    };
    query().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_id_in_a_reply() {
        assert_eq!(remote_id(&json!({"status": "ok", "data": {"id_user_trade": 42}}), "id_user_trade").as_deref(), Some("42"));
        assert_eq!(remote_id(&json!({"status": "ok", "transaction_hash": "ab12"}), "transaction_hash").as_deref(), Some("ab12"));
        assert_eq!(remote_id(&json!({"status": "ok"}), "transaction_hash").as_deref(), Some(""));
    }

    #[test]
    fn account_errors_stop_the_run() {
        assert!(is_account_error("invalid_secret_key"));
        assert!(is_account_error("user_not_verified"));
        assert!(!is_account_error("terminal_not_found"));
        assert!(account_problem("user_not_verified").contains("RSI handle"));
    }

    #[test]
    fn syncing_is_off_until_switched_on() {
        let db = Db::open_in_memory_for_tests();
        let config = config(&db);
        assert!(!config.enabled && !config.live && config.wallet && config.history);
        // Nothing to do (and no network) while off.
        let report = sync(&db, true);
        assert_eq!((report.sent, report.reversed), (0, 0));
        assert!(report.offline.is_none());
    }

    #[test]
    fn only_trades_after_the_start_point_are_sent() {
        let db = Db::open_in_memory_for_tests();
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let trade = |id: i64, when: &str| StoredTrade {
            id,
            cancelled: false,
            outcome: Outcome::Confirmed(crate::parser::TradeRequest {
                timestamp: at(when),
                side: Side::Buy,
                shop_id: "1".into(),
                shop_name: "SCShop_Test".into(),
                kiosk_id: "2".into(),
                resource_guid: "g".into(),
                scu: 1.0,
                total_price: 1.0,
                location: None,
                ship_id: None,
            }),
        };
        let old = trade(1, "2026-10-04T06:20:30Z");
        let new = trade(2, "2026-10-14T06:20:30Z");

        // No start point yet: nothing qualifies.
        assert!(!config(&db).wants(&new));
        db.set_setting("uex_since", Some("2026-10-13T12:00:00+00:00")).unwrap();
        assert!(!config(&db).wants(&old));
        assert!(config(&db).wants(&new));
        // Marking the start again doesn't move it.
        mark_start(&db);
        assert_eq!(config(&db).since, Some(at("2026-10-13T12:00:00Z")));
        // `uex send-past yes` includes everything.
        db.set_setting("uex_include_past", Some("1")).unwrap();
        assert!(config(&db).wants(&old));
    }

    #[test]
    fn records_what_was_sent_per_mode() {
        let db = Db::open_in_memory_for_tests();
        record(&db, 7, Target::Wallet, false, Some("hash".into()), false, None);
        assert!(load(&db, 7, Target::Wallet, false).unwrap().remote_id.is_some());
        // Sent in test mode doesn't count as sent for real.
        assert!(load(&db, 7, Target::Wallet, true).is_none());
        record(&db, 7, Target::History, true, None, false, Some("terminal_not_found"));
        assert_eq!(load(&db, 7, Target::History, true).unwrap().error.as_deref(), Some("terminal_not_found"));
    }
}
