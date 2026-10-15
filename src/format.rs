//! Formatting shared by the console and the overlay.

/// Text from the internet (UEX, the Star Citizen Wiki) without control
/// characters, so it can't send escape codes to the terminal (e.g. to change
/// its title or write to the clipboard).
pub fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Formats a credit amount with thousands separators, e.g. `-1,036,801`.
pub fn auec(amount: f64) -> String {
    let rounded = amount.round() as i64;
    let digits = rounded.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if rounded < 0 {
        out.insert(0, '-');
    }
    out
}

/// Like `auec`, but always shows the sign, e.g. `+434,159`.
pub fn signed_auec(amount: f64) -> String {
    let formatted = auec(amount);
    if formatted.starts_with('-') || formatted == "0" {
        formatted
    } else {
        format!("+{formatted}")
    }
}

/// ` (+41.9%)`, or nothing if the margin is unknown.
pub fn margin(margin: Option<f64>) -> String {
    margin
        .map(|m| format!(" ({:+.1}%)", m * 100.0))
        .unwrap_or_default()
}

/// `16` for whole amounts, otherwise `12.50`.
pub fn scu(scu: f64) -> String {
    if (scu - scu.round()).abs() < 0.005 {
        format!("{scu:.0}")
    } else {
        format!("{scu:.2}")
    }
}

/// How long ago something was, e.g. `5m ago`, `3h ago`, `2d ago`.
pub fn age(when: chrono::DateTime<chrono::Utc>) -> String {
    let minutes = (chrono::Utc::now() - when).num_minutes().max(0);
    match minutes {
        0..=59 => format!("{minutes}m ago"),
        60..=2879 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

/// `1h 05m`, or `23m` for under an hour.
pub fn duration(duration: chrono::TimeDelta) -> String {
    let minutes = duration.num_minutes().max(0);
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h {:02}m", minutes / 60, minutes % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_amounts() {
        assert_eq!(auec(1036801.0), "1,036,801");
        assert_eq!(auec(-57003.0), "-57,003");
        assert_eq!(signed_auec(434159.0), "+434,159");
        assert_eq!(signed_auec(0.0), "0");
        assert_eq!(margin(Some(0.41875)), " (+41.9%)");
        assert_eq!(scu(16.0), "16");
        assert_eq!(scu(12.5), "12.50");
    }

    #[test]
    fn cleans_control_characters() {
        assert_eq!(clean("Levski\u{1b}]52;c;aGk=\u{7}"), "Levski]52;c;aGk=");
        assert_eq!(clean("Port\u{9b}31m Tressler"), "Port31m Tressler");
        assert_eq!(clean("Pyro Gateway (Nyx)"), "Pyro Gateway (Nyx)");
    }
}
