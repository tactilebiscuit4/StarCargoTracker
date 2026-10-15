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
const USUAL_PLACES: &[&[&str]] = &[
    &["Roberts Space Industries"],
    &["Program Files", "Roberts Space Industries"],
    &["Games", "Roberts Space Industries"],
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
    let cleaned = path.to_string_lossy().trim().trim_matches(['"', '\'']).to_string();
    let mut path = expand_home(&cleaned);
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

/// `~/Games` as typed in a Linux shell; the shell isn't there to expand it.
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) if cfg!(not(windows)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
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
/// `[DialogService] Window 2 selected folder: D:\Roberts Space Industries`
#[cfg(windows)]
fn launcher_folders() -> Vec<PathBuf> {
    match std::env::var_os("APPDATA") {
        Some(appdata) => folders_in_launcher_logs(&Path::new(&appdata).join("rsilauncher").join("logs")),
        None => Vec::new(),
    }
}

/// On Linux the game runs under Wine, so the launcher's log is inside a Wine
/// prefix, and the Windows paths in it point into that prefix.
#[cfg(not(windows))]
fn launcher_folders() -> Vec<PathBuf> {
    let mut folders = Vec::new();
    for prefix in wine_prefixes() {
        let Ok(users) = fs::read_dir(prefix.join("drive_c").join("users")) else {
            continue;
        };
        for user in users.flatten() {
            let logs = user.path().join("AppData").join("Roaming").join("rsilauncher").join("logs");
            let found = folders_in_launcher_logs(&logs);
            folders.extend(found.iter().filter_map(|f| wine_path(&prefix, &f.to_string_lossy())));
        }
    }
    folders
}

fn folders_in_launcher_logs(logs: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(logs) else {
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

#[cfg(windows)]
fn usual_folders() -> Vec<PathBuf> {
    ('A'..='Z')
        .map(|drive| PathBuf::from(format!("{drive}:\\")))
        .filter(|root| root.exists())
        .flat_map(usual_places)
        .collect()
}

/// The usual places on each Wine prefix's C: drive.
#[cfg(not(windows))]
fn usual_folders() -> Vec<PathBuf> {
    wine_prefixes()
        .into_iter()
        .map(|prefix| prefix.join("drive_c"))
        .filter(|root| root.exists())
        .flat_map(usual_places)
        .collect()
}

fn usual_places(root: PathBuf) -> Vec<PathBuf> {
    USUAL_PLACES
        .iter()
        .map(|parts| parts.iter().fold(root.clone(), |path, part| path.join(part)))
        .collect()
}

/// Wine prefixes Star Citizen is often installed in: `$WINEPREFIX`, the LUG
/// Helper's `~/Games/star-citizen`, other folders in `~/Games` (Lutris),
/// `~/.wine`, and Steam's Proton prefixes.
#[cfg(not(windows))]
fn wine_prefixes() -> Vec<PathBuf> {
    let mut prefixes: Vec<PathBuf> = std::env::var_os("WINEPREFIX").map(PathBuf::from).into_iter().collect();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        prefixes.push(home.join("Games").join("star-citizen"));
        prefixes.extend(subfolders(&home.join("Games")));
        prefixes.push(home.join(".wine"));
        for steam in [home.join(".steam").join("steam"), home.join(".local").join("share").join("Steam")] {
            prefixes.extend(subfolders(&steam.join("steamapps").join("compatdata")).into_iter().map(|d| d.join("pfx")));
        }
    }
    let mut seen = Vec::new();
    prefixes.retain(|p| p.join("drive_c").is_dir() && !seen.contains(p) && {
        seen.push(p.clone());
        true
    });
    prefixes
}

#[cfg(not(windows))]
fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs
}

/// Where a Windows path inside a Wine prefix really is: `D:\Games\RSI` is
/// `<prefix>/dosdevices/d:/Games/RSI`, and `C:` is `<prefix>/drive_c`.
#[cfg(not(windows))]
fn wine_path(prefix: &Path, windows_path: &str) -> Option<PathBuf> {
    let (drive, rest) = windows_path.split_once(':')?;
    let drive = drive.trim();
    if drive.len() != 1 || !drive.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let root = if drive.eq_ignore_ascii_case("c") {
        prefix.join("drive_c")
    } else {
        prefix.join("dosdevices").join(format!("{}:", drive.to_ascii_lowercase()))
    };
    Some(rest.split(['\\', '/']).filter(|part| !part.is_empty()).fold(root, |path, part| path.join(part)))
}

/// An example install folder, for messages.
#[cfg(windows)]
const EXAMPLE_FOLDER: &str = r"C:\Program Files\Roberts Space Industries";
#[cfg(not(windows))]
const EXAMPLE_FOLDER: &str = "~/Games/star-citizen/drive_c/Program Files/Roberts Space Industries";

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
        color::grey(format!(
            "Enter the folder you chose in the RSI Launcher, e.g. {EXAMPLE_FOLDER}\n\
             (the StarCitizen or LIVE folder inside it works too). Press Enter on its own to skip."
        ))
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
        crate::shell::command_hint(&format!("game-folder \"{EXAMPLE_FOLDER}\""))
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

    #[cfg(not(windows))]
    #[test]
    fn maps_windows_paths_into_the_wine_prefix() {
        let prefix = Path::new("/home/pilot/Games/star-citizen");
        assert_eq!(
            wine_path(prefix, r"C:\Program Files\Roberts Space Industries"),
            Some(prefix.join("drive_c/Program Files/Roberts Space Industries"))
        );
        assert_eq!(wine_path(prefix, r"D:\Games\RSI"), Some(prefix.join("dosdevices/d:/Games/RSI")));
        assert_eq!(wine_path(prefix, "relative/path"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn finds_the_game_in_a_wine_prefix() {
        let home = std::env::temp_dir().join(format!("star-cargo-tracker-test-wine-{}", std::process::id()));
        let prefix = home.join("Games").join("star-citizen");
        let live = prefix.join("drive_c/Program Files/Roberts Space Industries/StarCitizen/LIVE");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("Data.p4k"), b"").unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let found = detect();
        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(found, Some(live));
        fs::remove_dir_all(home).unwrap();
    }
}
