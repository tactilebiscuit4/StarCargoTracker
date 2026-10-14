//! Readable names for the player's ships.

use crate::db::{Db, Ship};

/// Manufacturer codes the game puts at the start of ship types.
const MANUFACTURERS: &[(&str, &str)] = &[
    ("AEGS", "Aegis"),
    ("ANVL", "Anvil"),
    ("ARGO", "Argo"),
    ("BANU", "Banu"),
    ("CNOU", "C.O."),
    ("CRUS", "Crusader"),
    ("DRAK", "Drake"),
    ("ESPR", "Esperia"),
    ("GAMA", "Gatac"),
    ("GRIN", "Greycat"),
    ("KRIG", "Kruger"),
    ("MISC", "MISC"),
    ("MRAI", "Mirai"),
    ("ORIG", "Origin"),
    ("RSI", "RSI"),
    ("TMBL", "Tumbril"),
    ("XIAN", "Aopoa"),
];

/// `ANVL_Asgard` -> `Anvil Asgard`, `RSI_Aurora_Mk2` -> `RSI Aurora Mk2`.
pub fn class_name(class: &str) -> String {
    let (maker, model) = class.split_once('_').unwrap_or(("", class));
    let model = model.replace('_', " ");
    match MANUFACTURERS.iter().find(|(code, _)| *code == maker) {
        Some((_, name)) => format!("{name} {model}"),
        None => class.replace('_', " "),
    }
}

/// The user's name for the ship, else its type, else its ID.
pub fn label(ship: &Ship) -> String {
    if let Some(name) = &ship.name {
        return name.clone();
    }
    match &ship.class {
        Some(class) => class_name(class),
        None => format!("ship {}", ship.ship_id),
    }
}

pub fn label_for(db: &Db, ship_id: &str) -> String {
    match db.ship(ship_id) {
        Ok(Some(ship)) => label(&ship),
        Ok(None) => format!("ship {ship_id}"),
        Err(e) => {
            eprintln!("Cannot read ships: {e}");
            format!("ship {ship_id}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_ship_types() {
        assert_eq!(class_name("ANVL_Asgard"), "Anvil Asgard");
        assert_eq!(class_name("RSI_Aurora_Mk2"), "RSI Aurora Mk2");
        assert_eq!(class_name("DRAK_Ironclad"), "Drake Ironclad");
        assert_eq!(class_name("XXXX_Unknown_Ship"), "XXXX Unknown Ship");
    }

    #[test]
    fn prefers_users_name() {
        let mut ship = Ship {
            ship_id: "869668647419".into(),
            class: Some("ANVL_Asgard".into()),
            name: None,
            last_seen: None,
            trades: 0,
            destroyed_at: None,
        };
        assert_eq!(label(&ship), "Anvil Asgard");
        ship.name = Some("Big Bertha".into());
        assert_eq!(label(&ship), "Big Bertha");
        ship.name = None;
        ship.class = None;
        assert_eq!(label(&ship), "ship 869668647419");
    }
}
