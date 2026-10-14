//! Matches sales to the purchases they came from to work out profit.
//!
//! Each commodity's purchases form a first-in, first-out queue: a sale uses
//! up the oldest cargo bought first. SCU sold with no recorded purchase
//! (mined or salvaged cargo, or bought before tracking started) has no known
//! cost, so its revenue is kept out of profit.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::parser::{Side, TradeRequest};

/// Ignore floating point dust left over when SCU amounts are subtracted.
const SCU_EPSILON: f64 = 1e-6;

/// Cargo from one purchase that hasn't been sold yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Lot {
    pub buy: TradeRequest,
    pub scu_left: f64,
}

impl Lot {
    pub fn cost_left(&self) -> f64 {
        self.buy.total_price / self.buy.scu * self.scu_left
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sale {
    pub sell: TradeRequest,
    /// The purchases this sale used, with how many SCU came from each.
    pub sources: Vec<(TradeRequest, f64)>,
    /// What the matched SCU cost to buy.
    pub cost: f64,
    pub matched_scu: f64,
}

impl Sale {
    pub fn unmatched_scu(&self) -> f64 {
        (self.sell.scu - self.matched_scu).max(0.0)
    }

    /// Revenue from the SCU that have a known purchase price.
    pub fn matched_revenue(&self) -> f64 {
        if self.sell.scu <= 0.0 {
            return 0.0;
        }
        self.sell.total_price * self.matched_scu / self.sell.scu
    }

    pub fn unmatched_revenue(&self) -> f64 {
        self.sell.total_price - self.matched_revenue()
    }

    /// `None` if none of the cargo has a recorded purchase.
    pub fn profit(&self) -> Option<f64> {
        (self.matched_scu > SCU_EPSILON).then(|| self.matched_revenue() - self.cost)
    }

    /// Profit as a fraction of what the cargo cost.
    pub fn margin(&self) -> Option<f64> {
        let profit = self.profit()?;
        (self.cost > 0.0).then(|| profit / self.cost)
    }

    /// When the oldest cargo in this sale was bought.
    pub fn first_bought(&self) -> Option<DateTime<Utc>> {
        self.sources.iter().map(|(buy, _)| buy.timestamp).min()
    }
}

/// Cargo the user recorded as lost (destroyed, jettisoned, stolen...).
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOff {
    pub id: i64,
    pub timestamp: DateTime<Utc>,
    pub resource_guid: String,
    pub scu: f64,
    pub note: Option<String>,
    /// Set when the cargo was lost with this ship; only cargo loaded on it is
    /// written off.
    pub ship_id: Option<String>,
}

/// A write-off matched to the purchases of the cargo that was lost.
#[derive(Debug, Clone, PartialEq)]
pub struct Loss {
    pub write_off: WriteOff,
    pub sources: Vec<(TradeRequest, f64)>,
    /// What the lost cargo cost to buy.
    pub cost: f64,
}

#[derive(Debug, Default)]
pub struct Report {
    pub sales: Vec<Sale>,
    pub losses: Vec<Loss>,
    /// Cargo bought but not sold or lost yet, oldest first.
    pub holdings: Vec<Lot>,
}

impl Report {
    pub fn sales_profit(&self) -> f64 {
        self.sales.iter().filter_map(Sale::profit).sum()
    }

    pub fn total_losses(&self) -> f64 {
        self.losses.iter().map(|loss| loss.cost).sum()
    }

    pub fn total_profit(&self) -> f64 {
        self.sales_profit() - self.total_losses()
    }

    pub fn unmatched_revenue(&self) -> f64 {
        self.sales.iter().map(Sale::unmatched_revenue).sum()
    }
}

/// `trades` must be completed trades and `write_offs` write-offs, each in the
/// order they happened.
pub fn calculate(trades: &[TradeRequest], write_offs: &[WriteOff]) -> Report {
    let mut lots: HashMap<&str, Vec<Lot>> = HashMap::new();
    let mut report = Report::default();
    let mut write_offs = write_offs.iter().peekable();

    for trade in trades {
        // Apply write-offs that happened before this trade.
        while let Some(write_off) = write_offs.next_if(|w| w.timestamp < trade.timestamp) {
            report.losses.push(lose(&mut lots, write_off));
        }

        let queue = lots.entry(trade.resource_guid.as_str()).or_default();
        match trade.side {
            Side::Buy => queue.push(Lot {
                buy: trade.clone(),
                scu_left: trade.scu,
            }),
            Side::Sell => {
                // Cargo sold from a ship comes from that ship's purchases first.
                let (sources, cost, matched_scu) =
                    take_oldest(queue, trade.scu, trade.ship_id.as_deref(), false);
                report.sales.push(Sale {
                    sell: trade.clone(),
                    sources,
                    cost,
                    matched_scu,
                });
            }
        }
    }
    for write_off in write_offs {
        report.losses.push(lose(&mut lots, write_off));
    }

    report.holdings = lots.into_values().flatten().collect();
    report.holdings.sort_by_key(|lot| lot.buy.timestamp);
    report
}

fn lose<'a>(lots: &mut HashMap<&'a str, Vec<Lot>>, write_off: &'a WriteOff) -> Loss {
    let queue = lots.entry(write_off.resource_guid.as_str()).or_default();
    // A destroyed ship only takes the cargo that was loaded on it.
    let (sources, cost, _) = take_oldest(queue, write_off.scu, write_off.ship_id.as_deref(), true);
    Loss {
        write_off: write_off.clone(),
        sources,
        cost,
    }
}

/// Removes up to `scu` from the oldest lots, taking lots on `ship` first.
/// With `only_ship`, other lots are never used. Returns the purchases used,
/// their total cost, and how many SCU were found.
fn take_oldest(
    lots: &mut Vec<Lot>,
    scu: f64,
    ship: Option<&str>,
    only_ship: bool,
) -> (Vec<(TradeRequest, f64)>, f64, f64) {
    let (mut sources, mut cost, mut matched) = (Vec::new(), 0.0, 0.0);
    let mut to_match = scu;

    // Each pass takes lots that are (Some(true)) or aren't (Some(false)) on the
    // ship, or any lot (None).
    let passes: &[Option<bool>] = match (ship, only_ship) {
        (None, _) => &[None],
        (Some(_), true) => &[Some(true)],
        (Some(_), false) => &[Some(true), Some(false)],
    };
    for pass in passes {
        // `lots` is in purchase order, so this takes the oldest first.
        for lot in lots.iter_mut() {
            if to_match <= SCU_EPSILON {
                break;
            }
            if pass.is_some_and(|want| (lot.buy.ship_id.as_deref() == ship) != want) {
                continue;
            }
            let used = lot.scu_left.min(to_match);
            if used <= SCU_EPSILON {
                continue;
            }
            cost += lot.buy.total_price / lot.buy.scu * used;
            matched += used;
            sources.push((lot.buy.clone(), used));
            lot.scu_left -= used;
            to_match -= used;
        }
    }
    lots.retain(|lot| lot.scu_left > SCU_EPSILON);
    (sources, cost, matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(side: Side, minute: u32, commodity: &str, scu: f64, total_price: f64) -> TradeRequest {
        TradeRequest {
            timestamp: DateTime::parse_from_rfc3339(&format!("2026-10-04T06:{minute:02}:00Z"))
                .unwrap()
                .with_timezone(&Utc),
            side,
            shop_id: "1".into(),
            shop_name: "SCShop_Test".into(),
            kiosk_id: "2".into(),
            resource_guid: commodity.into(),
            scu,
            total_price,
            location: None,
            ship_id: None,
        }
    }

    fn on_ship(mut trade: TradeRequest, ship: &str) -> TradeRequest {
        trade.ship_id = Some(ship.into());
        trade
    }

    #[test]
    fn your_construction_materials_run() {
        let report = calculate(&[
            trade(Side::Buy, 20, "construction", 120.0, 1036801.0),
            trade(Side::Sell, 43, "construction", 120.0, 1470960.0),
        ], &[]);
        let sale = &report.sales[0];
        assert_eq!(sale.profit(), Some(434159.0));
        assert!((sale.margin().unwrap() - 0.41875).abs() < 1e-4);
        assert!(report.holdings.is_empty());
    }

    #[test]
    fn sells_oldest_cargo_first() {
        let report = calculate(&[
            trade(Side::Buy, 1, "silicon", 10.0, 100.0), // 10/SCU
            trade(Side::Buy, 2, "silicon", 10.0, 200.0), // 20/SCU
            trade(Side::Sell, 3, "silicon", 15.0, 450.0), // 30/SCU
        ], &[]);
        let sale = &report.sales[0];
        assert_eq!(sale.cost, 100.0 + 5.0 * 20.0);
        assert_eq!(sale.profit(), Some(250.0));
        assert_eq!(report.holdings.len(), 1);
        assert_eq!(report.holdings[0].scu_left, 5.0);
        assert_eq!(report.holdings[0].cost_left(), 100.0);
    }

    #[test]
    fn keeps_cargo_without_a_purchase_out_of_profit() {
        let report = calculate(&[
            trade(Side::Buy, 1, "silicon", 10.0, 100.0),
            trade(Side::Sell, 2, "silicon", 20.0, 600.0),
            trade(Side::Sell, 3, "scrap", 5.0, 50.0),
        ], &[]);
        assert_eq!(report.sales[0].matched_scu, 10.0);
        assert_eq!(report.sales[0].profit(), Some(200.0));
        assert_eq!(report.sales[1].profit(), None);
        assert_eq!(report.total_profit(), 200.0);
        assert_eq!(report.unmatched_revenue(), 300.0 + 50.0);
    }

    #[test]
    fn commodities_are_matched_separately() {
        let report = calculate(&[
            trade(Side::Buy, 1, "silicon", 10.0, 100.0),
            trade(Side::Sell, 2, "scrap", 10.0, 500.0),
        ], &[]);
        assert_eq!(report.sales[0].profit(), None);
        assert_eq!(report.holdings.len(), 1);
    }

    fn write_off(minute: u32, commodity: &str, scu: f64) -> WriteOff {
        WriteOff {
            id: 1,
            timestamp: DateTime::parse_from_rfc3339(&format!("2026-10-04T06:{minute:02}:00Z"))
                .unwrap()
                .with_timezone(&Utc),
            resource_guid: commodity.into(),
            scu,
            note: None,
            ship_id: None,
        }
    }

    #[test]
    fn destroyed_ship_only_loses_its_own_cargo() {
        let mut lost = write_off(3, "quartz", 16.0);
        lost.ship_id = Some("asgard".into());
        let report = calculate(
            &[
                on_ship(trade(Side::Buy, 1, "quartz", 10.0, 100.0), "cutlass"),
                on_ship(trade(Side::Buy, 2, "quartz", 16.0, 320.0), "asgard"),
            ],
            &[lost],
        );
        assert_eq!(report.losses[0].cost, 320.0);
        assert_eq!(report.holdings.len(), 1);
        assert_eq!(report.holdings[0].buy.ship_id.as_deref(), Some("cutlass"));
    }

    #[test]
    fn sales_use_their_own_ships_cargo_first() {
        let report = calculate(
            &[
                on_ship(trade(Side::Buy, 1, "quartz", 10.0, 100.0), "cutlass"),
                on_ship(trade(Side::Buy, 2, "quartz", 10.0, 200.0), "asgard"),
                on_ship(trade(Side::Sell, 3, "quartz", 15.0, 600.0), "asgard"),
            ],
            &[],
        );
        // All 10 SCU from the Asgard, then 5 of the Cutlass's.
        assert_eq!(report.sales[0].cost, 200.0 + 50.0);
        assert_eq!(report.holdings[0].scu_left, 5.0);
    }

    #[test]
    fn write_off_counts_as_a_loss() {
        let report = calculate(
            &[trade(Side::Buy, 1, "silicon", 10.0, 100.0)],
            &[write_off(2, "silicon", 4.0)],
        );
        assert_eq!(report.losses[0].cost, 40.0);
        assert_eq!(report.total_profit(), -40.0);
        assert_eq!(report.holdings[0].scu_left, 6.0);
    }

    #[test]
    fn later_sales_use_cargo_left_after_a_write_off() {
        let report = calculate(
            &[
                trade(Side::Buy, 1, "silicon", 10.0, 100.0),  // lost
                trade(Side::Buy, 3, "silicon", 10.0, 200.0),  // sold
                trade(Side::Sell, 4, "silicon", 10.0, 500.0),
            ],
            &[write_off(2, "silicon", 10.0)],
        );
        assert_eq!(report.sales[0].cost, 200.0);
        assert_eq!(report.sales[0].profit(), Some(300.0));
        assert_eq!(report.total_profit(), 300.0 - 100.0);
        assert!(report.holdings.is_empty());
    }
}
