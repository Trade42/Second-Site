//! Port of `apps/api/app/integrations/la_permits/provider.py`: the provider trait, `RawPermit`,
//! `address_prefix` and the fixture provider over the committed real LADBS snapshot.
//!
//! The live Socrata provider is not part of this crate (it needs HTTP); `soql_escape` and
//! `address_prefix` are exported for it.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value};
use ss_core::{DataOrigin, IntegrationStatus};

use super::base::{fixtures_dir, status_entry, ProviderError, ProviderErrorKind};
use crate::contracts::ProviderStatusEntry;
use crate::py;

pub const PROVIDER: &str = "la_open_data";

/// One permit row as returned by the source, unmodified.
#[derive(Debug, Clone, PartialEq)]
pub struct RawPermit {
    pub dataset: String,
    pub record: Map<String, Value>,
    pub data_origin: DataOrigin,
    pub source_url: String,
}

/// `LAPermitProvider` protocol. Synchronous: implementations here are file-based.
pub trait LaPermitProvider: Send + Sync {
    fn mode(&self) -> &str;

    fn search_by_address_prefix(
        &self,
        house_number: &str,
        direction: Option<&str>,
        street_name: &str,
        zip_code: Option<&str>,
    ) -> Result<Vec<RawPermit>, ProviderError>;

    fn get_permit(&self, permit_number: &str) -> Result<Vec<RawPermit>, ProviderError>;

    fn status(&self) -> ProviderStatusEntry;
}

/// `_soql_escape`.
pub fn soql_escape(value: &str) -> String {
    value.replace('\'', "''")
}

/// `address_prefix`: upper-cased `house [direction] street`.
pub fn address_prefix(house_number: &str, direction: Option<&str>, street_name: &str) -> String {
    let mut parts = vec![house_number];
    if let Some(d) = direction.filter(|d| !d.is_empty()) {
        parts.push(d);
    }
    parts.push(street_name);
    parts
        .iter()
        .map(|p| py::upper(p))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Deserialize)]
struct Snapshot {
    records: IndexMap<String, Vec<Map<String, Value>>>,
}

/// Serves the committed real snapshot (`fixtures/permits/la_permits_snapshot.json`), labelled
/// `FIXTURE_REAL`. Datasets keep the snapshot's order, rows keep file order.
#[derive(Debug, Clone)]
pub struct FixtureLaPermitProvider {
    pub path: PathBuf,
    rows: Vec<(String, Map<String, Value>)>,
}

impl FixtureLaPermitProvider {
    pub fn default_path() -> PathBuf {
        fixtures_dir()
            .join("permits")
            .join("la_permits_snapshot.json")
    }

    /// Load the default snapshot. Fails (as Python's constructor does) when it is unreadable.
    pub fn new() -> Result<Self, ProviderError> {
        Self::from_path(Self::default_path())
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ProviderError> {
        let path = path.as_ref().to_path_buf();
        let bad = |m: String| ProviderError::new(PROVIDER, ProviderErrorKind::NotConfigured, m);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| bad(format!("permit snapshot {}: {e}", path.display())))?;
        let doc: Snapshot = serde_json::from_str(&text)
            .map_err(|e| bad(format!("permit snapshot {}: {e}", path.display())))?;
        let rows = doc
            .records
            .into_iter()
            .flat_map(|(ds, rows)| rows.into_iter().map(move |r| (ds.clone(), r)))
            .collect();
        Ok(Self { path, rows })
    }

    fn wrap(&self, ds: &str, r: &Map<String, Value>) -> Result<RawPermit, ProviderError> {
        let nbr = r.get("permit_nbr").ok_or_else(|| {
            ProviderError::new(
                PROVIDER,
                ProviderErrorKind::BadResponse,
                "snapshot row without permit_nbr",
            )
        })?;
        Ok(RawPermit {
            dataset: ds.to_owned(),
            record: r.clone(),
            data_origin: DataOrigin::FixtureReal,
            source_url: format!(
                "https://data.lacity.org/resource/{ds}.json?permit_nbr={}",
                py::value_str(nbr)
            ),
        })
    }

    fn bad_row(msg: &str) -> ProviderError {
        ProviderError::new(PROVIDER, ProviderErrorKind::BadResponse, msg)
    }
}

impl LaPermitProvider for FixtureLaPermitProvider {
    fn mode(&self) -> &str {
        "fixture"
    }

    fn search_by_address_prefix(
        &self,
        house_number: &str,
        direction: Option<&str>,
        street_name: &str,
        zip_code: Option<&str>,
    ) -> Result<Vec<RawPermit>, ProviderError> {
        let prefix = address_prefix(house_number, direction, street_name);
        let mut out = Vec::new();
        for (ds, r) in &self.rows {
            let addr = match r.get("primary_address") {
                Some(Value::String(s)) => s,
                _ => {
                    return Err(Self::bad_row(
                        "snapshot row without a primary_address string",
                    ))
                }
            };
            let zip_ok = match zip_code {
                None => true,
                Some(z) => matches!(r.get("zip_code"), Some(Value::String(s)) if s == z),
            };
            if py::upper(addr).starts_with(&prefix) && zip_ok {
                out.push(self.wrap(ds, r)?);
            }
        }
        Ok(out)
    }

    fn get_permit(&self, permit_number: &str) -> Result<Vec<RawPermit>, ProviderError> {
        let mut out = Vec::new();
        for (ds, r) in &self.rows {
            match r.get("permit_nbr") {
                Some(Value::String(s)) if s == permit_number => out.push(self.wrap(ds, r)?),
                Some(_) => {}
                None => return Err(Self::bad_row("snapshot row without permit_nbr")),
            }
        }
        Ok(out)
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            PROVIDER,
            self.mode(),
            IntegrationStatus::Mock,
            false,
            "Real public records snapshot (FIXTURE_REAL); set PERMIT_PROVIDER_MODE=live for live API.",
        )
    }
}
