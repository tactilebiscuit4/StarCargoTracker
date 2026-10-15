//! Resolves commodity GUIDs from Game.log to readable names.
//!
//! Names come from the Star Citizen Wiki API, which is keyed by the same GUIDs
//! the game logs, and are cached in the database so each one is fetched once.

use std::collections::HashSet;
use std::time::Duration;

use crate::db::Db;

const API_URL: &str = "https://api.star-citizen.wiki/api/commodities/";
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Names<'a> {
    db: &'a Db,
    /// GUIDs that failed to resolve this run, so they aren't retried on every trade.
    unavailable: HashSet<String>,
}

impl<'a> Names<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self {
            db,
            unavailable: HashSet::new(),
        }
    }

    /// The commodity's name, or the first part of its GUID if it can't be found.
    pub fn get(&mut self, guid: &str) -> String {
        match self.db.commodity_name(guid) {
            Ok(Some(name)) => return name,
            Ok(None) => {}
            Err(e) => eprintln!("Cannot read commodity names: {e}"),
        }

        if !self.unavailable.contains(guid) {
            match fetch_name(guid) {
                Ok(Some(name)) => {
                    if let Err(e) = self.db.save_commodity_name(guid, &name) {
                        eprintln!("Cannot save commodity name: {e}");
                    }
                    return name;
                }
                Ok(None) => {}
                Err(e) => eprintln!("Cannot look up commodity {guid}: {e}"),
            }
            self.unavailable.insert(guid.to_string());
        }
        guid[..8.min(guid.len())].to_string()
    }
}

/// Returns `Ok(None)` if the wiki doesn't know the GUID.
fn fetch_name(guid: &str) -> Result<Option<String>, String> {
    if !guid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Ok(None);
    }

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .get(format!("{API_URL}{guid}"))
        .header("Accept", "application/json")
        .call()
        .map_err(|e| e.to_string())?;

    match response.status().as_u16() {
        200 => {}
        404 => return Ok(None),
        status => return Err(format!("HTTP {status}")),
    }

    let body = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    Ok(json["data"]["name"].as_str().map(crate::format::clean))
}
