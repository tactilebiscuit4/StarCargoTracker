//! Turns game location codes (`RR_ARC_LEO`, `Nyx_Levski`) into readable names.
//!
//! No public API maps these codes, so names come from, in order: names the
//! user has set, a few known stations, and patterns in the codes themselves.
//! Anything unrecognised is shown as the raw code.

use crate::db::Db;
use crate::parser::TradeRequest;

/// Orbital stations whose codes don't contain their name.
const KNOWN: &[(&str, &str)] = &[
    ("RR_ARC_LEO", "Baijini Point"),
    ("RR_CRU_LEO", "Seraphim Station"),
    ("RR_HUR_LEO", "Everus Harbor"),
    ("RR_MIC_LEO", "Port Tressler"),
];

pub fn display_name(db: &Db, code: &str) -> String {
    match db.location_name(code) {
        Ok(Some(name)) => return name,
        Ok(None) => {}
        Err(e) => eprintln!("Cannot read location names: {e}"),
    }
    builtin_name(code).unwrap_or_else(|| code.to_string())
}

/// Where a trade happened. Trades recorded before location tracking only
/// have the shop's internal name.
pub fn trade_place(db: &Db, trade: &TradeRequest) -> String {
    match &trade.location {
        Some(code) => display_name(db, code),
        None => trade.shop_name.clone(),
    }
}

fn builtin_name(code: &str) -> Option<String> {
    if let Some((_, name)) = KNOWN.iter().find(|(known, _)| *known == code) {
        return Some(name.to_string());
    }

    let parts: Vec<&str> = code.split('_').collect();
    match parts[..] {
        // RR_JP_NyxPyro: the station in Nyx at the jump point to Pyro.
        ["RR", "JP", route] => match split_camel(route)[..] {
            [here, there] => Some(format!("{there} Gateway ({here})")),
            _ => None,
        },
        // RR_CRU_L5: Lagrange point stations go by their short name in game.
        ["RR", planet, point] if is_lagrange(point) => Some(format!("{planet}-{point}")),
        // Stanton1a_HurdynMining_HDMSBezdek
        [_, .., last] if last.len() > 4 && last.starts_with("HDMS") => {
            Some(format!("HDMS-{}", &last[4..]))
        }
        // Nyx_Levski, Stanton4_NewBabbage
        [_, name] if name.starts_with(|c: char| c.is_ascii_uppercase())
            && name.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            Some(split_camel(name).join(" "))
        }
        _ => None,
    }
}

fn is_lagrange(point: &str) -> bool {
    point
        .strip_prefix('L')
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// `NewBabbage` -> `["New", "Babbage"]`
fn split_camel(s: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = 0;
    let mut prev_lower = false;
    for (i, c) in s.char_indices() {
        if c.is_ascii_uppercase() && prev_lower {
            words.push(&s[start..i]);
            start = i;
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    words.push(&s[start..]);
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_codes_from_your_logs() {
        let cases = [
            ("Nyx_Levski", Some("Levski")),
            ("Stanton4_NewBabbage", Some("New Babbage")),
            ("RR_ARC_LEO", Some("Baijini Point")),
            ("RR_MIC_LEO", Some("Port Tressler")),
            ("RR_CRU_L5", Some("CRU-L5")),
            ("RR_JP_NyxPyro", Some("Pyro Gateway (Nyx)")),
            ("RR_JP_StantonMagnus", Some("Magnus Gateway (Stanton)")),
            ("Stanton1a_HurdynMining_HDMSBezdek", Some("HDMS-Bezdek")),
            ("AsteroidClusterBase_Nyx_Social_Keeger_004", None),
        ];
        for (code, expected) in cases {
            assert_eq!(builtin_name(code).as_deref(), expected, "{code}");
        }
    }
}
