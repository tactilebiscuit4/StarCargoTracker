//! Exports trades, profit, write-offs and ships to an Excel workbook.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDateTime, Utc};
use rust_xlsxwriter::{Color, Format, FormatBorder, Workbook, Worksheet, XlsxError};

use crate::db::{Db, Ship, Totals};
use crate::locations;
use crate::names::Names;
use crate::parser::Side;
use crate::profit;
use crate::ships;
use crate::tracker::Outcome;

/// What was written, for the summary printed afterwards.
pub struct Exported {
    pub path: PathBuf,
    pub trades: usize,
    pub sales: usize,
    pub write_offs: usize,
}

/// Cell formats shared by every sheet.
struct Formats {
    header: Format,
    label: Format,
    time: Format,
    scu: Format,
    money: Format,
    percent: Format,
    hours: Format,
}

impl Formats {
    fn new() -> Self {
        Self {
            header: Format::new()
                .set_bold()
                .set_font_color(Color::White)
                .set_background_color(Color::RGB(0x1F3B57))
                .set_border_bottom(FormatBorder::Thin),
            label: Format::new().set_bold(),
            time: Format::new().set_num_format("yyyy-mm-dd hh:mm:ss"),
            scu: Format::new().set_num_format("#,##0.00"),
            money: Format::new().set_num_format("#,##0;[Red]-#,##0"),
            percent: Format::new().set_num_format("0.0%;[Red]-0.0%"),
            hours: Format::new().set_num_format("0.0"),
        }
    }
}

/// Where `export` saves with no path given: the Documents folder, named for
/// today, e.g. `Star Cargo Tracker 2026-10-14.xlsx`.
pub fn default_path() -> PathBuf {
    documents_folder().unwrap_or_default().join(default_file_name())
}

fn default_file_name() -> String {
    format!("Star Cargo Tracker {}.xlsx", Local::now().format("%Y-%m-%d"))
}

/// Where to save for a path the user gave: a folder gets the default file
/// name, and `.xlsx` is added if missing.
pub fn resolve_path(given: &Path) -> PathBuf {
    if given.is_dir() {
        return given.join(default_file_name());
    }
    let is_xlsx = given
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("xlsx"));
    if is_xlsx {
        given.to_path_buf()
    } else {
        let mut name = given.as_os_str().to_owned();
        name.push(".xlsx");
        name.into()
    }
}

pub fn export(db: &Db, path: &Path) -> Result<Exported, String> {
    let read = |e: rusqlite::Error| format!("Cannot read trade database: {e}");
    let trades = db.all_trades().map_err(read)?;
    let write_offs = db.write_offs().map_err(read)?;
    let report = profit::calculate(&db.completed_trades().map_err(read)?, &write_offs);
    let totals = db.totals().map_err(read)?;
    let ship_list = db.ships().map_err(read)?;
    let ship_labels: HashMap<&str, String> =
        ship_list.iter().map(|ship| (ship.ship_id.as_str(), ships::label(ship))).collect();
    let ship_label = |id: &Option<String>| -> String {
        match id {
            Some(id) => ship_labels.get(id.as_str()).cloned().unwrap_or_else(|| format!("ship {id}")),
            None => String::new(),
        }
    };

    let f = Formats::new();
    let mut names = Names::new(db);
    let mut workbook = Workbook::new();
    let xlsx = |e: XlsxError| format!("Couldn't build the workbook: {e}");

    // Summary.
    let sheet = workbook.add_worksheet().set_name("Summary").map_err(xlsx)?;
    write_summary(sheet, &report, &totals, &f).map_err(xlsx)?;

    // Every trade.
    let sheet = workbook.add_worksheet().set_name("Trades").map_err(xlsx)?;
    let headers = [
        "ID", "Time", "Side", "Commodity", "SCU", "Total (aUEC)", "Per SCU (aUEC)", "Location", "Ship",
        "Status", "Failure reason",
    ];
    (|| -> Result<(), XlsxError> {
        header(sheet, &headers, &f)?;
        for (i, stored) in trades.iter().enumerate() {
            let row = i as u32 + 1;
            let (trade, status, reason) = match &stored.outcome {
                Outcome::Confirmed(t) if stored.cancelled => (t, "Cancelled", ""),
                Outcome::Confirmed(t) => (t, "Completed", ""),
                Outcome::Failed(t, reason) => (t, "Failed", reason.as_str()),
            };
            sheet.write_number(row, 0, stored.id as f64)?;
            sheet.write_datetime_with_format(row, 1, local(trade.timestamp), &f.time)?;
            sheet.write_string(row, 2, side_name(trade.side))?;
            sheet.write_string(row, 3, names.get(&trade.resource_guid))?;
            sheet.write_number_with_format(row, 4, trade.scu, &f.scu)?;
            sheet.write_number_with_format(row, 5, trade.total_price, &f.money)?;
            if trade.scu > 0.0 {
                sheet.write_number_with_format(row, 6, trade.total_price / trade.scu, &f.money)?;
            }
            sheet.write_string(row, 7, locations::trade_place(db, trade))?;
            sheet.write_string(row, 8, ship_label(&trade.ship_id))?;
            sheet.write_string(row, 9, status)?;
            sheet.write_string(row, 10, reason)?;
        }
        finish(sheet, headers.len(), trades.len())
    })()
    .map_err(xlsx)?;

    // Sales matched to their purchases.
    let sheet = workbook.add_worksheet().set_name("Sales").map_err(xlsx)?;
    let headers = [
        "Sold", "Commodity", "SCU", "Sold at", "Bought at", "Held (hours)", "Cost (aUEC)", "Revenue (aUEC)",
        "Profit (aUEC)", "Margin", "SCU with no recorded purchase", "Revenue not counted (aUEC)",
    ];
    (|| -> Result<(), XlsxError> {
        header(sheet, &headers, &f)?;
        for (i, sale) in report.sales.iter().enumerate() {
            let row = i as u32 + 1;
            let sell = &sale.sell;
            sheet.write_datetime_with_format(row, 0, local(sell.timestamp), &f.time)?;
            sheet.write_string(row, 1, names.get(&sell.resource_guid))?;
            sheet.write_number_with_format(row, 2, sell.scu, &f.scu)?;
            sheet.write_string(row, 3, locations::trade_place(db, sell))?;
            let mut bought_at: Vec<String> = Vec::new();
            for (buy, _) in &sale.sources {
                let place = locations::trade_place(db, buy);
                if !bought_at.contains(&place) {
                    bought_at.push(place);
                }
            }
            sheet.write_string(row, 4, bought_at.join(", "))?;
            if let Some(first) = sale.first_bought() {
                let hours = (sell.timestamp - first).num_seconds() as f64 / 3600.0;
                sheet.write_number_with_format(row, 5, hours, &f.hours)?;
            }
            if let Some(profit) = sale.profit() {
                sheet.write_number_with_format(row, 6, sale.cost, &f.money)?;
                sheet.write_number_with_format(row, 7, sale.matched_revenue(), &f.money)?;
                sheet.write_number_with_format(row, 8, profit, &f.money)?;
                if let Some(margin) = sale.margin() {
                    sheet.write_number_with_format(row, 9, margin, &f.percent)?;
                }
            }
            if sale.unmatched_scu() > 0.0 {
                sheet.write_number_with_format(row, 10, sale.unmatched_scu(), &f.scu)?;
                sheet.write_number_with_format(row, 11, sale.unmatched_revenue(), &f.money)?;
            }
        }
        finish(sheet, headers.len(), report.sales.len())
    })()
    .map_err(xlsx)?;

    // Cargo written off.
    let sheet = workbook.add_worksheet().set_name("Write-offs").map_err(xlsx)?;
    let headers = ["ID", "Time", "Commodity", "SCU", "Cost lost (aUEC)", "Ship", "Note"];
    (|| -> Result<(), XlsxError> {
        header(sheet, &headers, &f)?;
        for (i, loss) in report.losses.iter().enumerate() {
            let row = i as u32 + 1;
            let w = &loss.write_off;
            sheet.write_number(row, 0, w.id as f64)?;
            sheet.write_datetime_with_format(row, 1, local(w.timestamp), &f.time)?;
            sheet.write_string(row, 2, names.get(&w.resource_guid))?;
            sheet.write_number_with_format(row, 3, w.scu, &f.scu)?;
            sheet.write_number_with_format(row, 4, loss.cost, &f.money)?;
            sheet.write_string(row, 5, ship_label(&w.ship_id))?;
            sheet.write_string(row, 6, w.note.as_deref().unwrap_or(""))?;
        }
        finish(sheet, headers.len(), report.losses.len())
    })()
    .map_err(xlsx)?;

    // Cargo not sold or lost yet.
    let sheet = workbook.add_worksheet().set_name("Unsold cargo").map_err(xlsx)?;
    let headers = ["Bought", "Commodity", "SCU", "Bought at", "Cost (aUEC)", "Ship"];
    (|| -> Result<(), XlsxError> {
        header(sheet, &headers, &f)?;
        for (i, lot) in report.holdings.iter().enumerate() {
            let row = i as u32 + 1;
            sheet.write_datetime_with_format(row, 0, local(lot.buy.timestamp), &f.time)?;
            sheet.write_string(row, 1, names.get(&lot.buy.resource_guid))?;
            sheet.write_number_with_format(row, 2, lot.scu_left, &f.scu)?;
            sheet.write_string(row, 3, locations::trade_place(db, &lot.buy))?;
            sheet.write_number_with_format(row, 4, lot.cost_left(), &f.money)?;
            sheet.write_string(row, 5, ship_label(&lot.buy.ship_id))?;
        }
        finish(sheet, headers.len(), report.holdings.len())
    })()
    .map_err(xlsx)?;

    // Ships.
    let sheet = workbook.add_worksheet().set_name("Ships").map_err(xlsx)?;
    let headers = ["ID", "Type", "Name", "Trades", "Last seen", "Destroyed"];
    (|| -> Result<(), XlsxError> {
        header(sheet, &headers, &f)?;
        for (i, ship) in ship_list.iter().enumerate() {
            write_ship(sheet, i as u32 + 1, ship, &f)?;
        }
        finish(sheet, headers.len(), ship_list.len())
    })()
    .map_err(xlsx)?;

    workbook.save(path).map_err(|e| match e {
        XlsxError::IoError(io) if io.raw_os_error() == Some(32) || io.kind() == std::io::ErrorKind::PermissionDenied => {
            format!("Couldn't save {}: it's open in another program (Excel?). Close it and try again.", path.display())
        }
        e => format!("Couldn't save {}: {e}", path.display()),
    })?;

    Ok(Exported {
        path: path.to_path_buf(),
        trades: trades.len(),
        sales: report.sales.len(),
        write_offs: report.losses.len(),
    })
}

fn write_summary(sheet: &mut Worksheet, report: &profit::Report, totals: &Totals, f: &Formats) -> Result<(), XlsxError> {
    let count = Format::new();
    let mut lines: Vec<Option<(&str, f64, &Format)>> = vec![
        Some(("Completed trades", totals.confirmed as f64, &count)),
        Some(("Failed trades", totals.failed as f64, &count)),
        Some(("Cancelled trades", totals.cancelled as f64, &count)),
        None,
        Some(("Spent on cargo (aUEC)", totals.spent, &f.money)),
        Some(("Earned from sales (aUEC)", totals.earned, &f.money)),
        None,
        Some(("Profit from sales (aUEC)", report.sales_profit(), &f.money)),
        Some(("Lost cargo (aUEC)", -report.total_losses(), &f.money)),
        Some(("Total profit (aUEC)", report.total_profit(), &f.money)),
    ];
    if report.unmatched_revenue() > 0.0 {
        lines.push(Some(("Sales with no recorded purchase (aUEC)", report.unmatched_revenue(), &f.money)));
    }

    sheet.write_string_with_format(0, 0, "Exported", &f.label)?;
    sheet.write_datetime_with_format(0, 1, local(Utc::now()), &f.time)?;
    let mut row = 2;
    for line in lines {
        if let Some((label, value, format)) = line {
            sheet.write_string_with_format(row, 0, label, &f.label)?;
            sheet.write_number_with_format(row, 1, value, format)?;
        }
        row += 1;
    }
    sheet.write_string(row + 1, 0, "Times are in your computer's time zone.")?;
    sheet.write_string(
        row + 2,
        0,
        "Profit counts only cargo with a recorded purchase; failed and cancelled trades aren't counted.",
    )?;
    sheet.set_column_width(0, 40)?;
    sheet.set_column_width(1, 20)?;
    Ok(())
}

fn write_ship(sheet: &mut Worksheet, row: u32, ship: &Ship, f: &Formats) -> Result<(), XlsxError> {
    // As text: the IDs are too long to be useful as numbers.
    sheet.write_string(row, 0, &ship.ship_id)?;
    sheet.write_string(row, 1, ship.class.as_deref().map(ships::class_name).unwrap_or_default())?;
    sheet.write_string(row, 2, ship.name.as_deref().unwrap_or(""))?;
    sheet.write_number(row, 3, ship.trades as f64)?;
    if let Some(seen) = ship.last_seen {
        sheet.write_datetime_with_format(row, 4, local(seen), &f.time)?;
    }
    if let Some(destroyed) = ship.destroyed_at {
        sheet.write_datetime_with_format(row, 5, local(destroyed), &f.time)?;
    }
    Ok(())
}

fn header(sheet: &mut Worksheet, headers: &[&str], f: &Formats) -> Result<(), XlsxError> {
    for (col, title) in headers.iter().enumerate() {
        sheet.write_string_with_format(0, col as u16, *title, &f.header)?;
    }
    Ok(())
}

/// Filters on the header row, keeps it in view while scrolling, and sizes the
/// columns to fit.
fn finish(sheet: &mut Worksheet, columns: usize, rows: usize) -> Result<(), XlsxError> {
    sheet.autofilter(0, 0, rows as u32, columns as u16 - 1)?;
    sheet.set_freeze_panes(1, 0)?;
    sheet.autofit();
    Ok(())
}

fn local(time: DateTime<Utc>) -> NaiveDateTime {
    time.with_timezone(&Local).naive_local()
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Buy => "Buy",
        Side::Sell => "Sell",
    }
}

/// The user's Documents folder, wherever Windows keeps it (e.g. in OneDrive).
fn documents_folder() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath};

    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: on success `raw` is a NUL-terminated wide string that we free
    // with CoTaskMemFree, as the API requires (it must be freed on failure too).
    unsafe {
        let ok = SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut raw) >= 0;
        let path = (ok && !raw.is_null()).then(|| {
            let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
            PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(raw, len)))
        });
        CoTaskMemFree(raw as *const _);
        path.or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Documents")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_the_extension_when_missing() {
        assert_eq!(resolve_path(Path::new("trades")), PathBuf::from("trades.xlsx"));
        assert_eq!(resolve_path(Path::new("trades.XLSX")), PathBuf::from("trades.XLSX"));
        assert_eq!(resolve_path(Path::new("trades.v2")), PathBuf::from("trades.v2.xlsx"));
    }

    #[test]
    fn a_folder_gets_the_default_name() {
        let folder = std::env::temp_dir();
        assert_eq!(resolve_path(&folder), folder.join(default_file_name()));
    }

    #[test]
    fn exports_an_empty_database() {
        let db = Db::open_in_memory_for_tests();
        let path = std::env::temp_dir().join(format!("star-cargo-export-test-{}.xlsx", std::process::id()));
        let exported = export(&db, &path).unwrap();
        assert_eq!(exported.trades, 0);
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        std::fs::remove_file(&path).unwrap();
    }
}
