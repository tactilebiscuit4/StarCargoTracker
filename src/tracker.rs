//! Turns trade requests into confirmed or failed trades.
//!
//! The game logs an error shortly after a failed request but logs nothing on
//! success, so a request is treated as confirmed once `CONFIRM_AFTER_SECS`
//! pass without a matching error.

use chrono::{DateTime, TimeDelta, Utc};

use crate::parser::{self, Event, ShipDestroyed, TradeRequest};

/// Failures observed in logs arrive within ~0.5s of the request.
const CONFIRM_AFTER_SECS: i64 = 5;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Confirmed(TradeRequest),
    Failed(TradeRequest, String),
}

#[derive(Debug, Default)]
pub struct Tracker {
    pending: Vec<TradeRequest>,
    /// Where the player last arrived; trades happen at this location.
    location: Option<String>,
    /// The ship the commodity terminal last selected for loading.
    selected_ship: Option<String>,
    /// Ship destructions not yet collected with `take_destroyed`.
    destroyed: Vec<ShipDestroyed>,
    /// The player's ships whose type was logged, not yet collected with
    /// `take_ships_seen`.
    ships_seen: Vec<ShipSeen>,
}

/// One of the player's ships, as named in the log.
#[derive(Debug, Clone, PartialEq)]
pub struct ShipSeen {
    pub timestamp: DateTime<Utc>,
    pub ship_id: String,
    /// e.g. `ANVL_Asgard`
    pub class: String,
}

impl Tracker {
    pub fn feed(&mut self, line: &str) -> Vec<Outcome> {
        let Some(now) = parser::timestamp(line) else {
            return Vec::new();
        };
        let mut outcomes = self.expire(now);

        match parser::parse_line(line) {
            Some(Event::Request { mut request, auto_load }) => {
                request.location = self.location.clone();
                if auto_load {
                    request.ship_id = self.selected_ship.clone();
                }
                self.pending.push(request);
            }
            Some(Event::Location(code)) => {
                self.location = Some(code);
                // A terminal at the old location can't load ships here.
                self.selected_ship = None;
            }
            Some(Event::ShipSelected(id)) => self.selected_ship = Some(id),
            Some(Event::ShipDestroyed(destroyed)) => {
                self.ships_seen.push(ShipSeen {
                    timestamp: destroyed.timestamp,
                    ship_id: destroyed.ship_id.clone(),
                    class: destroyed.class.clone(),
                });
                self.destroyed.push(destroyed);
            }
            Some(Event::ShipIdentified { timestamp, ship_id, class }) => {
                self.ships_seen.push(ShipSeen { timestamp, ship_id, class });
            }
            Some(Event::Failed { side, result, .. }) => {
                if let Some(i) = self.pending.iter().position(|p| p.side == side) {
                    outcomes.push(Outcome::Failed(self.pending.remove(i), result));
                }
            }
            None => {}
        }
        outcomes
    }

    /// Confirms requests that have gone `CONFIRM_AFTER_SECS` without an error.
    pub fn expire(&mut self, now: DateTime<Utc>) -> Vec<Outcome> {
        let cutoff = now - TimeDelta::seconds(CONFIRM_AFTER_SECS);
        let (done, waiting): (Vec<_>, Vec<_>) = self
            .pending
            .drain(..)
            .partition(|p| p.timestamp <= cutoff);
        self.pending = waiting;
        done.into_iter().map(Outcome::Confirmed).collect()
    }

    /// Confirms everything still pending, e.g. at the end of a log file.
    pub fn flush(&mut self) -> Vec<Outcome> {
        self.pending.drain(..).map(Outcome::Confirmed).collect()
    }

    /// Where the player last arrived, as a location code.
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    /// Whether any request is still waiting to be confirmed or failed.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Ships destroyed since the last call, in the order it happened.
    pub fn take_destroyed(&mut self) -> Vec<ShipDestroyed> {
        std::mem::take(&mut self.destroyed)
    }

    /// Ships identified since the last call.
    pub fn take_ships_seen(&mut self) -> Vec<ShipSeen> {
        std::mem::take(&mut self.ships_seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUY: &str = "<2026-10-11T04:25:50.807Z> [Notice] <CEntityComponentCommodityUIProvider::SendCommodityBuyRequest> Sending SShopCommodityBuyRequest - playerId[204713014658] shopId[863370406972] shopName[SCShop_Admin_lt_base_g] kioskId[863370406971] price[251329.000000] shopPricePerCentiSCU[19.635000] resourceGUID[7bbd3197-a6e1-49b3-a495-0b7ef4f8ce40] autoLoading[1] quantity[12800.000000 cSCU] Cargo Box Data: boxSize[16.000000] | unitAmount[8] [Team_CoreGameplayFeatures][Shops][UI]";
    const FAILED: &str = "<2026-10-11T04:25:51.181Z> [Error] <CEntityComponentCommodityUIProvider::RmToken_CommodityTransactionResponse> Commodity Transaction Response Error - playerId[204713014658] result[CargoCreationFailed] type[Buying] [Team_CoreGameplayFeatures][Shops]";
    const LATER: &str = "<2026-10-11T04:26:07.259Z> [Notice] <Carriage movement state changing> [TRANSPORT] Movement state changing";

    #[test]
    fn error_marks_request_failed() {
        let mut t = Tracker::default();
        assert!(t.feed(BUY).is_empty());
        let out = t.feed(FAILED);
        assert!(matches!(&out[..], [Outcome::Failed(_, r)] if r == "CargoCreationFailed"));
        assert!(t.flush().is_empty());
    }

    #[test]
    fn request_without_error_is_confirmed() {
        let mut t = Tracker::default();
        assert!(t.feed(BUY).is_empty());
        let out = t.feed(LATER);
        assert!(matches!(&out[..], [Outcome::Confirmed(r)] if r.scu == 128.0));
    }

    #[test]
    fn auto_loaded_request_gets_selected_ship() {
        let mut t = Tracker::default();
        t.feed("<2026-10-11T04:25:40.000Z> [Notice] <CEntityComponentCommodityUIProvider::LoadSelectedShipBindings> Sending VehicleCargoDataRequest for vehicleEntityId[869668647419] [Shops]");
        t.feed(BUY);
        let out = t.flush();
        assert!(matches!(&out[..], [Outcome::Confirmed(r)] if r.ship_id.as_deref() == Some("869668647419")));
    }

    #[test]
    fn selected_ship_is_forgotten_after_moving() {
        let mut t = Tracker::default();
        t.feed("<2026-10-11T04:20:00.000Z> [Notice] <CEntityComponentCommodityUIProvider::LoadSelectedShipBindings> Sending VehicleCargoDataRequest for vehicleEntityId[869668647419] [Shops]");
        t.feed("<2026-10-11T04:22:00.000Z> [Notice] <RequestLocationInventory> Player[P] requested inventory for Location[RR_ARC_LEO] [Inventory]");
        t.feed(BUY);
        let out = t.flush();
        assert!(matches!(&out[..], [Outcome::Confirmed(r)] if r.ship_id.is_none()));
    }

    #[test]
    fn collects_destroyed_ships() {
        let mut t = Tracker::default();
        t.feed("<2026-10-12T02:13:47.985Z> [Notice] <[ActorState] Dead> [ACTOR STATE] Actor 'P' [1] ejected from zone 'ANVL_Asgard_869668647419' [869668647419] to zone 'OOC_Stanton_4_Microtech' [874501775333] due to previous zone being in a destroyed vehicle with detached interior. [Actor]");
        let destroyed = t.take_destroyed();
        assert_eq!(destroyed.len(), 1);
        assert_eq!(destroyed[0].ship_id, "869668647419");
        assert!(t.take_destroyed().is_empty());
        // A destroyed ship's type is also worth remembering.
        assert_eq!(t.take_ships_seen()[0].class, "ANVL_Asgard");
    }

    #[test]
    fn collects_ships_seen() {
        let mut t = Tracker::default();
        t.feed("<2026-10-11T04:06:41.616Z> [Notice] <Vehicle Control Flow> CVehicleMovementBase::ClearDriver: Local client node [204713014658] releasing control token for 'ANVL_Asgard_869668647419' [869668647419] [Team_CGP4][Vehicle]");
        let seen = t.take_ships_seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].ship_id, "869668647419");
        assert_eq!(seen[0].class, "ANVL_Asgard");
        assert!(t.take_ships_seen().is_empty());
    }

    #[test]
    fn request_gets_last_location() {
        let mut t = Tracker::default();
        t.feed("<2026-10-11T03:46:20.761Z> [Notice] <RequestLocationInventory> Player[P] requested inventory for Location[Stanton4_NewBabbage] [Inventory]");
        t.feed("<2026-10-11T04:04:38.887Z> [Notice] <RequestLocationInventory> Player[P] requested inventory for Location[RR_ARC_LEO] [Inventory]");
        t.feed(BUY);
        let out = t.flush();
        assert!(matches!(&out[..], [Outcome::Confirmed(r)] if r.location.as_deref() == Some("RR_ARC_LEO")));
    }
}
