//! Follows Game.log, records trades as they happen, and reports what it saw
//! as `Notice`s. Shared by the `watch` (console) and `overlay` commands.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::db::Db;
use crate::locations;
use crate::names::Names;
use crate::parser::{ShipDestroyed, Side, TradeRequest};
use crate::profit;
use crate::ships;
use crate::uex;
use crate::tracker::{Outcome, ShipSeen, Tracker};
use crate::watcher::LogTail;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Set on Ctrl+C (or when the overlay closes); `run` finishes up and returns.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Something worth telling the user about.
#[derive(Debug, Clone)]
pub enum Notice {
    Watching(PathBuf),
    WaitingForLog(PathBuf),
    /// The log appeared after `WaitingForLog`.
    LogFound,
    NewSession,
    /// Stop was requested, but a recent trade is still being confirmed.
    Stopping,
    Trade(TradeNotice),
    ShipDestroyed(ShipLossNotice),
    /// A destroyed ship's name passed to a new ship of the same type,
    /// most likely its claimed replacement.
    ShipNamed { ship_id: String, class: String, name: String },
    /// Trades were sent to (or reversed on) your UEX account, or UEX said no.
    UexSync(crate::uex_sync::Report),
}

#[derive(Debug, Clone)]
pub struct TradeNotice {
    /// For `cancel-trade`.
    pub id: i64,
    pub outcome: Outcome,
    pub commodity: String,
    pub place: String,
    /// The ship the cargo was loaded to or from, if auto-loaded.
    pub ship: Option<String>,
    /// For completed sales only.
    pub sale: Option<SaleResult>,
    /// How the price compares with the market (from UEX), for completed trades.
    pub market: Option<uex::MarketNote>,
}

#[derive(Debug, Clone)]
pub enum SaleResult {
    Profit { profit: f64, margin: Option<f64> },
    /// None of the cargo had a recorded purchase.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ShipLossNotice {
    pub timestamp: DateTime<Utc>,
    pub ship_name: String,
    /// Empty if no tracked cargo was aboard.
    pub lost: Vec<LostCargo>,
}

#[derive(Debug, Clone)]
pub struct LostCargo {
    pub write_off_id: i64,
    pub commodity: String,
    pub scu: f64,
    pub cost: f64,
}

/// First Ctrl+C asks `run` to stop; a second one exits immediately.
pub fn install_ctrlc_handler() {
    let handler = ctrlc::set_handler(|| {
        if STOP_REQUESTED.swap(true, Ordering::SeqCst) {
            eprintln!("Stopped without confirming the last trade; it will be recorded next run.");
            std::process::exit(1);
        }
    });
    if let Err(e) = handler {
        eprintln!("Cannot handle Ctrl+C ({e}); stopping may cut off the last trade.");
    }
}

pub fn request_stop() {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}

/// Follows the log until a stop is requested and any recent trade is settled.
pub fn run(db: &Db, path: PathBuf, notify: &mut dyn FnMut(Notice)) {
    notify(Notice::Watching(path.clone()));
    let mut tail = LogTail::new(path);
    let mut tracker = Tracker::default();
    let mut names = Names::new(db);
    let mut waiting_for_log = false;
    let mut announced_stop = false;

    let mut save = |outcome: Outcome, notify: &mut dyn FnMut(Notice)| match db.record(&outcome) {
        Ok(Some(id)) => {
            notify(Notice::Trade(trade_notice(db, &mut names, id, outcome)));
            // Send it to your UEX account, if that's switched on.
            crate::uex_sync::nudge();
        }
        Ok(None) => {}
        Err(e) => eprintln!("Cannot save trade: {e}"),
    };

    loop {
        match tail.poll() {
            Ok(poll) => {
                if waiting_for_log {
                    notify(Notice::LogFound);
                    waiting_for_log = false;
                }
                if poll.reset {
                    for outcome in tracker.flush() {
                        save(outcome, notify);
                    }
                    handle_ships_destroyed(db, tracker.take_destroyed(), notify);
                    tracker = Tracker::default();
                    notify(Notice::NewSession);
                }
                for line in &poll.lines {
                    let outcomes = tracker.feed(line);
                    if let Some(location) = tracker.location() {
                        *CURRENT_LOCATION.lock().unwrap() = Some(location.to_string());
                    }
                    // First, so trades and losses can show the ship's type.
                    save_ships_seen(db, tracker.take_ships_seen(), notify);
                    for outcome in outcomes {
                        save(outcome, notify);
                    }
                }
                for outcome in tracker.expire(Utc::now()) {
                    save(outcome, notify);
                }
                handle_ships_destroyed(db, tracker.take_destroyed(), notify);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if !waiting_for_log {
                    notify(Notice::WaitingForLog(tail.path().clone()));
                    waiting_for_log = true;
                }
            }
            Err(e) => eprintln!("Error reading log: {e}"),
        }

        if stop_requested() {
            // A trade from the last few seconds might still fail, so keep
            // reading until it's settled rather than guessing.
            if !tracker.has_pending() {
                break;
            }
            if !announced_stop {
                notify(Notice::Stopping);
                announced_stop = true;
            }
        }
        wait_for_next_poll();
    }
}

/// Sleeps for `POLL_INTERVAL`, waking early if a stop is requested.
fn wait_for_next_poll() {
    let step = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    loop {
        thread::sleep(step);
        waited += step;
        if waited >= POLL_INTERVAL || stop_requested() {
            break;
        }
    }
}

fn trade_notice(db: &Db, names: &mut Names, id: i64, outcome: Outcome) -> TradeNotice {
    let trade = match &outcome {
        Outcome::Confirmed(t) | Outcome::Failed(t, _) => t,
    };
    let commodity = names.get(&trade.resource_guid);
    let place = locations::trade_place(db, trade);
    let ship = trade.ship_id.as_deref().map(|id| ships::label_for(db, id));
    let sale = match &outcome {
        Outcome::Confirmed(t) if t.side == Side::Sell => sale_result(db, t),
        _ => None,
    };
    let market = match &outcome {
        Outcome::Confirmed(t) => uex::market_note(db, t, &commodity),
        Outcome::Failed(..) => None,
    };
    TradeNotice { id, outcome, commodity, place, ship, sale, market }
}

/// Where the player last arrived, while tracking; see `current_location`.
static CURRENT_LOCATION: Mutex<Option<String>> = Mutex::new(None);

/// The location code the tracker last saw the player arrive at, if tracking.
pub fn current_location() -> Option<String> {
    CURRENT_LOCATION.lock().unwrap().clone()
}

pub fn save_ships_seen(db: &Db, seen: Vec<ShipSeen>, notify: &mut dyn FnMut(Notice)) {
    for ship in seen {
        match db.save_ship_seen(&ship.ship_id, &ship.class, ship.timestamp) {
            Ok(Some(name)) => notify(Notice::ShipNamed {
                ship_id: ship.ship_id,
                class: ship.class,
                name,
            }),
            Ok(None) => {}
            Err(e) => eprintln!("Cannot save ship: {e}"),
        }
    }
}

/// The profit on a sale that was just recorded.
fn sale_result(db: &Db, sell: &TradeRequest) -> Option<SaleResult> {
    let report = match crate::load_report(db) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("Cannot calculate profit: {e}");
            return None;
        }
    };
    let sale = report.sales.iter().find(|s| {
        s.sell.timestamp == sell.timestamp
            && s.sell.kiosk_id == sell.kiosk_id
            && s.sell.resource_guid == sell.resource_guid
    })?;
    Some(match sale.profit() {
        Some(profit) => SaleResult::Profit { profit, margin: sale.margin() },
        None => SaleResult::Unknown,
    })
}

pub fn handle_ships_destroyed(
    db: &Db,
    destroyed: Vec<ShipDestroyed>,
    notify: &mut dyn FnMut(Notice),
) {
    for ship in &destroyed {
        match write_off_ship_cargo(db, ship) {
            Ok(Some(notice)) => notify(Notice::ShipDestroyed(notice)),
            Ok(None) => {}
            Err(e) => eprintln!("Cannot write off cargo for destroyed {}: {e}", ship.class),
        }
    }
}

/// Writes off the tracked cargo that was aboard a destroyed ship. Returns
/// `None` if this destruction was already handled on an earlier run.
fn write_off_ship_cargo(
    db: &Db,
    destroyed: &ShipDestroyed,
) -> rusqlite::Result<Option<ShipLossNotice>> {
    if !db.record_ship_loss(destroyed.timestamp, &destroyed.ship_id, &destroyed.class)? {
        return Ok(None);
    }
    db.save_ship_seen(&destroyed.ship_id, &destroyed.class, destroyed.timestamp)?;
    let ship_name = ships::label_for(db, &destroyed.ship_id);

    // What was aboard at the moment it was destroyed.
    let at = destroyed.timestamp;
    let trades: Vec<_> = db.completed_trades()?.into_iter().filter(|t| t.timestamp <= at).collect();
    let write_offs: Vec<_> = db.write_offs()?.into_iter().filter(|w| w.timestamp <= at).collect();
    let report = profit::calculate(&trades, &write_offs);

    // (commodity, SCU, cost) per commodity aboard.
    let mut aboard: Vec<(String, f64, f64)> = Vec::new();
    for lot in &report.holdings {
        if lot.buy.ship_id.as_deref() != Some(destroyed.ship_id.as_str()) {
            continue;
        }
        match aboard.iter_mut().find(|(guid, _, _)| *guid == lot.buy.resource_guid) {
            Some(entry) => {
                entry.1 += lot.scu_left;
                entry.2 += lot.cost_left();
            }
            None => aboard.push((lot.buy.resource_guid.clone(), lot.scu_left, lot.cost_left())),
        }
    }

    let note = format!("{ship_name} destroyed");
    let mut names = Names::new(db);
    let mut lost = Vec::new();
    for (guid, scu, cost) in aboard {
        let write_off_id = db.add_write_off(at, &guid, scu, Some(&note), Some(&destroyed.ship_id))?;
        lost.push(LostCargo {
            write_off_id,
            commodity: names.get(&guid),
            scu,
            cost,
        });
    }
    Ok(Some(ShipLossNotice {
        timestamp: at,
        ship_name,
        lost,
    }))
}
