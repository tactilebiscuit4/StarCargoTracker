//! Finding the Star Citizen install, and the logs in it.
//!
//! The game keeps `Game.log` and the `logbackups` folder in
//! `<install>\StarCitizen\LIVE`. The folder is found once and saved: from
//! where the RSI Launcher was told to install games (it notes that in its
//! log), else the usual places on every drive, else by asking. Any level of
//! the install can be given: the launcher's library folder, `StarCitizen`,
//! `LIVE`, or `Game.log` itself.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::color;
use crate::db::Db;

const SETTING: &str = "game_folder";

/// Where installs usually are, relative to a drive's root.
const USUAL_PLACES: &[&str] = &[
    "Roberts Space Industries",
    r"Program Files\Roberts Space Industries",
    r"Games\Roberts Space Industries",
];

/// `<install>\StarCitizen\LIVE\Game.log`
pub fn log_file(live: &Path) -> PathBuf {
    live.join("Game.log")
}

/// `<install>\StarCitizen\LIVE\logbackups`
pub fn backups_folder(live: &Path) -> PathBuf {
    live.join("logbackups")
}

/// A game folder holds the game's data, or at least its log.
fn is_game_folder(dir: &Path) -> bool {
    dir.join("Data.p4k").is_file() || dir.join("Game.log").is_file()
}

/// The `LIVE` folder for any path into an install: the launcher's library
/// folder, `StarCitizen`, `LIVE` (or another channel's folder), `Game.log`
/// or `logbackups`. `None` if Star Citizen isn't there.
pub fn resolve(path: &Path) -> Option<PathBuf> {
    // Pasted paths often come with quotes.
    let cleaned = path.to_string_lossy().trim().trim_matches('"').to_string();
    let mut path = PathBuf::from(cleaned);
    if path.is_file() || path.file_name().is_some_and(|n| n.eq_ignore_ascii_case("logbackups")) {
        path = path.parent()?.to_path_buf();
    }
    [
        path.clone(),
        path.join("LIVE"),
        path.join("StarCitizen").join("LIVE"),
        path.join("Roberts Space Industries").join("StarCitizen").join("LIVE"),
    ]
    .into_iter()
    .find(|dir| is_game_folder(dir))
}

/// The game folder to use without asking: the saved one, else one found
/// automatically (which is then saved).
pub fn find(db: &Db) -> Option<PathBuf> {
    if let Some(saved) = saved(db) {
        return Some(saved);
    }
    let found = detect()?;
    save(db, &found);
    Some(found)
}

/// The saved folder, if it still holds the game.
pub fn saved(db: &Db) -> Option<PathBuf> {
    let saved = PathBuf::from(db.setting(SETTING).ok().flatten()?);
    is_game_folder(&saved).then_some(saved)
}

pub fn save(db: &Db, live: &Path) {
    if let Err(e) = db.set_setting(SETTING, Some(&live.to_string_lossy())) {
        eprintln!("Cannot save the game folder: {e}");
    }
}

/// Looks where the launcher installed games, then the usual places on
/// every drive.
pub fn detect() -> Option<PathBuf> {
    launcher_folders()
        .into_iter()
        .chain(usual_folders())
        .find_map(|folder| resolve(&folder))
}

/// Folders chosen in the RSI Launcher, most recent first, from its log:
/// `[DialogService] Window 2 selected folder: D:\\Roberts Space Industries`
fn launcher_folders() -> Vec<PathBuf> {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Vec::new();
    };
    let logs = Path::new(&appdata).join("rsilauncher").join("logs");
    let Ok(entries) = fs::read_dir(&logs) else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    for entry in entries.flatten() {
        if let Ok(text) = fs::read_to_string(entry.path()) {
            folders.extend(folders_in_launcher_log(&text));
        }
    }
    folders.reverse();
    folders
}

fn folders_in_launcher_log(text: &str) -> Vec<PathBuf> {
    const MARKER: &str = "selected folder: ";
    text.lines()
        .filter_map(|line| {
            let rest = &line[line.find(MARKER)? + MARKER.len()..];
            let folder = rest.split('"').next()?.replace(r"\\", r"\");
            (!folder.trim().is_empty()).then(|| PathBuf::from(folder.trim()))
        })
        .collect()
}

fn usual_folders() -> Vec<PathBuf> {
    ('A'..='Z')
        .map(|drive| PathBuf::from(format!("{drive}:\\")))
        .filter(|root| root.exists())
        .flat_map(|root| USUAL_PLACES.iter().map(move |place| root.join(place)))
        .collect()
}

/// Asks where the game is installed, until a folder with Star Citizen in it
/// is given (then saved) or nothing is entered. Only when someone can answer.
pub fn ask(db: &Db) -> Option<PathBuf> {
    if !io::stdin().is_terminal() {
        return None;
    }
    println!(
        "{}",
        color::amber("Couldn't find Star Citizen. Where is it installed?")
    );
    println!(
        "{}",
        color::grey(
            "Enter the folder you chose in the RSI Launcher, e.g. C:\\Program Files\\Roberts Space Industries\n\
             (the StarCitizen or LIVE folder inside it works too). Press Enter on its own to skip."
        )
    );
    let stdin = io::stdin();
    loop {
        print!("> ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            return None;
        }
        match resolve(Path::new(line.trim())) {
            Some(live) => {
                save(db, &live);
                println!("Found it: {}", live.display());
                return Some(live);
            }
            None => println!(
                "{}",
                color::amber("Star Citizen isn't in that folder. Try the folder you chose in the RSI Launcher.")
            ),
        }
    }
}

/// How to tell the tracker where the game is, for when it can't ask.
pub fn how_to_set() -> String {
    format!(
        "Tell it where Star Citizen is installed with: {}",
        crate::shell::command_hint(r#"game-folder "C:\Program Files\Roberts Space Industries""#)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway install: `<root>\Roberts Space Industries\StarCitizen\LIVE\Data.p4k`.
    fn fake_install(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("star-cargo-tracker-test-{name}-{}", std::process::id()));
        let library = root.join("Roberts Space Industries");
        let live = library.join("StarCitizen").join("LIVE");
        fs::create_dir_all(live.join("logbackups")).unwrap();
        fs::write(live.join("Data.p4k"), b"").unwrap();
        (root, live)
    }

    #[test]
    fn finds_live_from_any_level_of_the_install() {
        let (root, live) = fake_install("levels");
        let library = root.join("Roberts Space Industries");
        for given in [
            root.clone(),
            library.clone(),
            library.join("StarCitizen"),
            live.clone(),
            live.join("logbackups"),
        ] {
            assert_eq!(resolve(&given).as_deref(), Some(live.as_path()), "{}", given.display());
        }
        // Quoted, as pasted from Explorer.
        let quoted = PathBuf::from(format!("\"{}\"", library.display()));
        assert_eq!(resolve(&quoted).as_deref(), Some(live.as_path()));
        // Somewhere without the game.
        assert_eq!(resolve(&std::env::temp_dir()), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_live_from_game_log_before_data_exists() {
        let root = std::env::temp_dir().join(format!("star-cargo-tracker-test-log-{}", std::process::id()));
        let live = root.join("StarCitizen").join("LIVE");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("Game.log"), b"").unwrap();
        assert_eq!(resolve(&live.join("Game.log")).as_deref(), Some(live.as_path()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_folders_from_the_launcher_log() {
        let log = r#"{ "t":"2026-10-03 22:00:00.000", "[main][info] ": "[App] started"  },
{ "t":"2026-10-03 22:01:00.346", "[main][info] ": "[DialogService] Window 2 selected folder: D:\\Roberts Space Industries"  },
{ "t":"2026-10-04 10:00:00.000", "[main][info] ": "[DialogService] Window 2 selected folder: E:\\Games\\RSI"  },"#;
        assert_eq!(
            folders_in_launcher_log(log),
            vec![PathBuf::from(r"D:\Roberts Space Industries"), PathBuf::from(r"E:\Games\RSI")]
        );
    }
}
