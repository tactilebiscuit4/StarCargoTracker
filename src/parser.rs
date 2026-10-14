//! Parses individual Game.log lines into commodity trade events.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        })
    }
}

/// A buy or sell request sent from a commodity terminal. The game only logs
/// failures afterwards, so a request is not a completed trade on its own.
#[derive(Debug, Clone, PartialEq)]
pub struct TradeRequest {
    pub timestamp: DateTime<Utc>,
    pub side: Side,
    pub shop_id: String,
    pub shop_name: String,
    pub kiosk_id: String,
    pub resource_guid: String,
    pub scu: f64,
    /// Total aUEC paid (buy) or received (sell).
    pub total_price: f64,
    /// Location code such as `RR_ARC_LEO`. Not on the request line itself;
    /// the tracker fills it in from the player's last known location.
    pub location: Option<String>,
    /// Entity ID of the ship the cargo was auto-loaded into or sold from.
    /// Filled in by the tracker from the terminal's selected ship.
    pub ship_id: Option<String>,
}

/// The player died because the ship they were in was destroyed.
#[derive(Debug, Clone, PartialEq)]
pub struct ShipDestroyed {
    pub timestamp: DateTime<Utc>,
    pub ship_id: String,
    /// The game's name for the ship type, e.g. `ANVL_Asgard`.
    pub class: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Request {
        request: TradeRequest,
        /// `autoLoading[1]`: the cargo moves to or from the selected ship.
        auto_load: bool,
    },
    /// The player arrived somewhere, e.g. `Location[Nyx_Levski]`.
    Location(String),
    /// A commodity terminal selected a ship for loading, by entity ID.
    ShipSelected(String),
    ShipDestroyed(ShipDestroyed),
    /// The player left the pilot seat of one of their ships, which tells us
    /// its type, e.g. `ANVL_Asgard`.
    ShipIdentified {
        timestamp: DateTime<Utc>,
        ship_id: String,
        class: String,
    },
    Failed {
        timestamp: DateTime<Utc>,
        side: Side,
        result: String,
    },
}

/// Extracts the `<2026-10-11T04:24:10.855Z>` timestamp that starts most lines.
pub fn timestamp(line: &str) -> Option<DateTime<Utc>> {
    let rest = line.strip_prefix('<')?;
    let end = rest.find('>')?;
    DateTime::parse_from_rfc3339(&rest[..end])
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

pub fn parse_line(line: &str) -> Option<Event> {
    if line.contains("<RequestLocationInventory>") {
        return fields(line)
            .get("Location")
            .map(|code| Event::Location(code.to_string()));
    }
    if line.contains("Sending VehicleCargoDataRequest") {
        return fields(line)
            .get("vehicleEntityId")
            .map(|id| Event::ShipSelected(id.to_string()));
    }
    if line.contains("<[ActorState] Dead>") && line.contains("destroyed vehicle") {
        return parse_ship_destroyed(line).map(Event::ShipDestroyed);
    }
    if line.contains("<Vehicle Control Flow>") && line.contains("Local client node") {
        return parse_ship_identified(line);
    }

    let side = if line.contains("SendCommodityBuyRequest>") {
        Some(Side::Buy)
    } else if line.contains("SendCommoditySellRequest>") {
        Some(Side::Sell)
    } else if line.contains("Commodity Transaction Response Error") {
        None
    } else {
        return None;
    };

    let timestamp = timestamp(line)?;
    let fields = fields(line);

    let Some(side) = side else {
        let side = match fields.get("type").copied()? {
            "Buying" => Side::Buy,
            "Selling" => Side::Sell,
            _ => return None,
        };
        let result = fields.get("result").copied().unwrap_or("Unknown").to_string();
        return Some(Event::Failed { timestamp, side, result });
    };

    let price_key = match side {
        Side::Buy => "price",
        Side::Sell => "amount",
    };

    let request = TradeRequest {
        timestamp,
        side,
        shop_id: fields.get("shopId")?.to_string(),
        shop_name: fields.get("shopName")?.to_string(),
        kiosk_id: fields.get("kioskId")?.to_string(),
        resource_guid: fields.get("resourceGUID")?.to_string(),
        scu: parse_scu(fields.get("quantity")?)?,
        total_price: fields.get(price_key)?.parse().ok()?,
        location: None,
        ship_id: None,
    };
    let auto_load = fields.get("autoLoading") == Some(&"1");
    Some(Event::Request { request, auto_load })
}

/// `... ejected from zone 'ANVL_Asgard_869668647419' [869668647419] to zone ...
/// due to previous zone being in a destroyed vehicle ...`
fn parse_ship_destroyed(line: &str) -> Option<ShipDestroyed> {
    static ZONE: OnceLock<Regex> = OnceLock::new();
    let re = ZONE.get_or_init(|| Regex::new(r"ejected from zone '([^']+)' \[(\d+)\]").unwrap());

    let cap = re.captures(line)?;
    Some(ShipDestroyed {
        timestamp: timestamp(line)?,
        ship_id: cap[2].to_string(),
        class: ship_class(&cap[1], &cap[2]),
    })
}

/// `<Vehicle Control Flow> CVehicleMovementBase::ClearDriver: Local client node
/// [204713014658] releasing control token for 'ANVL_Asgard_869668647419' [869668647419]`
fn parse_ship_identified(line: &str) -> Option<Event> {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    let re = TOKEN.get_or_init(|| Regex::new(r"control token for '([^']+)' \[(\d+)\]").unwrap());

    let cap = re.captures(line)?;
    Some(Event::ShipIdentified {
        timestamp: timestamp(line)?,
        ship_id: cap[2].to_string(),
        class: ship_class(&cap[1], &cap[2]),
    })
}

/// `ANVL_Asgard_869668647419` -> `ANVL_Asgard`
fn ship_class(entity_name: &str, ship_id: &str) -> String {
    entity_name
        .strip_suffix(&format!("_{ship_id}"))
        .unwrap_or(entity_name)
        .to_string()
}

/// Collects the `key[value]` pairs on a line. The first occurrence of a key wins.
fn fields(line: &str) -> HashMap<&str, &str> {
    static FIELD: OnceLock<Regex> = OnceLock::new();
    let re = FIELD.get_or_init(|| Regex::new(r"(\w+)\[([^\]]*)\]").unwrap());

    let mut map = HashMap::new();
    for cap in re.captures_iter(line) {
        let (_, [key, value]) = cap.extract();
        map.entry(key).or_insert(value);
    }
    map
}

/// Buys log `12000.000000 cSCU`, sells log a bare `120` (SCU).
fn parse_scu(quantity: &str) -> Option<f64> {
    let mut parts = quantity.split_whitespace();
    let amount: f64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("cSCU") => Some(amount / 100.0),
        Some("SCU") | None => Some(amount),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUY: &str = "<2026-10-04T06:20:30.252Z> [Notice] <CEntityComponentCommodityUIProvider::SendCommodityBuyRequest> Sending SShopCommodityBuyRequest - playerId[204713014658] shopId[833998721419] shopName[SCShop_Levski_CargoOffice_Commodities] kioskId[833998721416] price[1036801.000000] shopPricePerCentiSCU[86.400002] resourceGUID[98aa6a70-80e7-41d7-9128-9692053d57a2] autoLoading[1] quantity[12000.000000 cSCU] Cargo Box Data: boxSize[8.000000] | unitAmount[15] [Team_CoreGameplayFeatures][Shops][UI]";
    const SELL: &str = "<2026-10-04T06:43:49.495Z> [Notice] <CEntityComponentCommodityUIProvider::SendCommoditySellRequest> Sending SShopCommoditySellRequest - playerId[204713014658] shopId[832828594576] shopName[SCShop_Admin_lt_base_g] kioskId[832828594575] amount[1470960.000000] resourceGUID[98aa6a70-80e7-41d7-9128-9692053d57a2] autoLoading[1] quantity[120] transactionMode[ResourceContainer] Cargo Box Data:  [boxSize[8] | unitAmount[15]] [Team_CoreGameplayFeatures][Shops][UI]";
    const FAILED: &str = "<2026-10-11T04:25:51.181Z> [Error] <CEntityComponentCommodityUIProvider::RmToken_CommodityTransactionResponse> Commodity Transaction Response Error - playerId[204713014658] result[CargoCreationFailed] type[Buying] [Team_CoreGameplayFeatures][Shops]";

    #[test]
    fn parses_buy() {
        let Some(Event::Request { request: r, auto_load: true }) = parse_line(BUY) else {
            panic!("expected request")
        };
        assert_eq!(r.side, Side::Buy);
        assert_eq!(r.shop_name, "SCShop_Levski_CargoOffice_Commodities");
        assert_eq!(r.resource_guid, "98aa6a70-80e7-41d7-9128-9692053d57a2");
        assert_eq!(r.scu, 120.0);
        assert_eq!(r.total_price, 1036801.0);
    }

    #[test]
    fn parses_sell() {
        let Some(Event::Request { request: r, auto_load: true }) = parse_line(SELL) else {
            panic!("expected request")
        };
        assert_eq!(r.side, Side::Sell);
        assert_eq!(r.shop_name, "SCShop_Admin_lt_base_g");
        assert_eq!(r.scu, 120.0);
        assert_eq!(r.total_price, 1470960.0);
    }

    #[test]
    fn parses_failure() {
        let Some(Event::Failed { side, result, .. }) = parse_line(FAILED) else {
            panic!("expected failure")
        };
        assert_eq!(side, Side::Buy);
        assert_eq!(result, "CargoCreationFailed");
    }

    #[test]
    fn parses_selected_ship() {
        let line = "<2026-10-12T02:06:34.647Z> [Notice] <CEntityComponentCommodityUIProvider::LoadSelectedShipBindings> Sending VehicleCargoDataRequest for vehicleEntityId[869668647419] [Team_CoreGameplayFeatures][Shops][UI]";
        assert_eq!(parse_line(line), Some(Event::ShipSelected("869668647419".into())));
    }

    #[test]
    fn parses_ship_destroyed() {
        let line = "<2026-10-12T02:13:47.985Z> [Notice] <[ActorState] Dead> [ACTOR STATE][CSCActorControlStateDead::PrePhysicsUpdate] Actor 'TactileBiscuit4' [204713014658] ejected from zone 'ANVL_Asgard_869668647419' [869668647419] to zone 'OOC_Stanton_4_Microtech' [874501775333] due to previous zone being in a destroyed vehicle with detached interior. [Team_ActorFeatures][Actor]";
        let Some(Event::ShipDestroyed(d)) = parse_line(line) else {
            panic!("expected ship destroyed")
        };
        assert_eq!(d.ship_id, "869668647419");
        assert_eq!(d.class, "ANVL_Asgard");
    }

    #[test]
    fn identifies_ship_when_leaving_pilot_seat() {
        let line = "<2026-10-04T22:49:20.481Z> [Notice] <Vehicle Control Flow> CVehicleMovementBase::ClearDriver: Local client node [204713014658] releasing control token for 'RSI_Aurora_Mk2_855291308015' [855291308015] [Team_CGP4][Vehicle]";
        let Some(Event::ShipIdentified { ship_id, class, .. }) = parse_line(line) else {
            panic!("expected ship identified")
        };
        assert_eq!(ship_id, "855291308015");
        assert_eq!(class, "RSI_Aurora_Mk2");
    }

    #[test]
    fn parses_location() {
        let line = "<2026-10-11T04:04:38.887Z> [Notice] <RequestLocationInventory> Player[TactileBiscuit4] requested inventory for Location[RR_ARC_LEO] [Team_CoreGameplayFeatures][Inventory]";
        assert_eq!(parse_line(line), Some(Event::Location("RR_ARC_LEO".into())));
    }

    #[test]
    fn ignores_item_shop_purchases() {
        let line = "<2026-10-11T02:26:16.889Z> [Notice] <CEntityComponentShopUIProvider::SendShopBuyRequest> Sending SShopBuyRequest - playerId[204713014658] shopId[863374148614] shopName[SCShop_XS_MiningCart] kioskId[863374148604] client_price[19175.000000] itemClassGUID[27adea05-f94d-4439-872d-b043a631c34f] itemName[grin_tractor_01] quantity[1]  [Team_CoreGameplayFeatures][Shops][UI]";
        assert_eq!(parse_line(line), None);
    }
}
