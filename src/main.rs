mod banner;
mod color;
mod db;
mod export;
mod format;
mod game;
mod live;
mod locations;
mod names;
mod overlay;
mod parser;
mod profit;
mod shell;
mod ships;
mod tracker;
mod uex;
mod uex_sync;
mod watcher;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;

use chrono::Utc;

use db::Db;
use live::{Notice, SaleResult};
use names::Names;
use parser::Side;
use tracker::{Outcome, Tracker};

const DEFAULT_HISTORY: usize = 20;

const USAGE: &str = "\
Usage: star-cargo-tracker [COMMAND]
With no command, tracking starts (the same as watch).

Tracking:
  star-cargo-tracker watch [GAME_LOG]     Follow Game.log and record trades live, with a
                                          cargo> prompt for running the commands below
  star-cargo-tracker overlay [GAME_LOG]   Like watch, plus an on-screen overlay over the game
  star-cargo-tracker import [PATH...]     Record trades from log files or folders; with no
                                          PATH, imports the game's logbackups folder
  star-cargo-tracker game-folder [PATH]   Show where Star Citizen is installed, or set it

Trades and profit:
  star-cargo-tracker history [COUNT] [--failed]
                                          Recent trades and overall totals; trades that
                                          failed in the game are hidden unless --failed
  star-cargo-tracker profit               Profit per sale, losses, and unsold cargo
  star-cargo-tracker export [PATH]        Save trades, profit, write-offs and ships to an
                                          Excel workbook (default: your Documents folder)
  star-cargo-tracker cancel-trade ID      Stop counting a trade that didn't really happen,
                                          e.g. a sale whose cargo never went down the elevator
  star-cargo-tracker restore-trade ID     Count a cancelled trade again
  star-cargo-tracker write-off COMMODITY SCU|all [NOTE]
                                          Record cargo you lost (destroyed, jettisoned...)
  star-cargo-tracker delete-write-off ID  Undo a write-off

Market (UEX data):
  star-cargo-tracker prices COMMODITY     Where to buy it cheapest and sell it best
  star-cargo-tracker routes [FROM] [--scu N] [--budget AMOUNT]
                                          Most profitable runs from where you are (or FROM),
                                          for your ship's cargo space (or N SCU);
                                          with --budget, only what AMOUNT aUEC can buy (e.g. 250k)
  star-cargo-tracker terminals [SEARCH]   UEX terminals matched to your locations, or search them
  star-cargo-tracker link-terminal LOCATION ID
                                          Use UEX terminal ID for a location (its name as
                                          terminals shows it, or its code)

UEX account (sends your trades to your UEX wallet and trade history):
  star-cargo-tracker uex                  Show syncing status, what's been sent, and your balance
  star-cargo-tracker uex secret KEY       Save your UEX secret key (from your UEX profile)
  star-cargo-tracker uex token TOKEN      Save your UEX app token (make an app at uexcorp.space/api/apps)
  star-cargo-tracker uex sync on|off      Send new completed trades (starts in test mode)
  star-cargo-tracker uex send-past        Also send trades from before syncing started
  star-cargo-tracker uex live|test        Real entries, or test mode (nothing real recorded)
  star-cargo-tracker uex wallet on|off    Send to your wallet or not (same for: uex history)
  star-cargo-tracker uex push             Send anything waiting now, retrying rejected ones
  star-cargo-tracker uex forget           Remove your saved key and token, and stop syncing

Ships and locations:
  star-cargo-tracker ships                Your ships, their cargo, and destroyed ships
  star-cargo-tracker name-ship SHIP NAME  Name a ship; SHIP is its ID, the end of its ID,
                                          or part of its type (e.g. Asgard)
  star-cargo-tracker locations [--failed] Trade locations and their names; --failed also
                                          counts trades that failed in the game
  star-cargo-tracker name-location CODE NAME
                                          Set the name shown for a location code

Other:
  star-cargo-tracker help                 Show this help

Notes:
  IDs like #12 are shown by history (trades), profit (write-offs) and ships.
  Without GAME_LOG, Star Citizen is found automatically (or asked for once) and
  remembered; see game-folder.
  Trades are stored in %APPDATA%\\star-cargo-tracker\\trades.db on Windows, or
  ~/.local/share/star-cargo-tracker/trades.db on Linux (set STAR_CARGO_DB to use a
  different file).
  The overlay only shows over the game in Borderless Windowed mode.
  Market data comes from UEX (uexcorp.space). Optional: STAR_CARGO_UEX_TOKEN (an app
  token) and STAR_CARGO_UEX_URL (if its address changes).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);
    let db_path = db_path();
    let db = open_db();

    // Where Game.log is: as given to watch/overlay, else the saved or
    // automatically found game folder.
    let given = match command {
        Some("watch" | "overlay") => args.get(1).map(PathBuf::from),
        _ => None,
    };
    let mut log = match &given {
        Some(path) if path.is_dir() => match game::resolve(path) {
            Some(live) => Some(game::log_file(&live)),
            None => {
                eprintln!("Star Citizen isn't in {}. {}", path.display(), game::how_to_set());
                return ExitCode::FAILURE;
            }
        },
        Some(path) => Some(path.clone()),
        None => db.as_ref().ok().and_then(game::find).map(|live| game::log_file(&live)),
    };
    banner::print(log.as_deref(), db_path.as_deref().map_err(String::as_str));

    match command {
        None | Some("watch" | "overlay") => {
            if log.is_none() {
                log = db.as_ref().ok().and_then(game::ask).map(|live| game::log_file(&live));
            }
            let Some(log) = log else {
                eprintln!("{}", game::how_to_set());
                return ExitCode::FAILURE;
            };
            // Tracking opens its own connections on its threads.
            drop(db);
            track(log, command == Some("overlay"))
        }
        Some("help") => {
            shell::print_help(USAGE);
            ExitCode::SUCCESS
        }
        Some(_) => {
            let db = match db {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("Cannot open trade database: {e}");
                    return ExitCode::FAILURE;
                }
            };
            run_command(&db, &args).unwrap_or_else(|| {
                eprintln!("{USAGE}");
                ExitCode::FAILURE
            })
        }
    }
}

/// Runs a one-off command: anything but `help`, `watch` and `overlay`. Used
/// both from the command line and the `cargo>` prompt. Returns `None` if the
/// command is unknown or is missing details.
fn run_command(db: &Db, args: &[String]) -> Option<ExitCode> {
    let command = args.first()?.as_str();
    let valid = match command {
        "history" | "locations" | "profit" | "ships" | "import" | "routes" | "terminals"
        | "game-folder" | "export" => true,
        "prices" => args.len() > 1,
        "uex" => true,
        "link-terminal" => args.len() >= 3,
        "name-location" | "write-off" | "name-ship" => args.len() > 2,
        "delete-write-off" | "cancel-trade" | "restore-trade" => args.len() == 2,
        _ => false,
    };
    if !valid {
        return None;
    }

    Some(match command {
        "import" if args.len() > 1 => import(db, &args[1..]),
        "import" => {
            let Some(live) = game::find(db) else {
                eprintln!("Couldn't find Star Citizen's logbackups folder. {}", game::how_to_set());
                return Some(ExitCode::FAILURE);
            };
            let backups = game::backups_folder(&live).display().to_string();
            println!("Importing {backups}");
            import(db, &[backups])
        }
        "game-folder" => game_folder(db, &args[1..].join(" ")),
        "locations" => match args.get(1).map(String::as_str) {
            None => list_locations(db, false),
            Some("--failed" | "-f") => list_locations(db, true),
            Some(_) => return None,
        },
        "prices" => show_prices(db, &args[1..].join(" ")),
        "routes" => show_routes(db, &args[1..])?,
        "terminals" => show_terminals(db, &args[1..].join(" ")),
        "link-terminal" => link_terminal(db, &args[1..args.len() - 1].join(" "), &args[args.len() - 1]),
        "uex" => uex_command(db, &args[1..])?,
        "profit" => show_profit(db),
        "export" => export(db, &args[1..].join(" ")),
        "name-location" => name_location(db, &args[1], &args[2..].join(" ")),
        "ships" => list_ships(db),
        "name-ship" => name_ship(db, &args[1], &args[2..].join(" ")),
        "write-off" => {
            let note = args[3..].join(" ");
            write_off(db, &args[1], &args[2], (!note.is_empty()).then_some(note.as_str()))
        }
        "delete-write-off" => delete_write_off(db, &args[1]),
        "cancel-trade" => set_trade_cancelled(db, &args[1], true),
        "restore-trade" => set_trade_cancelled(db, &args[1], false),
        _ => {
            // `history [COUNT] [--failed]`, in either order.
            let (mut count, mut show_failed) = (DEFAULT_HISTORY, false);
            for arg in &args[1..] {
                match arg.as_str() {
                    "--failed" | "-f" => show_failed = true,
                    n => count = n.parse().ok()?,
                }
            }
            history(db, count, show_failed)
        }
    })
}

/// `game-folder [PATH]`: shows where Star Citizen is, or sets it. PATH can
/// be any level of the install, e.g. the folder chosen in the RSI Launcher.
fn game_folder(db: &Db, path: &str) -> ExitCode {
    if path.is_empty() {
        match game::find(db) {
            Some(live) => {
                println!("Star Citizen: {}", live.display());
                println!("  Game log:    {}", game::log_file(&live).display());
                println!("  Log backups: {}", game::backups_folder(&live).display());
                println!("{}", color::grey(format!("Change it with: {}", shell::command_hint("game-folder PATH"))));
            }
            None => println!("{}", color::amber(format!("Star Citizen wasn't found. {}", game::how_to_set()))),
        }
        return ExitCode::SUCCESS;
    }
    match game::resolve(Path::new(path)) {
        Some(live) => {
            game::save(db, &live);
            println!("Star Citizen: {}", live.display());
            println!("{}", color::grey("Saved. Restart tracking to follow this game's log."));
            ExitCode::SUCCESS
        }
        None => {
            eprintln!(
                "Star Citizen isn't in {path}. Give the folder you chose in the RSI Launcher \
                 (or the StarCitizen or LIVE folder inside it)."
            );
            ExitCode::FAILURE
        }
    }
}

fn db_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("STAR_CARGO_DB") {
        return Ok(path.into());
    }
    Ok(data_folder()?.join("star-cargo-tracker").join("trades.db"))
}

/// `%APPDATA%` on Windows.
#[cfg(windows)]
fn data_folder() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA").map(PathBuf::from).ok_or_else(|| "APPDATA is not set".to_string())
}

/// `$XDG_DATA_HOME`, usually `~/.local/share`, elsewhere.
#[cfg(not(windows))]
fn data_folder() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(dir.into());
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(Path::new(&home).join(".local").join("share"))
}

fn open_db() -> Result<Db, String> {
    let path = db_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Follows the log in the background, with the `cargo>` prompt in front for
/// running other commands meanwhile.
fn track(log: PathBuf, with_overlay: bool) -> ExitCode {
    live::install_ctrlc_handler();
    let overlay_starts = overlay::start_requests();
    // Sends trades to your UEX account in the background, if switched on.
    uex_sync::start_background(|report| {
        let notice = Notice::UexSync(report.clone());
        print_notice(&notice);
        overlay::forward(&notice);
    });

    let tracking = thread::spawn(move || {
        let db = match open_db() {
            Ok(db) => db,
            Err(e) => {
                eprintln!("Cannot open trade database: {e}");
                std::process::exit(1);
            }
        };
        live::run(&db, log, &mut |notice| {
            print_notice(&notice);
            overlay::forward(&notice);
        });
        overlay::tracking_finished();
        println!("Stopped watching.");
        // The prompt may still be waiting for a line that isn't coming (after
        // Ctrl+C), so end the program from here.
        std::process::exit(0);
    });

    thread::spawn(|| {
        if let shell::End::Exit = shell::run() {
            live::request_stop();
        }
    });

    if with_overlay {
        overlay::set_visible(true);
    }
    // The overlay window has to run on this (main) thread, so wait here until
    // it's first switched on. It then runs until tracking stops.
    if overlay_starts.recv().is_ok() {
        if let Err(e) = overlay::run_window() {
            eprintln!("Cannot show the overlay: {e}");
        }
    }
    let _ = tracking.join();
    ExitCode::SUCCESS
}

/// Shows a `watch` or `overlay` notice in the console.
/// Shows a `watch` or `overlay` notice in the console, above the prompt.
fn print_notice(notice: &Notice) {
    shell::print_background(&notice_text(notice));
}

fn notice_text(notice: &Notice) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    // Writing to a String can't fail.
    let _ = match notice {
        Notice::Watching(path) => writeln!(
            out,
            "{} {}\n{}\n{}",
            color::green("Watching"),
            path.display(),
            color::grey("Trades already in the database are not shown again."),
            color::grey("Type `help` for commands (Tab completes them), `exit` or Ctrl+C to stop."),
        ),
        Notice::WaitingForLog(path) => writeln!(
            out,
            "{}",
            color::amber(format!("Waiting for {} to appear...", path.display()))
        ),
        Notice::LogFound => writeln!(out, "{}", color::green("Found the log.")),
        Notice::NewSession => writeln!(out, "{}", color::magenta("--- new game session ---")),
        Notice::ShipNamed { ship_id, class, name } => writeln!(
            out,
            "{}",
            color::grey(format!(
                "New {} ({ship_id}) named \"{name}\", as it most likely replaces the destroyed one. \
                 Change it with name-ship.",
                ships::class_name(class)
            ))
        ),
        Notice::Stopping => writeln!(out, "{}", color::grey("Confirming the last trade before stopping...")),
        Notice::UexSync(report) => write!(out, "{}", sync_report_text(report)),
        Notice::Trade(trade) => {
            let line = TradeLine {
                id: trade.id,
                outcome: &trade.outcome,
                commodity: &trade.commodity,
                place: &trade.place,
                ship: trade.ship.as_deref(),
                cancelled: false,
            };
            let _ = writeln!(out, "{line}");
            let _ = match &trade.sale {
                Some(SaleResult::Profit { profit, margin }) => writeln!(
                    out,
                    "    profit {}",
                    color::money(
                        *profit,
                        format!("{} aUEC{}", format::signed_auec(*profit), format::margin(*margin))
                    )
                ),
                Some(SaleResult::Unknown) => writeln!(
                    out,
                    "    {}",
                    color::grey("no recorded purchase for this cargo, so profit is unknown")
                ),
                None => Ok(()),
            };
            match &trade.market {
                Some(note) => writeln!(out, "    {}", market_text(note)),
                None => Ok(()),
            }
        }
        Notice::ShipDestroyed(loss) => {
            let when = color::grey(loss.timestamp.format("%Y-%m-%d %H:%M:%S"));
            if loss.lost.is_empty() {
                writeln!(
                    out,
                    "{when}  {}",
                    color::red(format!("{} destroyed (no tracked cargo aboard)", loss.ship_name))
                )
            } else {
                let _ = writeln!(
                    out,
                    "{when}  {}",
                    color::red(format!("{} destroyed, writing off its cargo:", loss.ship_name))
                );
                for cargo in &loss.lost {
                    let _ = writeln!(
                        out,
                        "    {} {:>8.2} SCU {} {}",
                        color::grey(format!("#{:<3}", cargo.write_off_id)),
                        cargo.scu,
                        color::bold(format!("{:<24}", cargo.commodity)),
                        color::red(format!("lost {} aUEC", format::auec(cargo.cost)))
                    );
                }
                writeln!(out, "    {}", color::grey("Undo one with: delete-write-off ID"))
            }
        }
    };
    out
}

/// A UEX market note, coloured by whether it's good news.
fn market_text(note: &uex::MarketNote) -> String {
    let (text, tone) = note.describe();
    match tone {
        uex::Tone::Good => color::green(text),
        uex::Tone::Bad => color::amber(text),
        uex::Tone::Neutral => color::grey(text),
    }
}

fn import(db: &Db, paths: &[String]) -> ExitCode {
    let mut files = Vec::new();
    for path in paths {
        if let Err(e) = collect_logs(Path::new(path), &mut files) {
            eprintln!("Cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    }

    let mut names = Names::new(db);
    let (mut added, mut skipped) = (0, 0);
    for file in &files {
        let contents = match fs::read(file) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                eprintln!("Cannot read {}: {e}", file.display());
                return ExitCode::FAILURE;
            }
        };

        // Name the file only if something new came from it.
        let mut header_shown = false;
        let mut show_header = || {
            if !header_shown {
                println!("== {} ==", file.display());
                header_shown = true;
            }
        };

        // Each log file is a separate game session.
        let mut tracker = Tracker::default();
        let mut outcomes = Vec::new();
        for line in contents.lines() {
            outcomes.extend(tracker.feed(line));
        }
        outcomes.extend(tracker.flush());
        // First, so the trades below can show their ship's type.
        live::save_ships_seen(db, tracker.take_ships_seen(), &mut |notice| {
            show_header();
            print_notice(&notice)
        });

        for outcome in &outcomes {
            match db.record(outcome) {
                Ok(Some(id)) => {
                    show_header();
                    print_trade(id, outcome, false, db, &mut names);
                    added += 1;
                }
                Ok(None) => skipped += 1,
                Err(e) => {
                    eprintln!("Cannot save trade: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        // After the file's trades are saved, so cargo bought before the loss is known.
        live::handle_ships_destroyed(db, tracker.take_destroyed(), &mut |notice| {
            show_header();
            print_notice(&notice)
        });
    }

    println!();
    println!(
        "Read {} log file{}. {} {}",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        color::green(format!("Recorded {added} new trades")),
        color::grey(format!("({skipped} already in the database).")),
    );
    ExitCode::SUCCESS
}

fn history(db: &Db, count: usize, show_failed: bool) -> ExitCode {
    let result = db.recent(count, show_failed).and_then(|trades| Ok((trades, db.totals()?)));
    let (trades, totals) = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Cannot read trade database: {e}");
            return ExitCode::FAILURE;
        }
    };

    if trades.is_empty() && totals.failed == 0 {
        println!("No trades recorded yet. Run `watch` or `import` first.");
        return ExitCode::SUCCESS;
    }
    let mut names = Names::new(db);
    for trade in &trades {
        print_trade(trade.id, &trade.outcome, trade.cancelled, db, &mut names);
    }

    println!();
    let cancelled = if totals.cancelled > 0 {
        format!(", {} cancelled", totals.cancelled)
    } else {
        String::new()
    };
    let failed = if show_failed || totals.failed == 0 {
        format!("{} failed", totals.failed)
    } else {
        format!("{} failed, hidden: use `history --failed` to show them", totals.failed)
    };
    println!(
        "{} {}",
        color::bold("All recorded trades"),
        color::grey(format!("({} completed, {failed}{cancelled}):", totals.confirmed))
    );
    let net = totals.earned - totals.spent;
    println!("  Spent:  {}", color::red(format!("{:>15} aUEC", format::auec(totals.spent))));
    println!("  Earned: {}", color::green(format!("{:>15} aUEC", format::auec(totals.earned))));
    println!(
        "  Net:    {}",
        color::bold(color::money(net, format!("{:>15} aUEC", format::signed_auec(net))))
    );
    ExitCode::SUCCESS
}

fn load_report(db: &Db) -> rusqlite::Result<profit::Report> {
    Ok(profit::calculate(&db.completed_trades()?, &db.write_offs()?))
}

/// `export [PATH]`: saves everything to an Excel workbook.
fn export(db: &Db, path: &str) -> ExitCode {
    let path = if path.is_empty() { export::default_path() } else { export::resolve_path(Path::new(path)) };
    match export::export(db, &path) {
        Ok(done) => {
            println!(
                "Saved {} trades, {} sales and {} write-offs to {}",
                done.trades,
                done.sales,
                done.write_offs,
                color::bold(done.path.display())
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn show_profit(db: &Db) -> ExitCode {
    let report = match load_report(db) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("Cannot read trade database: {e}");
            return ExitCode::FAILURE;
        }
    };
    if report.sales.is_empty() && report.holdings.is_empty() && report.losses.is_empty() {
        println!("No completed trades recorded yet. Run `watch` or `import` first.");
        return ExitCode::SUCCESS;
    }

    let mut names = Names::new(db);
    if !report.sales.is_empty() {
        println!("{}", color::bold("Sales:"));
        for sale in &report.sales {
            let sell = &sale.sell;
            println!(
                "  {}  {:>8.2} SCU {} {} {}",
                color::grey(sell.timestamp.format("%Y-%m-%d %H:%M:%S")),
                sell.scu,
                color::bold(format!("{:<24}", names.get(&sell.resource_guid))),
                color::green("sold at"),
                locations::trade_place(db, sell),
            );
            match sale.profit() {
                Some(profit) => {
                    let mut bought_at: Vec<String> = Vec::new();
                    for (buy, _) in &sale.sources {
                        let place = locations::trade_place(db, buy);
                        if !bought_at.contains(&place) {
                            bought_at.push(place);
                        }
                    }
                    let held = sale
                        .first_bought()
                        .map(|t| format!(", held {}", format::duration(sell.timestamp - t)))
                        .unwrap_or_default();
                    println!(
                        "      {} {}{}",
                        color::blue("bought at"),
                        bought_at.join(", "),
                        color::grey(held)
                    );
                    println!(
                        "      cost {} -> revenue {} = profit {}",
                        color::red(format::auec(sale.cost)),
                        color::green(format::auec(sale.matched_revenue())),
                        color::bold(color::money(
                            profit,
                            format!("{}{}", format::signed_auec(profit), format::margin(sale.margin()))
                        )),
                    );
                }
                None => println!("      {}", color::grey("no recorded purchase, so profit is unknown")),
            }
            if sale.profit().is_some() && sale.unmatched_scu() > 0.0 {
                println!(
                    "      {}",
                    color::grey(format!(
                        "{:.2} SCU had no recorded purchase ({} aUEC not counted)",
                        sale.unmatched_scu(),
                        format::auec(sale.unmatched_revenue()),
                    ))
                );
            }
        }
        println!();
    }

    if !report.losses.is_empty() {
        println!("{}", color::bold("Written off:"));
        for loss in &report.losses {
            let w = &loss.write_off;
            let note = w.note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default();
            println!(
                "  {} {}  {:>8.2} SCU {} {}{}",
                color::grey(format!("#{:<3}", w.id)),
                color::grey(w.timestamp.format("%Y-%m-%d %H:%M:%S")),
                w.scu,
                color::bold(format!("{:<24}", names.get(&w.resource_guid))),
                color::red(format!("lost {} aUEC", format::auec(loss.cost))),
                color::grey(note),
            );
        }
        println!();
    }

    // What unsold cargo would fetch at the best known prices (from UEX).
    let (mut unsold_cost, mut unsold_value, mut unvalued) = (0.0, 0.0, 0);
    if !report.holdings.is_empty() {
        println!("{}", color::bold("Cargo bought but not sold yet:"));
        for lot in &report.holdings {
            let commodity = names.get(&lot.buy.resource_guid);
            let on_ship = lot
                .buy
                .ship_id
                .as_deref()
                .map(|id| format!(", on {}", color::cyan(ships::label_for(db, id))))
                .unwrap_or_default();
            println!(
                "  {}  {:>8.2} SCU {} {} {} for {}{on_ship}",
                color::grey(lot.buy.timestamp.format("%Y-%m-%d %H:%M:%S")),
                lot.scu_left,
                color::bold(format!("{commodity:<24}")),
                color::blue("bought at"),
                locations::trade_place(db, &lot.buy),
                color::red(format!("{} aUEC", format::auec(lot.cost_left()))),
            );

            // Prefer buyers in the system it was bought in.
            let system = uex::terminal_for(db, &lot.buy).map(|t| t.system);
            match uex::best_sell(db, &commodity, system.as_deref()) {
                Some(best) => {
                    let value = best.price * lot.scu_left;
                    let gain = value - lot.cost_left();
                    unsold_cost += lot.cost_left();
                    unsold_value += value;
                    println!(
                        "      {} {} {}",
                        color::grey("worth about"),
                        color::money(gain, format!("{} aUEC ({})", format::auec(value), format::signed_auec(gain))),
                        color::grey(format!(
                            "at {} ({}, {}, UEX)",
                            best.terminal,
                            best.system,
                            format::age(best.updated)
                        )),
                    );
                }
                None => {
                    unvalued += 1;
                    println!("      {}", color::grey("no known buyers on UEX"));
                }
            }
        }
        println!();
    }

    let signed = |amount: f64| color::money(amount, format!("{} aUEC", format::signed_auec(amount)));
    if !report.losses.is_empty() {
        println!("Profit from sales: {}", signed(report.sales_profit()));
        println!("Lost cargo:        {}", signed(-report.total_losses()));
    }
    println!("{} {}", color::bold("Total profit:"), color::bold(signed(report.total_profit())));
    if unsold_value > 0.0 {
        let gain = unsold_value - unsold_cost;
        println!(
            "{} {} {}",
            color::grey("Unsold cargo, if sold at the best known prices:"),
            signed(gain),
            color::grey(format!(
                "more ({} aUEC for cargo that cost {}){}",
                format::auec(unsold_value),
                format::auec(unsold_cost),
                if unvalued > 0 { format!("; {unvalued} lot(s) have no known buyer") } else { String::new() }
            )),
        );
    }
    let unmatched = report.unmatched_revenue();
    if unmatched > 0.0 {
        println!(
            "{}",
            color::grey(format!(
                "Plus {} aUEC from cargo with no recorded purchase (mined, salvaged, or bought before tracking).",
                format::auec(unmatched)
            ))
        );
    }
    ExitCode::SUCCESS
}

fn write_off(db: &Db, commodity: &str, amount: &str, note: Option<&str>) -> ExitCode {
    let report = match load_report(db) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("Cannot read trade database: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Total SCU held per commodity, in the order first bought.
    let mut names = Names::new(db);
    let mut held: Vec<(String, String, f64)> = Vec::new();
    for lot in &report.holdings {
        match held.iter_mut().find(|(guid, _, _)| *guid == lot.buy.resource_guid) {
            Some(entry) => entry.2 += lot.scu_left,
            None => held.push((
                lot.buy.resource_guid.clone(),
                names.get(&lot.buy.resource_guid),
                lot.scu_left,
            )),
        }
    }
    if held.is_empty() {
        eprintln!("You aren't holding any bought cargo, so there's nothing to write off.");
        return ExitCode::FAILURE;
    }

    // Exact name or ID first, then partial name matches.
    let wanted = commodity.to_lowercase();
    let mut matches: Vec<_> = held
        .iter()
        .filter(|(guid, name, _)| name.to_lowercase() == wanted || guid.starts_with(&wanted))
        .collect();
    if matches.is_empty() {
        matches = held
            .iter()
            .filter(|(_, name, _)| name.to_lowercase().contains(&wanted))
            .collect();
    }
    let (guid, name, held_scu) = match matches[..] {
        [one] => one,
        [] => {
            eprintln!("You aren't holding any cargo matching \"{commodity}\".");
            print_held(&held);
            return ExitCode::FAILURE;
        }
        _ => {
            eprintln!("\"{commodity}\" matches more than one commodity:");
            print_held(&matches.into_iter().cloned().collect::<Vec<_>>());
            return ExitCode::FAILURE;
        }
    };

    let scu = if amount.eq_ignore_ascii_case("all") {
        *held_scu
    } else {
        match amount.parse::<f64>() {
            Ok(scu) if scu > 0.0 && scu.is_finite() => scu,
            _ => {
                eprintln!("SCU must be a positive number or \"all\", not \"{amount}\".");
                return ExitCode::FAILURE;
            }
        }
    };
    if scu > held_scu + 1e-6 {
        eprintln!("You only hold {held_scu:.2} SCU of {name}.");
        return ExitCode::FAILURE;
    }

    let cost: f64 = {
        // Lost cargo comes from the oldest purchases, like a sale.
        let mut left = scu;
        let mut cost = 0.0;
        for lot in report.holdings.iter().filter(|l| l.buy.resource_guid == *guid) {
            let used = lot.scu_left.min(left);
            cost += lot.buy.total_price / lot.buy.scu * used;
            left -= used;
        }
        cost
    };

    match db.add_write_off(Utc::now(), guid, scu, note, None) {
        Ok(id) => {
            println!(
                "{}",
                color::red(format!(
                    "Wrote off {scu:.2} SCU of {name} (cost {} aUEC) as write-off #{id}.",
                    format::auec(cost)
                ))
            );
            println!("Undo with: {}", shell::command_hint(&format!("delete-write-off {id}")));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Cannot save write-off: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_held(held: &[(String, String, f64)]) {
    eprintln!("Cargo you're holding:");
    for (_, name, scu) in held {
        eprintln!("  {scu:>8.2} SCU  {name}");
    }
}

fn delete_write_off(db: &Db, id: &str) -> ExitCode {
    let Ok(id) = id.trim_start_matches('#').parse::<i64>() else {
        eprintln!("Write-off ID must be a number, e.g. 3.");
        return ExitCode::FAILURE;
    };
    match db.delete_write_off(id) {
        Ok(true) => {
            println!("Deleted write-off #{id}.");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            eprintln!("There is no write-off #{id}. Run `profit` to see them.");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("Cannot delete write-off: {e}");
            ExitCode::FAILURE
        }
    }
}

fn set_trade_cancelled(db: &Db, id: &str, cancel: bool) -> ExitCode {
    let Ok(id) = id.trim_start_matches('#').parse::<i64>() else {
        eprintln!("Trade ID must be a number, e.g. 12.");
        return ExitCode::FAILURE;
    };
    let trade = match db.trade(id) {
        Ok(Some(trade)) => trade,
        Ok(None) => {
            eprintln!("There is no trade #{id}. Run `history` to see trade IDs.");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("Cannot read trade database: {e}");
            return ExitCode::FAILURE;
        }
    };
    if matches!(trade.outcome, Outcome::Failed(..)) {
        eprintln!("Trade #{id} failed in the game, so it's already not counted.");
        return ExitCode::FAILURE;
    }

    match db.set_cancelled(id, cancel) {
        Ok(changed) => {
            let mut names = Names::new(db);
            print_trade(id, &trade.outcome, cancel, db, &mut names);
            println!(
                "{}",
                match (cancel, changed) {
                    (true, true) => color::amber("Cancelled: no longer counted in profit or cargo."),
                    (true, false) => color::grey("Already cancelled."),
                    (false, true) => color::green("Restored: counted again."),
                    (false, false) => color::grey("Wasn't cancelled."),
                }
            );
            if cancel && changed {
                println!("Undo with: {}", shell::command_hint(&format!("restore-trade {id}")));
            }
            if changed {
                // Reverse (or re-send) it on your UEX account, if syncing.
                uex_sync::nudge();
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Cannot update trade: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `prices COMMODITY`: where it's cheapest to buy and best to sell, from UEX.
fn show_prices(db: &Db, wanted: &str) -> ExitCode {
    let all = uex::commodity_names(db);
    if all.is_empty() {
        eprintln!("No UEX prices yet: UEX couldn't be reached and nothing is saved.");
        return ExitCode::FAILURE;
    }
    let wanted_lower = wanted.to_lowercase();
    let mut matches: Vec<&String> = all.iter().filter(|n| n.to_lowercase() == wanted_lower).collect();
    if matches.is_empty() {
        matches = all.iter().filter(|n| n.to_lowercase().contains(&wanted_lower)).collect();
    }
    let commodity = match matches[..] {
        [one] => one,
        [] => {
            eprintln!("UEX has no commodity matching \"{wanted}\".");
            return ExitCode::FAILURE;
        }
        _ => {
            eprintln!("\"{wanted}\" matches several commodities:");
            for name in matches.iter().take(20) {
                eprintln!("  {name}");
            }
            return ExitCode::FAILURE;
        }
    };

    let rows = uex::market(db, commodity);
    let mut buy: Vec<_> = rows.iter().filter(|r| r.price_buy > 0.0).collect();
    buy.sort_by(|a, b| a.price_buy.total_cmp(&b.price_buy));
    let mut sell: Vec<_> = rows.iter().filter(|r| r.price_sell > 0.0).collect();
    sell.sort_by(|a, b| b.price_sell.total_cmp(&a.price_sell));

    println!("{} {}", color::bold(commodity), color::grey("(UEX prices per SCU)"));
    println!();
    println!("{}", color::blue(color::bold("Cheapest to buy:")));
    if buy.is_empty() {
        println!("  {}", color::grey("nowhere known"));
    }
    for row in buy.iter().take(10) {
        println!(
            "  {} {}  {:<44} {} {}",
            color::red(format!("{:>10}", format::auec(row.price_buy))),
            color::grey(format!("{:>9} SCU", format::auec(row.scu_buy))),
            row.terminal,
            color::cyan(format!("{:<8}", row.system)),
            color::grey(format::age(row.updated)),
        );
    }
    println!();
    println!("{}", color::green(color::bold("Best to sell:")));
    if sell.is_empty() {
        println!("  {}", color::grey("nowhere known"));
    }
    for row in sell.iter().take(10) {
        println!(
            "  {}  {:<58} {} {}",
            color::green(format!("{:>10}", format::auec(row.price_sell))),
            row.terminal,
            color::cyan(format!("{:<8}", row.system)),
            color::grey(format::age(row.updated)),
        );
    }
    println!();
    println!("{}", color::grey("Crowd-sourced by UEX; check the terminal before committing."));
    ExitCode::SUCCESS
}

/// `routes [FROM] [--scu N]`: the most profitable runs from a location.
/// Returns `None` for malformed arguments.
fn show_routes(db: &Db, args: &[String]) -> Option<ExitCode> {
    let mut scu: Option<f64> = None;
    let mut budget: Option<f64> = None;
    let mut from = Vec::new();
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if word == "--scu" {
            scu = Some(words.next()?.parse().ok().filter(|n: &f64| *n > 0.0)?);
        } else if word == "--budget" || word == "--spend" {
            let Some(amount) = words.next().and_then(|w| format::parse_auec(w)) else {
                eprintln!("--budget needs an amount of aUEC, e.g. --budget 250000 or --budget 250k.");
                return Some(ExitCode::FAILURE);
            };
            budget = Some(amount);
        } else {
            from.push(word.as_str());
        }
    }
    let from = from.join(" ");

    // Where from: what was asked for, else where you are.
    let (origins, place) = if from.is_empty() {
        let code = live::current_location().or_else(|| db.last_location().ok().flatten());
        let Some(code) = code else {
            eprintln!("I don't know where you are yet. Try `routes LOCATION`, e.g. routes Levski.");
            return Some(ExitCode::FAILURE);
        };
        let place = locations::display_name(db, &code);
        let origins = uex::terminals_at(db, &code);
        if origins.is_empty() {
            eprintln!(
                "UEX has no commodity terminal matching {place} ({code}). Find it with \
                 `terminals NAME`, then link it with `link-terminal {code} ID`."
            );
            return Some(ExitCode::FAILURE);
        }
        (origins, place)
    } else {
        let found = uex::search_terminals(db, &from);
        let mut places: Vec<String> = Vec::new();
        for t in &found {
            if !places.contains(&t.place) {
                places.push(t.place.clone());
            }
        }
        let exact = places.iter().find(|p| p.eq_ignore_ascii_case(&from)).cloned();
        let place = match (exact, places.len()) {
            (Some(place), _) => place,
            (None, 1) => places[0].clone(),
            (None, 0) => {
                eprintln!("UEX has no commodity terminal matching \"{from}\".");
                return Some(ExitCode::FAILURE);
            }
            (None, _) => {
                eprintln!("\"{from}\" matches several places; be more specific:");
                for p in places.iter().take(15) {
                    eprintln!("  {p}");
                }
                return Some(ExitCode::FAILURE);
            }
        };
        (found.into_iter().filter(|t| t.place == place).collect(), place)
    };

    // How much you can carry: as given, else the ship you were last in.
    let (capacity, carrying) = match scu {
        Some(scu) => (Some(scu), format!("{} SCU", format::scu(scu))),
        None => {
            let ship = db.ships().unwrap_or_default().into_iter().find_map(|ship| {
                if ship.destroyed_at.is_some() {
                    return None;
                }
                let scu = uex::capacity(db, ship.class.as_deref()?)?;
                Some((scu, ships::label(&ship)))
            });
            match ship {
                Some((scu, name)) => (Some(scu), format!("{} SCU ({name})", format::scu(scu))),
                None => (None, "the full stock on offer (use --scu N for your cargo space)".into()),
            }
        }
    };

    let mut routes = match uex::routes_from(db, &origins) {
        Ok(routes) => routes,
        Err(e) => {
            eprintln!("Cannot get routes from UEX: {e}");
            return Some(ExitCode::FAILURE);
        }
    };
    let load = |r: &uex::Route| r.load(capacity, budget);
    let profit = |r: &uex::Route| (r.sell - r.buy) * load(r);
    let any_on_offer = !routes.is_empty();
    routes.retain(|r| load(r) > 0.0);
    routes.sort_by(|a, b| profit(b).total_cmp(&profit(a)));
    // Only the best place to sell each commodity, so one doesn't fill the list.
    let mut listed: Vec<String> = Vec::new();
    routes.retain(|r| {
        let first = !listed.contains(&r.commodity);
        listed.push(r.commodity.clone());
        first
    });

    let system = origins.first().map(|t| t.system.clone()).unwrap_or_default();
    let spending = budget.map(|b| format!(", spending up to {} aUEC", format::auec(b))).unwrap_or_default();
    println!(
        "{} {} {}",
        color::bold(format!("Routes from {place}")),
        color::cyan(format!("({system})")),
        color::grey(format!("for {carrying}{spending}:")),
    );
    if routes.is_empty() {
        let why = match budget {
            Some(b) if any_on_offer => format!("{} aUEC doesn't buy a full SCU of anything profitable here.", format::auec(b)),
            _ => "UEX knows no profitable routes from here.".to_string(),
        };
        println!("  {}", color::grey(why));
        return Some(ExitCode::SUCCESS);
    }
    // With a budget, how much each run buys and costs differs, so show it.
    let budget_columns = |scu: &str, cost: &str| {
        if budget.is_some() {
            format!(" {scu:>7} {cost:>11}")
        } else {
            String::new()
        }
    };
    println!(
        "{}",
        color::bold(format!(
            "  {:<28} {:>9}  {:<38} {:>9} {:>9}{} {:>12} {:>7}",
            "COMMODITY",
            "BUY/SCU",
            "SELL AT",
            "SELL/SCU",
            "+/SCU",
            budget_columns("SCU", "COST"),
            "RUN PROFIT",
            "DIST"
        ))
    );
    for route in routes.iter().take(12) {
        // Some names already say the system, e.g. "Stanton Gateway (Pyro)".
        let destination = if route.destination_system.eq_ignore_ascii_case(&system)
            || route.destination.ends_with(&format!("({})", route.destination_system))
        {
            route.destination.clone()
        } else {
            format!("{} ({})", route.destination, route.destination_system)
        };
        // Less on offer than your cargo space and budget would otherwise take.
        let room = [capacity, budget.map(|b| (b / route.buy).floor())]
            .into_iter()
            .flatten()
            .fold(f64::INFINITY, f64::min);
        let short_load = room.is_finite() && route.stock < room;
        let scu_and_cost = color::grey(budget_columns(
            &format::scu(load(route)),
            &format::auec(load(route) * route.buy),
        ));
        println!(
            "  {} {}  {:<38} {} {}{scu_and_cost} {} {}{}",
            color::bold(format!("{:<28}", route.commodity)),
            color::red(format!("{:>9}", format::auec(route.buy))),
            destination,
            color::green(format!("{:>9}", format::auec(route.sell))),
            color::green(format!("{:>9}", format::auec(route.sell - route.buy))),
            color::green(color::bold(format!("{:>12}", format::signed_auec(profit(route))))),
            color::grey(format!("{:>4.0} GM", route.distance)),
            if short_load {
                color::amber(format!("  only {} SCU on offer", format::scu(route.stock)))
            } else {
                String::new()
            },
        );
    }
    println!();
    println!(
        "{}",
        color::grey(
            "The best place to sell each commodity. Prices from UEX (crowd-sourced); check the \
             terminals before committing. `prices COMMODITY` lists other buyers."
        )
    );
    Some(ExitCode::SUCCESS)
}

/// `terminals [SEARCH]`: UEX commodity terminals and their IDs, for
/// `link-terminal`. Without SEARCH, the ones matched to your trade locations.
fn show_terminals(db: &Db, search: &str) -> ExitCode {
    if search.is_empty() {
        let locations = db.trade_locations().unwrap_or_default();
        if locations.is_empty() {
            println!("No trade locations yet. Search UEX with `terminals NAME`.");
            return ExitCode::SUCCESS;
        }
        println!("{}", color::bold("UEX terminals for your trade locations:"));
        let mut unmatched = None;
        for (code, _, _) in &locations {
            let found = uex::terminals_at(db, code);
            let name = locations::display_name(db, code);
            let terminals = if found.is_empty() {
                unmatched.get_or_insert_with(|| name.clone());
                color::amber("none found")
            } else {
                found
                    .iter()
                    .map(|t| format!("{} {}", color::grey(format!("#{}", t.id)), t.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            println!("  {name:<30} {} {terminals}", color::grey(format!("{code:<34}")));
        }
        if let Some(name) = unmatched {
            println!();
            println!(
                "{}",
                color::grey(format!(
                    "Find a terminal with `terminals NAME`, then link it, e.g. {}",
                    shell::command_hint(&format!("link-terminal \"{name}\" ID"))
                ))
            );
        }
        return ExitCode::SUCCESS;
    }

    let found = uex::search_terminals(db, search);
    if found.is_empty() {
        println!("No UEX commodity terminal matches \"{search}\".");
        return ExitCode::SUCCESS;
    }
    for t in found.iter().take(40) {
        println!(
            "  {} {:<50} {}",
            color::grey(format!("{:>5}", t.id)),
            t.name,
            color::cyan(&t.system)
        );
    }
    println!();
    println!("{}", color::grey("Link one to a location with: link-terminal LOCATION ID"));
    ExitCode::SUCCESS
}

/// `link-terminal LOCATION ID`, where LOCATION is a trade location's name as
/// `terminals` shows it, or its code.
fn link_terminal(db: &Db, location: &str, id: &str) -> ExitCode {
    let Ok(id) = id.trim_start_matches('#').parse::<i64>() else {
        eprintln!("Terminal ID must be a number; find it with `terminals NAME`.");
        return ExitCode::FAILURE;
    };
    let Some(code) = locations::find(db, location) else {
        eprintln!(
            "\"{location}\" isn't one of your trade locations. Use a name as `terminals` shows it, e.g. {}",
            shell::command_hint("link-terminal \"Port Tressler\" 66")
        );
        return ExitCode::FAILURE;
    };
    match uex::link_terminal(db, &code, id) {
        Ok(Some(terminal)) => {
            // Many names already end with the system, e.g. "Terra Gateway (Stanton)".
            let system = if terminal.name.contains(&terminal.system) {
                String::new()
            } else {
                format!(" ({})", terminal.system)
            };
            println!(
                "{} is now {}{system} for UEX prices.",
                locations::display_name(db, &code),
                terminal.name,
            );
            ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("UEX has no commodity terminal #{id}. Find IDs with `terminals NAME`.");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("Cannot save the link: {e}");
            ExitCode::FAILURE
        }
    }
}

/// What a run of sending to your UEX account did, for the console.
fn sync_report_text(report: &uex_sync::Report) -> String {
    let mut out = String::new();
    if report.sent + report.reversed > 0 {
        let mut parts = Vec::new();
        if report.sent > 0 {
            parts.push(format!(
                "sent {} entr{} for {}",
                report.sent,
                if report.sent == 1 { "y" } else { "ies" },
                format::trade_ids(&report.sent_trades)
            ));
        }
        if report.reversed > 0 {
            parts.push(format!("reversed {} for cancelled {}", report.reversed, format::trade_ids(&report.reversed_trades)));
        }
        let test = if report.test { color::grey(" (test mode: nothing recorded)") } else { String::new() };
        out.push_str(&format!("{} {}{test}\n", color::green("UEX account:"), parts.join(", ")));
    }
    for rejected in &report.rejected {
        // Per-trade rejections start with the trade's ID; account problems don't.
        if rejected.starts_with('#') {
            out.push_str(&format!("{} {rejected}\n", color::amber("UEX rejected")));
        } else {
            out.push_str(&format!("{}\n", color::amber(rejected)));
        }
    }
    if let Some(why) = report.offline.as_deref().filter(|w| *w != "waiting to retry") {
        out.push_str(&format!("{}\n", color::amber(format!("UEX account: can't reach UEX ({why}); will retry"))));
    }
    out
}

/// `uex ...`: sending your trades to your UEX account's wallet and trade
/// history. Returns `None` for an unknown subcommand.
fn uex_command(db: &Db, args: &[String]) -> Option<ExitCode> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    let value = args.get(1).map(String::as_str);
    let save = |key: &str, value: Option<&str>| {
        if let Err(e) = db.set_setting(key, value) {
            eprintln!("Cannot save setting: {e}");
            return false;
        }
        true
    };

    Some(match (sub, value) {
        ("status", None) => uex_status(db),
        ("secret", Some(key)) => {
            if key.len() != 40 {
                eprintln!("A UEX secret key is 40 characters; copy it from your profile on uexcorp.space.");
                return Some(ExitCode::FAILURE);
            }
            if !save("uex_secret", Some(key)) {
                return Some(ExitCode::FAILURE);
            }
            println!("Secret key saved (ending ...{}).", &key[36..]);
            uex_sync::mark_start(db);
            print_start_point(db);
            println!("{}", color::grey("Turn syncing on with: uex sync on"));
            ExitCode::SUCCESS
        }
        ("token", Some(token)) => {
            if !save("uex_token", Some(token)) {
                return Some(ExitCode::FAILURE);
            }
            println!("App token saved.");
            ExitCode::SUCCESS
        }
        ("forget", None) => {
            // The start point goes too, so a key added later starts from then.
            for key in ["uex_secret", "uex_token", "uex_sync", "uex_since", "uex_include_past"] {
                save(key, None);
            }
            println!("Removed your UEX secret key and saved app token, and turned syncing off.");
            if uex::app_token(db).is_some_and(|(_, source)| source == uex::TokenSource::BuiltIn) {
                println!("{}", color::grey("This copy's built-in app token stays; it doesn't identify you."));
            }
            ExitCode::SUCCESS
        }
        ("sync", Some("on")) => {
            if uex_sync::config(db).secret.is_none() {
                eprintln!("Set your UEX secret key first: uex secret KEY (it's on your UEX profile page).");
                return Some(ExitCode::FAILURE);
            }
            save("uex_sync", Some("on"));
            if db.setting("uex_live").ok().flatten().is_none() {
                save("uex_live", Some("0"));
            }
            uex_sync::mark_start(db);
            print_start_point(db);
            if uex_sync::config(db).live {
                println!("{}", color::green("Syncing on, LIVE: sending real wallet and trade-history entries."));
            } else {
                println!(
                    "{} {}",
                    color::green("Syncing on, in test mode:"),
                    "UEX checks each entry but records nothing real. Switch to real entries with `uex live`."
                );
            }
            send_now(db, false)
        }
        ("sync", Some("off")) => {
            save("uex_sync", Some("off"));
            println!("Syncing off. Nothing more is sent until `uex sync on`.");
            ExitCode::SUCCESS
        }
        ("live", None) => {
            save("uex_live", Some("1"));
            println!(
                "{} trades from when syncing started (and earlier ones, if you chose `uex send-past`) \
                 go to your real UEX wallet and trade history, including any sent in test mode.",
                color::amber("Live:")
            );
            if uex_sync::config(db).enabled {
                send_now(db, false)
            } else {
                println!("Syncing is off; turn it on with `uex sync on`.");
                ExitCode::SUCCESS
            }
        }
        ("test", None) => {
            save("uex_live", Some("0"));
            println!("Test mode: UEX checks entries but records nothing real.");
            ExitCode::SUCCESS
        }
        ("wallet" | "history", Some(state @ ("on" | "off"))) => {
            save(&format!("uex_{sub}"), Some(state));
            let place = if sub == "wallet" { "wallet" } else { "trade history" };
            println!("Sending to your UEX {place}: {state}.");
            if state == "on" {
                uex_sync::nudge();
            }
            ExitCode::SUCCESS
        }
        ("push", None) => send_now(db, true),
        ("send-past", confirm) => {
            if uex_sync::config(db).include_past {
                println!("Earlier trades are already included.");
                return Some(ExitCode::SUCCESS);
            }
            let held = uex_sync::held_back(db);
            if held == 0 {
                println!("There are no earlier trades to send.");
                return Some(ExitCode::SUCCESS);
            }
            if confirm != Some("yes") {
                println!(
                    "{held} completed trade{} from before syncing started will be sent to your UEX wallet \
                     and trade history.",
                    if held == 1 { "" } else { "s" }
                );
                println!(
                    "{}",
                    color::amber(
                        "UEX can't be told when a trade happened, so they'll all show as happening when \
                         they're sent (in the right order). To remove them later, delete them by hand on the UEX site."
                    )
                );
                println!("To go ahead: uex send-past yes");
                return Some(ExitCode::SUCCESS);
            }
            save("uex_include_past", Some("1"));
            println!("Earlier trades included.");
            send_now(db, false)
        }
        _ => return None,
    })
}

/// Says which trades will be sent, after saving a key or switching on.
fn print_start_point(db: &Db) {
    let held = uex_sync::held_back(db);
    if held > 0 {
        println!(
            "Only trades from now on will be sent. {}",
            color::grey(format!(
                "To also send your {held} earlier trade{}: uex send-past",
                if held == 1 { "" } else { "s" }
            ))
        );
    }
}

/// Sends what's waiting: through the background sender while tracking (so
/// there's only ever one sender), otherwise right here until done.
fn send_now(db: &Db, force: bool) -> ExitCode {
    if !uex_sync::config(db).enabled {
        println!("Syncing is off; turn it on with `uex sync on`.");
        return ExitCode::SUCCESS;
    }
    if uex_sync::background_running() {
        if force {
            uex_sync::nudge_force();
        } else {
            uex_sync::nudge();
        }
        println!("{}", color::grey("Sending in the background; progress appears here."));
        return ExitCode::SUCCESS;
    }
    loop {
        let report = uex_sync::sync(db, force);
        print!("{}", sync_report_text(&report));
        if report.offline.is_some() {
            return ExitCode::FAILURE;
        }
        if !report.more {
            break;
        }
    }
    let counts = uex_sync::counts(db);
    let waiting: usize = counts.iter().map(|(_, _, waiting, _)| waiting).sum();
    let rejected: usize = counts.iter().map(|(_, _, _, rejected)| rejected).sum();
    if waiting == 0 && rejected == 0 {
        println!("{}", color::green("Everything is sent."));
    } else if rejected > 0 {
        println!(
            "{}",
            color::amber(format!("{rejected} entr{} rejected; see `uex` for why.", if rejected == 1 { "y" } else { "ies" }))
        );
    }
    ExitCode::SUCCESS
}

fn uex_status(db: &Db) -> ExitCode {
    let config = uex_sync::config(db);
    let on_off = |on: bool| if on { color::green("on") } else { color::grey("off") };
    println!("{}", color::bold("Syncing trades to your UEX account"));
    println!(
        "  Syncing:     {}{}",
        on_off(config.enabled),
        match (config.enabled, config.live) {
            (true, true) => format!(", {}", color::amber("LIVE (real entries)")),
            (true, false) => ", test mode (nothing real is recorded; `uex live` to switch)".to_string(),
            _ => String::new(),
        }
    );
    match &config.secret {
        Some(secret) => println!(
            "  Secret key:  set (ending ...{})",
            &secret[secret.len().saturating_sub(4)..]
        ),
        None => println!("  Secret key:  {}", color::amber("not set: uex secret KEY (from your UEX profile)")),
    }
    println!(
        "  App token:   {}",
        match config.token_source {
            Some(uex::TokenSource::Environment) => "set (STAR_CARGO_UEX_TOKEN)".to_string(),
            Some(uex::TokenSource::Saved) => "set (saved with uex token)".to_string(),
            Some(uex::TokenSource::BuiltIn) => "built into this copy of the program".to_string(),
            None => color::amber("not set: uex token TOKEN (make an app at uexcorp.space/api/apps)"),
        }
    );
    let held = uex_sync::held_back(db);
    println!(
        "  Sending:     {}",
        match (config.include_past, config.since) {
            (true, _) => "all completed trades, including earlier ones".to_string(),
            (false, Some(since)) => format!(
                "trades since {}{}",
                since.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                if held > 0 {
                    color::grey(format!(" ({held} earlier not sent: uex send-past)"))
                } else {
                    String::new()
                }
            ),
            (false, None) => "trades from when syncing is switched on".to_string(),
        }
    );
    for (target, sent, waiting, rejected) in uex_sync::counts(db) {
        let wanted = if target == "wallet" { config.wallet } else { config.history };
        let name = if target == "wallet" { "Wallet:" } else { "History:" };
        println!(
            "  {name:<12} {}, {sent} trade(s) sent, {waiting} waiting{}",
            on_off(wanted),
            if rejected > 0 { color::amber(format!(", {rejected} rejected")) } else { String::new() }
        );
    }
    if config.secret.is_some() {
        match uex_sync::wallet_balance(db) {
            Ok(balance) => println!("  UEX wallet:  {} aUEC", format::auec(balance)),
            Err(why) => println!("  UEX wallet:  {}", color::amber(format!("couldn't read balance: {why}"))),
        }
    }
    let rejections = uex_sync::rejections(db, 5);
    if !rejections.is_empty() {
        println!("  Rejected by UEX (fix, then `uex push` to retry):");
        for r in rejections {
            println!("    {}", color::amber(r));
        }
    }
    println!();
    println!(
        "{}",
        color::grey(
            "uex secret KEY | uex token TOKEN | uex sync on|off | uex live | uex test\n\
             uex wallet on|off | uex history on|off | uex push | uex forget"
        )
    );
    ExitCode::SUCCESS
}

/// `locations [--failed]`: trade locations and their names. Trades that
/// failed in the game aren't counted unless `show_failed`, which adds a
/// column for them (and lists places where every trade failed).
fn list_locations(db: &Db, show_failed: bool) -> ExitCode {
    let all = match db.trade_locations() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Cannot read trade database: {e}");
            return ExitCode::FAILURE;
        }
    };
    let failed_total: i64 = all.iter().map(|(_, _, failed)| failed).sum();
    let locations: Vec<_> = all
        .into_iter()
        .filter(|(_, completed, _)| show_failed || *completed > 0)
        .collect();
    if locations.is_empty() {
        println!("No trade locations recorded yet.");
        return ExitCode::SUCCESS;
    }

    let header = format!("{:<45} {:<30} {:>6}", "CODE", "NAME", "TRADES");
    if show_failed {
        println!("{}", color::bold(format!("{header} {:>6}", "FAILED")));
    } else {
        println!("{}", color::bold(header));
    }
    for (code, completed, failed) in &locations {
        let row = format!("{:<45} {:<30} {:>6}", code, locations::display_name(db, code), completed);
        if show_failed {
            let failed = format!("{failed:>6}");
            println!("{row} {}", if failed.trim() == "0" { color::grey(failed) } else { color::amber(failed) });
        } else {
            println!("{row}");
        }
    }
    println!();
    if !show_failed && failed_total > 0 {
        println!(
            "{}",
            color::grey(format!(
                "{failed_total} failed trade{} not counted: use `locations --failed` to show them.",
                if failed_total == 1 { "" } else { "s" }
            ))
        );
    }
    println!("Rename one with: {}", shell::command_hint("name-location CODE NAME"));
    ExitCode::SUCCESS
}

fn name_location(db: &Db, code: &str, name: &str) -> ExitCode {
    match db.save_location_name(code, name) {
        Ok(()) => {
            println!("{code} will be shown as \"{name}\".");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Cannot save location name: {e}");
            ExitCode::FAILURE
        }
    }
}

fn list_ships(db: &Db) -> ExitCode {
    let (ships, report) = match db.ships().and_then(|ships| Ok((ships, load_report(db)?))) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Cannot read trade database: {e}");
            return ExitCode::FAILURE;
        }
    };
    if ships.is_empty() {
        println!("No ships seen yet. Ships show up after you fly one or auto-load cargo into it.");
        return ExitCode::SUCCESS;
    }

    let mut names = Names::new(db);
    let (destroyed, active): (Vec<_>, Vec<_>) =
        ships.iter().partition(|ship| ship.destroyed_at.is_some());

    println!(
        "{}",
        color::bold(format!(
            "{:<14} {:<32} {:<24} {:>6}  {}",
            "ID", "TYPE", "NAME", "TRADES", "LAST SEEN"
        ))
    );
    for ship in &active {
        if let Err(e) = print_ship(db, ship, &report, &mut names) {
            eprintln!("Cannot read trade database: {e}");
            return ExitCode::FAILURE;
        }
    }
    if !destroyed.is_empty() {
        println!();
        println!(
            "{} {}",
            color::red(color::bold("Destroyed")),
            color::grey("(claimed ships come back with a new ID):")
        );
        for ship in &destroyed {
            if let Err(e) = print_ship(db, ship, &report, &mut names) {
                eprintln!("Cannot read trade database: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    println!();
    println!(
        "{}",
        color::grey(format!(
            "Only auto-loaded cargo is linked to a ship; cargo moved by hand isn't counted here.\n\
             Name one with: {}\n\
             A destroyed ship's name passes to the next new ship of its type (its likely replacement).",
            shell::command_hint("name-ship SHIP NAME")
        ))
    );
    ExitCode::SUCCESS
}

/// One ship in `ships`: its row, then what it carries (or lost) and has traded.
fn print_ship(
    db: &Db,
    ship: &db::Ship,
    report: &profit::Report,
    names: &mut Names,
) -> rusqlite::Result<()> {
    let when = match (ship.destroyed_at, ship.last_seen) {
        (Some(t), _) => color::red(format!("destroyed {}", t.format("%Y-%m-%d %H:%M"))),
        (None, Some(t)) => color::grey(t.format("%Y-%m-%d %H:%M")),
        (None, None) => color::grey("-"),
    };
    let class = match &ship.class {
        Some(class) => color::cyan(format!("{:<32}", ships::class_name(class))),
        None => color::grey(format!("{:<32}", "unknown")),
    };
    let name = match &ship.name {
        Some(name) => color::bold(format!("{name:<24}")),
        None => color::grey(format!("{:<24}", "-")),
    };
    println!(
        "{} {class} {name} {:>6}  {when}",
        color::grey(format!("{:<14}", ship.ship_id)),
        ship.trades,
    );

    if ship.destroyed_at.is_some() {
        // Cargo written off when it was destroyed.
        let lost: Vec<String> = report
            .losses
            .iter()
            .filter(|loss| loss.write_off.ship_id.as_deref() == Some(ship.ship_id.as_str()))
            .map(|loss| {
                format!(
                    "{} SCU {} ({} aUEC)",
                    format::scu(loss.write_off.scu),
                    names.get(&loss.write_off.resource_guid),
                    format::auec(loss.cost)
                )
            })
            .collect();
        println!(
            "{:15}{}    {}",
            "",
            color::grey("lost:"),
            if lost.is_empty() { color::grey("no tracked cargo") } else { color::red(lost.join(", ")) }
        );
    } else {
        // Cargo still aboard: bought onto this ship and not sold or lost.
        let mut aboard: Vec<(String, f64, f64)> = Vec::new();
        for lot in &report.holdings {
            if lot.buy.ship_id.as_deref() != Some(ship.ship_id.as_str()) {
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
        let aboard: Vec<String> = aboard
            .iter()
            .map(|(guid, scu, cost)| {
                format!("{} SCU {} (cost {})", format::scu(*scu), names.get(guid), format::auec(*cost))
            })
            .collect();
        println!(
            "{:15}{}  {}",
            "",
            color::grey("aboard:"),
            if aboard.is_empty() { color::grey("nothing tracked") } else { color::blue(aboard.join(", ")) }
        );
    }

    let traded: Vec<String> = db
        .ship_cargo_traded(&ship.ship_id)?
        .iter()
        .map(|(guid, bought, sold)| {
            let amounts = match (*bought > 0.0, *sold > 0.0) {
                (true, true) => {
                    format!("{} SCU bought, {} sold", format::scu(*bought), format::scu(*sold))
                }
                (true, false) => format!("{} SCU bought", format::scu(*bought)),
                _ => format!("{} SCU sold", format::scu(*sold)),
            };
            format!("{} ({amounts})", names.get(guid))
        })
        .collect();
    println!(
        "{:15}{}  {}",
        "",
        color::grey("traded:"),
        if traded.is_empty() { color::grey("nothing yet") } else { traded.join(", ") }
    );
    Ok(())
}

fn name_ship(db: &Db, wanted: &str, name: &str) -> ExitCode {
    let ships = match db.ships() {
        Ok(ships) => ships,
        Err(e) => {
            eprintln!("Cannot read trade database: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The full ID, else the end of an ID or part of the type or current name.
    let wanted_lower = wanted.to_lowercase();
    let mut matches: Vec<_> = ships.iter().filter(|s| s.ship_id == wanted).collect();
    if matches.is_empty() {
        matches = ships
            .iter()
            .filter(|s| {
                s.ship_id.ends_with(wanted)
                    || s.class.as_deref().is_some_and(|c| {
                        ships::class_name(c).to_lowercase().contains(&wanted_lower)
                    })
                    || s.name.as_deref().is_some_and(|n| n.to_lowercase().contains(&wanted_lower))
            })
            .collect();
        // "asgard" means the one you still have, not a destroyed one.
        if matches.iter().any(|s| s.destroyed_at.is_none()) {
            matches.retain(|s| s.destroyed_at.is_none());
        }
    }

    let ship = match matches[..] {
        [one] => one,
        [] => {
            eprintln!("No ship matches \"{wanted}\". Run `ships` to see them.");
            return ExitCode::FAILURE;
        }
        _ => {
            eprintln!("\"{wanted}\" matches more than one ship; use its ID:");
            for ship in matches {
                eprintln!("  {}  {}", ship.ship_id, ships::label(ship));
            }
            return ExitCode::FAILURE;
        }
    };

    match db.save_ship_name(&ship.ship_id, name) {
        Ok(()) => {
            println!("{} ({}) will be shown as \"{name}\".", ships::label(ship), ship.ship_id);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Cannot save ship name: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Adds `path` if it is a file, or every `.log` file in it (oldest first) if it is a folder.
fn collect_logs(path: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    if !path.exists() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no such file or folder"));
    }
    if !path.is_dir() {
        files.push(path.to_path_buf());
        return Ok(());
    }

    let mut logs = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("log")) {
            logs.push((entry.metadata()?.modified()?, path));
        }
    }
    logs.sort();
    files.extend(logs.into_iter().map(|(_, path)| path));
    Ok(())
}

fn print_trade(id: i64, outcome: &Outcome, cancelled: bool, db: &Db, names: &mut Names) {
    let trade = match outcome {
        Outcome::Confirmed(t) | Outcome::Failed(t, _) => t,
    };
    let commodity = names.get(&trade.resource_guid);
    let place = locations::trade_place(db, trade);
    let ship = trade.ship_id.as_deref().map(|id| ships::label_for(db, id));
    let line = TradeLine {
        id,
        outcome,
        commodity: &commodity,
        place: &place,
        ship: ship.as_deref(),
        cancelled,
    };
    println!("{line}");
}

/// One console line for a trade.
struct TradeLine<'a> {
    id: i64,
    outcome: &'a Outcome,
    commodity: &'a str,
    place: &'a str,
    ship: Option<&'a str>,
    cancelled: bool,
}

impl std::fmt::Display for TradeLine<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (trade, mut status) = match self.outcome {
            Outcome::Confirmed(t) => (t, String::new()),
            Outcome::Failed(t, reason) => (t, color::amber(format!("  FAILED ({reason})"))),
        };
        if self.cancelled {
            status.push_str(&color::amber("  CANCELLED"));
        }
        let (sign, side_colour): (f64, fn(String) -> String) = match trade.side {
            Side::Buy => (-1.0, color::blue),
            Side::Sell => (1.0, color::green),
        };

        // Pad before colouring, so the codes don't upset the columns.
        let id = color::grey(format!("{:<6}", format!("#{}", self.id)));
        let time = color::grey(trade.timestamp.format("%Y-%m-%d %H:%M:%S"));
        let side = format!("{:<4}", trade.side);
        let scu = format!("{:>8.2} SCU", trade.scu);
        let commodity = format!("{:<24}", self.commodity);
        let place = format!("{:<30}", self.place);
        let amount = format!("{:>14} aUEC", format::signed_auec(sign * trade.total_price));
        let ship = self.ship.map(|s| format!("  ({s})")).unwrap_or_default();

        // Trades that don't count are grey throughout, apart from why.
        if matches!(self.outcome, Outcome::Failed(..)) || self.cancelled {
            let rest = format!("{side}  {scu}  {commodity}  {place} {amount}{ship}");
            return write!(f, "{id} {time}  {}{status}", color::grey(rest));
        }
        write!(
            f,
            "{id} {time}  {}  {scu}  {}  {} {}{}{status}",
            side_colour(side),
            color::bold(commodity),
            place,
            color::money(sign, amount),
            color::cyan(ship),
        )
    }
}
