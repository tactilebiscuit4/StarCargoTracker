//! The `cargo>` prompt shown while tracking, for running commands without
//! stopping the tracker. In a terminal it has Tab completion and arrow-key
//! history; piped input is read line by line.

use std::borrow::Cow;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor, ExternalPrinter, Helper};

use crate::db::Db;
use crate::names::Names;
use crate::tracker::Outcome;
use crate::{color, locations, overlay, ships};

const PROMPT: &str = "cargo> ";

pub const HELP: &str = "\
Tracking keeps running while you type. Tab completes, Up and Down recall commands.

Tracking:
  overlay [on|off]              Show or hide the on-screen overlay (no word switches it)
  import [PATH...]              Record trades from log files or folders
                                (no PATH: the game's logbackups folder)
  game-folder [PATH]            Show where Star Citizen is installed, or set it

Trades and profit:
  history [COUNT] [--failed]    Recent trades and totals (--failed also shows failed ones)
  profit                        Profit per sale, losses, and unsold cargo
  export [PATH]                 Save everything to an Excel workbook (default: Documents)
  cancel-trade ID               Stop counting a trade that didn't really happen
  restore-trade ID              Count a cancelled trade again
  write-off COMMODITY SCU|all [NOTE]
                                Record cargo you lost
  delete-write-off ID           Undo a write-off

Market (UEX data):
  prices COMMODITY              Where to buy it cheapest and sell it best
  routes [FROM] [--scu N] [--budget AMOUNT]
                                Most profitable runs from here (or FROM) for your cargo space,
                                and with --budget, for what AMOUNT aUEC can buy (e.g. 250k)
  terminals [SEARCH]            UEX terminals for your locations, or search them
  link-terminal LOCATION ID     Use UEX terminal ID for a location (its name or code)

UEX account (sends your trades to your UEX wallet and trade history):
  uex                           Syncing status, what's been sent, and your balance
  uex secret KEY                Save your UEX secret key (from your UEX profile)
  uex token TOKEN               Save your UEX app token (make an app at uexcorp.space/api/apps)
  uex sync on|off               Send new completed trades (starts in test mode)
  uex send-past                 Also send trades from before syncing started
  uex live|test                 Real entries, or test mode (nothing real recorded)
  uex wallet on|off             Send to your wallet or not (same for: uex history)
  uex push                      Send anything waiting now, retrying rejected ones
  uex forget                    Remove your saved key and token, and stop syncing

Ships and locations:
  ships                         Your ships, their cargo, and destroyed ships
  name-ship SHIP NAME           Name a ship (ID, end of ID, or part of its type)
  locations [--failed]          Trade locations and names (--failed also counts failed trades)
  name-location CODE NAME       Name a location code

Other:
  help                          This list
  exit                          Stop tracking and quit (or press Ctrl+C)

Notes:
  Put names with spaces in double quotes, e.g. write-off \"Construction Materials\" all
  IDs like #12 are shown by history (trades), profit (write-offs) and ships.";

/// Every command the prompt understands, for completion.
const COMMANDS: &[&str] = &[
    "cancel-trade",
    "delete-write-off",
    "exit",
    "export",
    "game-folder",
    "help",
    "history",
    "import",
    "link-terminal",
    "locations",
    "name-location",
    "name-ship",
    "overlay",
    "prices",
    "profit",
    "restore-trade",
    "routes",
    "ships",
    "terminals",
    "uex",
    "write-off",
];

/// Prints tracker output above the prompt without mangling what's being
/// typed. Only used while the prompt is waiting; otherwise output would be
/// held until the next prompt.
static PRINTER: Mutex<Option<Box<dyn ExternalPrinter + Send>>> = Mutex::new(None);
static PROMPT_WAITING: AtomicBool = AtomicBool::new(false);

/// Set once the prompt is running, so hints can be written as you'd type
/// them there (`restore-trade 15`) rather than in a terminal
/// (`star-cargo-tracker restore-trade 15`).
static RUNNING: AtomicBool = AtomicBool::new(false);

/// A command for the user to run, written for where they are: as typed at
/// the `cargo>` prompt, or with the program's name when run from a terminal.
pub fn command_hint(command: &str) -> String {
    if RUNNING.load(Ordering::SeqCst) {
        command.to_string()
    } else {
        format!("star-cargo-tracker {command}")
    }
}

/// How the prompt ended.
pub enum End {
    /// The user typed `exit` or pressed Ctrl+C.
    Exit,
    /// No more input (e.g. stdin isn't a terminal); tracking carries on.
    NoInput,
}

/// Reads and runs commands until `exit` or the end of input. Opens its own
/// database connection, separate from the tracker's.
pub fn run() -> End {
    RUNNING.store(true, Ordering::SeqCst);
    let db = match crate::open_db() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Cannot open trade database for commands: {e}");
            return End::NoInput;
        }
    };
    if !io::stdin().is_terminal() {
        return run_piped(&db);
    }

    let config = Config::builder()
        .completion_type(CompletionType::List)
        .auto_add_history(true)
        .build();
    let mut editor: Editor<Completions, DefaultHistory> = match Editor::with_config(config) {
        Ok(editor) => editor,
        Err(e) => {
            eprintln!("Cannot set up the prompt ({e}); reading commands without it.");
            return run_piped(&db);
        }
    };
    editor.set_helper(Some(Completions::new()));
    if let Ok(printer) = editor.create_external_printer() {
        *PRINTER.lock().unwrap() = Some(Box::new(printer));
    }

    loop {
        PROMPT_WAITING.store(true, Ordering::SeqCst);
        let read = editor.readline(PROMPT);
        PROMPT_WAITING.store(false, Ordering::SeqCst);
        match read {
            Ok(line) => {
                if let Some(end) = run_line(&db, &line) {
                    return end;
                }
            }
            // The prompt reads keys itself, so Ctrl+C arrives here rather
            // than as a signal; treat it like the first Ctrl+C elsewhere.
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => return End::Exit,
            Err(e) => {
                eprintln!("Cannot read input: {e}");
                return End::NoInput;
            }
        }
    }
}

/// Commands from a pipe or file: no prompt, completion or history.
fn run_piped(db: &Db) -> End {
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => return End::NoInput,
            Ok(_) => {}
        }
        if let Some(end) = run_line(db, &line) {
            return end;
        }
    }
}

/// Runs one line of input. Returns `Some` when the prompt should end.
fn run_line(db: &Db, line: &str) -> Option<End> {
    let args = split_args(line);
    match args.first().map(String::as_str) {
        None => {}
        Some("exit" | "quit") => return Some(End::Exit),
        Some("help") => print_help(HELP),
        Some("overlay") => overlay_command(args.get(1).map(String::as_str)),
        Some("watch") => println!("{}", color::amber("Already tracking. Type `exit` to stop.")),
        Some(_) => {
            if crate::run_command(db, &args).is_none() {
                println!(
                    "{}",
                    color::amber(format!(
                        "Unknown command or missing details: `{}`. Type `help` for the list.",
                        args.join(" ")
                    ))
                );
            }
        }
    }
    None
}

/// `overlay on`, `overlay off`, or `overlay` to switch it.
fn overlay_command(arg: Option<&str>) {
    let show = match arg {
        Some("on") => true,
        Some("off") => false,
        None => !overlay::is_visible(),
        Some(other) => {
            println!(
                "{}",
                color::amber(format!(
                    "Use `overlay on`, `overlay off`, or just `overlay` to switch it (not `{other}`)."
                ))
            );
            return;
        }
    };
    overlay::set_visible(show);
    if show {
        println!(
            "{} {}",
            color::green("Overlay on."),
            color::grey("It only shows over the game in Borderless Windowed mode.")
        );
    } else {
        println!("{}", color::grey("Overlay off."));
    }
}

/// Prints a help text (the prompt's or the command line's) with section
/// headings in bold, command names in cyan, and notes in grey.
pub fn print_help(text: &str) {
    let mut in_notes = false;
    for line in text.lines() {
        // Headings are unindented and end in a colon, e.g. "Market (UEX data):".
        if !line.starts_with(' ') && line.ends_with(':') {
            in_notes = line == "Notes:";
            println!("{}", color::bold(line));
            continue;
        }
        if in_notes {
            println!("{}", color::grey(line));
            continue;
        }
        let Some(entry) = line.strip_prefix("  ") else {
            println!("{line}");
            continue;
        };
        // On the command line, entries start with the program's name.
        let (program, entry) = match entry.strip_prefix("star-cargo-tracker ") {
            Some(rest) => (color::grey("star-cargo-tracker "), rest),
            None => (String::new(), entry),
        };
        match entry.split_once(' ') {
            Some((command, rest)) if COMMANDS.contains(&command) || command == "watch" => {
                println!("  {program}{} {rest}", color::cyan(command))
            }
            _ if COMMANDS.contains(&entry) => println!("  {program}{}", color::cyan(entry)),
            _ => println!("{line}"),
        }
    }
}

/// Prints output from the tracker, above the prompt if it's waiting.
pub fn print_background(text: &str) {
    if PROMPT_WAITING.load(Ordering::SeqCst) {
        if let Some(printer) = PRINTER.lock().unwrap().as_mut() {
            if printer.print(text.to_string()).is_ok() {
                return;
            }
        }
    }
    print!("{text}");
    let _ = io::stdout().flush();
}

/// Tab completion for commands and their arguments, using the database for
/// trade IDs, ships, commodities and locations.
struct Completions {
    /// A separate connection, as completion runs inside the prompt.
    db: Option<Db>,
    files: FilenameCompleter,
}

impl Completions {
    fn new() -> Self {
        Self {
            db: crate::open_db().ok(),
            files: FilenameCompleter::new(),
        }
    }

    /// Candidates for argument number `index` (1 = first after the command).
    fn arguments(&self, command: &str, index: usize) -> Vec<Pair> {
        match (command, index) {
            ("overlay", 1) => return vec![plain("on"), plain("off")],
            ("write-off", 2) => return vec![plain("all")],
            ("history", 1 | 2) | ("locations", 1) => return vec![plain("--failed")],
            ("routes", _) => return vec![plain("--scu"), plain("--budget")],
            ("uex", 1) => {
                return ["status", "secret", "token", "sync", "live", "test", "wallet", "history", "push", "send-past", "forget"]
                    .iter()
                    .map(|word| plain(word))
                    .collect()
            }
            ("uex", 2) => return vec![plain("on"), plain("off"), plain("yes")],
            _ => {}
        }
        let Some(db) = &self.db else {
            return Vec::new();
        };
        match (command, index) {
            ("cancel-trade", 1) => trade_ids(db, false),
            ("restore-trade", 1) => trade_ids(db, true),
            ("delete-write-off", 1) => write_off_ids(db),
            ("write-off", 1) => held_commodities(db),
            ("name-ship", 1) => ship_ids(db),
            ("name-location" | "link-terminal", 1) => location_codes(db),
            ("prices", 1) => crate::uex::commodity_names(db)
                .iter()
                .map(|name| plain(&quoted(name)))
                .collect(),
            _ => Vec::new(),
        }
    }
}

impl Completer for Completions {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        let start = word_start(before);
        let word = &before[start..];
        let args = split_args(&before[..start]);

        let candidates = match args.first().map(String::as_str) {
            None => COMMANDS.iter().map(|c| plain(c)).collect(),
            Some("import" | "game-folder") => return self.files.complete(line, pos, ctx),
            Some(command) => self.arguments(command, args.len()),
        };
        // Add the space before the next argument, as shells do.
        let completed = matching(word, candidates)
            .into_iter()
            .map(|mut pair| {
                pair.replacement.push(' ');
                pair
            })
            .collect();
        Ok((start, completed))
    }
}

impl Hinter for Completions {
    type Hint = String;
}
impl Highlighter for Completions {
    /// Colours the prompt; rustyline still measures it by its plain text.
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Owned(color::cyan(color::bold(prompt)))
    }
}
impl Validator for Completions {}
impl Helper for Completions {}

fn plain(text: &str) -> Pair {
    Pair {
        display: text.to_string(),
        replacement: text.to_string(),
    }
}

/// Wraps text with spaces in quotes so it stays one argument.
fn quoted(text: &str) -> String {
    if text.contains(' ') {
        format!("\"{text}\"")
    } else {
        text.to_string()
    }
}

/// Candidates whose text starts with what's been typed (ignoring case and an
/// opening quote).
fn matching(word: &str, candidates: Vec<Pair>) -> Vec<Pair> {
    let typed = word.trim_start_matches('"').to_lowercase();
    candidates
        .into_iter()
        .filter(|c| c.replacement.trim_start_matches('"').to_lowercase().starts_with(&typed))
        .collect()
}

/// Where the word being typed starts: after the last space outside quotes.
fn word_start(before: &str) -> usize {
    let mut start = 0;
    let mut in_quotes = false;
    for (i, c) in before.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => start = i + c.len_utf8(),
            _ => {}
        }
    }
    start
}

/// Recent trades, newest first, that are (or aren't) cancelled.
fn trade_ids(db: &Db, cancelled: bool) -> Vec<Pair> {
    let Ok(trades) = db.recent(50, false) else {
        return Vec::new();
    };
    let mut names = Names::new(db);
    trades
        .iter()
        .rev()
        .filter(|t| t.cancelled == cancelled)
        .filter_map(|t| match &t.outcome {
            Outcome::Confirmed(trade) => Some((t.id, trade)),
            Outcome::Failed(..) => None,
        })
        .map(|(id, trade)| Pair {
            display: format!(
                "{id:<5} {} {} SCU {} @ {}",
                trade.side,
                crate::format::scu(trade.scu),
                names.get(&trade.resource_guid),
                locations::trade_place(db, trade)
            ),
            replacement: id.to_string(),
        })
        .collect()
}

fn write_off_ids(db: &Db) -> Vec<Pair> {
    let Ok(write_offs) = db.write_offs() else {
        return Vec::new();
    };
    let mut names = Names::new(db);
    write_offs
        .iter()
        .rev()
        .map(|w| Pair {
            display: format!(
                "{:<5} {} SCU {}{}",
                w.id,
                crate::format::scu(w.scu),
                names.get(&w.resource_guid),
                w.note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default()
            ),
            replacement: w.id.to_string(),
        })
        .collect()
}

/// Commodities you're holding, for `write-off`.
fn held_commodities(db: &Db) -> Vec<Pair> {
    let Ok(report) = crate::load_report(db) else {
        return Vec::new();
    };
    let mut names = Names::new(db);
    let mut held: Vec<String> = Vec::new();
    for lot in &report.holdings {
        let name = names.get(&lot.buy.resource_guid);
        if !held.contains(&name) {
            held.push(name);
        }
    }
    held.iter().map(|name| plain(&quoted(name))).collect()
}

fn ship_ids(db: &Db) -> Vec<Pair> {
    let Ok(ships) = db.ships() else {
        return Vec::new();
    };
    ships
        .iter()
        .map(|ship| Pair {
            display: format!("{}  {}", ship.ship_id, ships::label(ship)),
            replacement: ship.ship_id.clone(),
        })
        .collect()
}

fn location_codes(db: &Db) -> Vec<Pair> {
    let Ok(codes) = db.trade_locations() else {
        return Vec::new();
    };
    codes
        .iter()
        .map(|(code, _, _)| Pair {
            display: format!("{code}  ({})", locations::display_name(db, code)),
            replacement: code.clone(),
        })
        .collect()
}

/// Splits a command line on spaces, keeping "double quoted" parts together.
fn split_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_arg = false;
    // Piped input from PowerShell can start with a byte order mark.
    for c in line.trim().trim_start_matches('\u{feff}').chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_arg = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_arg {
                    args.push(std::mem::take(&mut current));
                    has_arg = false;
                }
            }
            c => {
                current.push(c);
                has_arg = true;
            }
        }
    }
    if has_arg {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_arguments() {
        assert_eq!(split_args("  profit \n"), vec!["profit"]);
        assert_eq!(
            split_args(r#"write-off "Construction Materials" all ship lost"#),
            vec!["write-off", "Construction Materials", "all", "ship", "lost"]
        );
        assert_eq!(split_args(r#"name-ship 7419 """#), vec!["name-ship", "7419", ""]);
        assert!(split_args("   ").is_empty());
        assert_eq!(split_args("\u{feff}history 2"), vec!["history", "2"]);
    }

    #[test]
    fn finds_word_being_typed() {
        assert_eq!(word_start("can"), 0);
        assert_eq!(word_start("cancel-trade 1"), 13);
        assert_eq!(word_start(r#"write-off "Construction Mat"#), 10);
    }

    #[test]
    fn matches_typed_prefix() {
        let names = |pairs: Vec<Pair>| -> Vec<String> {
            pairs.into_iter().map(|p| p.replacement).collect()
        };
        let commands = || COMMANDS.iter().map(|c| plain(c)).collect();
        assert_eq!(names(matching("can", commands())), vec!["cancel-trade"]);
        assert_eq!(names(matching("NAME", commands())), vec!["name-location", "name-ship"]);
        assert_eq!(
            names(matching(r#""constr"#, vec![plain(&quoted("Construction Materials"))])),
            vec![r#""Construction Materials""#]
        );
    }

    #[test]
    fn completes_fixed_words_without_a_database() {
        let completions = Completions {
            db: None,
            files: FilenameCompleter::new(),
        };
        let words = |pairs: Vec<Pair>| -> Vec<String> {
            pairs.into_iter().map(|p| p.replacement).collect()
        };
        assert_eq!(words(completions.arguments("overlay", 1)), vec!["on", "off"]);
        assert_eq!(words(completions.arguments("write-off", 2)), vec!["all"]);
        assert!(completions.arguments("cancel-trade", 1).is_empty());
    }
}
