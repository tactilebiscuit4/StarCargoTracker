//! Terminal colours, matching the overlay's. Turned off when output isn't a
//! terminal (piped or saved to a file) or the `NO_COLOR` environment
//! variable is set; `CLICOLOR_FORCE=1` turns them on regardless.

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::OnceLock;

const BLUE: (u8, u8, u8) = (110, 180, 255);
const GREEN: (u8, u8, u8) = (110, 220, 130);
const AMBER: (u8, u8, u8) = (240, 180, 80);
const RED: (u8, u8, u8) = (240, 95, 95);
const GREY: (u8, u8, u8) = (140, 140, 140);
const CYAN: (u8, u8, u8) = (90, 200, 220);
const MAGENTA: (u8, u8, u8) = (200, 140, 240);

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return false;
        }
        if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
            return true;
        }
        std::io::stdout().is_terminal() && enable_escape_codes()
    })
}

/// Windows Terminal understands colour codes already; the older console
/// window needs them switched on.
fn enable_escape_codes() -> bool {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };
    // SAFETY: plain console calls on our own stdout handle.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(out, &mut mode) == 0 {
            return false;
        }
        mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || SetConsoleMode(out, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

fn paint(code: &str, text: impl Display) -> String {
    if enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn rgb((r, g, b): (u8, u8, u8), text: impl Display) -> String {
    paint(&format!("38;2;{r};{g};{b}"), text)
}

pub fn blue(text: impl Display) -> String {
    rgb(BLUE, text)
}
pub fn green(text: impl Display) -> String {
    rgb(GREEN, text)
}
pub fn amber(text: impl Display) -> String {
    rgb(AMBER, text)
}
pub fn red(text: impl Display) -> String {
    rgb(RED, text)
}
pub fn grey(text: impl Display) -> String {
    rgb(GREY, text)
}
pub fn cyan(text: impl Display) -> String {
    rgb(CYAN, text)
}
pub fn magenta(text: impl Display) -> String {
    rgb(MAGENTA, text)
}
pub fn bold(text: impl Display) -> String {
    paint("1", text)
}

/// Green for gains, red for losses: for text like `+434,159` or `-57,003`.
pub fn money(amount: f64, text: impl Display) -> String {
    if amount < 0.0 {
        red(text)
    } else {
        green(text)
    }
}
