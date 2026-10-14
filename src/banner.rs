//! The title banner shown every time the program runs.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::color;

const ART: &str = r"
  .    *          .       *           .       *      .
 ████ █████  ███  ████      ████  ███  ████   ████  ███
█       █   █   █ █   █    █     █   █ █   █ █     █   █
 ███    █   █████ ████     █     █████ ████  █  ██ █   █
    █   █   █   █ █  █     █     █   █ █  █  █   █ █   █
████    █   █   █ █   █     ████ █   █ █   █  ████  ███

        █████ ████   ███   ████ █   █ █████ ████
          █   █   █ █   █ █     █  █  █     █   █
          █   ████  █████ █     ███   ████  ████
          █   █  █  █   █ █     █  █  █     █  █
          █   █   █ █   █  ████ █   █ █████ █   █
  *       .        *          .        *          .    *";

const TIPS: &[&str] = &[
    "Run `profit` to see profit per sale, losses, and cargo you're still holding.",
    "A sale that never paid out? `cancel-trade ID` stops it being counted.",
    "Name your ships with `name-ship`, e.g. name-ship asgard \"Big Bertha\".",
    "Cargo lost some other way? Record it with `write-off`.",
    "The overlay only shows over the game in Borderless Windowed mode.",
    "See a raw location code? Give it a name with `name-location`.",
    "Run `import` to load older sessions from the game's logbackups folder.",
    "Ship destroyed with you aboard? Its cargo is written off automatically.",
    "Run `routes` for the best runs from where you are, using UEX market data.",
    "Run `prices quartz` to see where to buy and sell a commodity.",
];

/// Prints the banner, with where the tracker reads and stores data.
pub fn print(log: &Path, db: Result<&Path, &str>) {
    for line in ART.lines() {
        // The letters in cyan, the star field dim.
        if line.contains('█') {
            println!("{}", color::cyan(line));
        } else {
            println!("{}", color::grey(line));
        }
    }
    println!(
        "  {}",
        color::grey(format!(
            "v{} - tracks your Star Citizen commodity trades from Game.log",
            env!("CARGO_PKG_VERSION")
        ))
    );
    println!();

    let found = if log.exists() {
        color::green("found")
    } else {
        color::amber("not found yet")
    };
    println!("  {}  {} ({found})", color::bold("Game log:"), log.display());
    match db {
        Ok(path) => println!("  {}  {}", color::bold("Database:"), path.display()),
        Err(e) => println!("  {}  {}", color::bold("Database:"), color::red(format!("unavailable ({e})"))),
    }
    println!(
        "  {}  watch, overlay, history, profit, ships... (`help` for all)",
        color::bold("Commands:")
    );
    println!("             While tracking, type them at the {} prompt.", color::cyan("cargo>"));
    println!("  {}       {}", color::bold("Tip:"), color::amber(tip()));
    println!();
}

/// A different tip each run, without needing a random number crate.
fn tip() -> &'static str {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    TIPS[seconds as usize % TIPS.len()]
}
