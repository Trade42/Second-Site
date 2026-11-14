//! Port of `apps/api/app/domains/property/service.py`: Property Scout and digital-profile
//! assembly (`profile_from_listing`, `PropertyScoutService.search`, `attach_permits`).
//! Matterport/Vexcel enrichment (`enrichment.py`) is not part of this crate yet.

use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::{Map, Value};
use ss_core::{utcnow, Contract, DataOrigin};
use uuid::Uuid;

use super::identity::build_identity;
use crate::contracts::{
    CommercialListing, ExteriorAsset, InteriorAsset, PermitRecord, PropertyDigitalProfile,
    PropertySearchRequest, SourceManifestEntry, LISTING_FIELDS,
};
use crate::error::DomainError;
use crate::integrations::costar::{CoStarProvider, RawListing};
use crate::py;

/// `SOURCE_STATUS.get(origin, "LIVE")`.
pub fn source_status(origin: DataOrigin) -> &'static str {
    match origin {
        DataOrigin::FixtureSynthetic => "MOCK",
        DataOrigin::FixtureReal => "SNAPSHOT",
        _ => "LIVE",
    }
}

fn bad(msg: impl Into<String>) -> DomainError {
    DomainError::BadRecord(msg.into())
}

fn req_str<'a>(m: &'a Map<String, Value>, key: &str) -> Result<&'a str, DomainError> {
    match m.get(key) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(bad(format!("{key} is not a string"))),
        None => Err(bad(format!("{key} missing"))),
    }
}

fn opt_str<'a>(m: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, DomainError> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad(format!("{key} is not a string"))),
    }
}

fn opt_f64(m: &Map<String, Value>, key: &str) -> Result<Option<f64>, DomainError> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| bad(format!("{key} is not a number"))),
    }
}

/// Build an asset contract from the keys Python copies (`x["k"]` required, `x.get("k")` optional).
fn asset_value(
    x: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
    origin: DataOrigin,
) -> Result<Value, DomainError> {
    let mut m = Map::new();
    for k in required {
        let v = x.get(*k).ok_or_else(|| bad(format!("asset {k} missing")))?;
        m.insert((*k).to_owned(), v.clone());
    }
    m.insert(
        "data_origin".into(),
        Value::String(origin.as_str().to_owned()),
    );
    for k in optional {
        m.insert((*k).to_owned(), x.get(*k).cloned().unwrap_or(Value::Null));
    }
    Ok(Value::Object(m))
}

/// `profile_from_listing`.
pub fn profile_from_listing(
    project_id: Uuid,
    raw: &RawListing,
) -> Result<PropertyDigitalProfile, DomainError> {
    let a = &raw.address;
    let mut ids = IndexMap::new();
    ids.insert(raw.provider.clone(), raw.provider_listing_id.clone());
    if let Some(cp) = raw
        .listing
        .get("costar_property_id")
        .filter(|v| py::truthy(v))
    {
        ids.insert("costar_property".to_owned(), py::value_str(cp));
    }
    let identity = build_identity(
        req_str(a, "street_address")?,
        opt_str(a, "unit_or_suite")?,
        req_str(a, "city")?,
        req_str(a, "state")?,
        opt_str(a, "postal_code")?,
        opt_str(a, "parcel_id")?,
        opt_f64(a, "latitude")?,
        opt_f64(a, "longitude")?,
        Some(ids),
    )?;

    let mut fields = Map::new();
    for (k, v) in &raw.listing {
        if LISTING_FIELDS.contains(&k.as_str()) {
            if k == "data_origin" {
                // Python: CommercialListing(**{...}, data_origin=...) -> duplicate keyword argument.
                return Err(bad("listing carries its own data_origin"));
            }
            fields.insert(k.clone(), v.clone());
        }
    }
    fields.insert(
        "data_origin".into(),
        Value::String(raw.data_origin.as_str().to_owned()),
    );
    let listing = CommercialListing::from_json_value(Value::Object(fields))?;

    let mut interior = Vec::new();
    for x in &raw.interior_assets {
        let mut v = asset_value(
            x,
            &["provider", "asset_id", "asset_type"],
            &["url", "label", "area_sqm"],
            raw.data_origin,
        )?;
        v["has_measured_geometry"] = x
            .get("has_measured_geometry")
            .cloned()
            .unwrap_or(Value::Bool(false));
        interior.push(InteriorAsset::from_json_value(v)?);
    }
    let mut exterior = Vec::new();
    for x in &raw.exterior_assets {
        let v = asset_value(
            x,
            &["provider", "asset_id", "asset_type"],
            &["url"],
            raw.data_origin,
        )?;
        exterior.push(ExteriorAsset::from_json_value(v)?);
    }

    let mut missing = Vec::new();
    if interior.is_empty() {
        missing.push("No interior model or measured floor plan".to_owned());
    }
    if let Some(service) = listing
        .lease_service_type
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        if listing.nnn_charges_monthly.is_none() || listing.nnn_is_estimate {
            missing.push(format!(
                "Operating-expense charges not quoted ({service} lease){}",
                if listing.nnn_is_estimate {
                    "; estimated from comparable listings"
                } else {
                    ""
                }
            ));
        }
    }
    if exterior.is_empty() {
        missing.push("No exterior imagery".to_owned());
    }
    if listing.existing_buildout.is_none() {
        missing.push("Existing build-out unknown".to_owned());
    }
    let manifest = vec![SourceManifestEntry {
        provider: raw.provider.clone(),
        dataset: Some("listings".into()),
        status: source_status(raw.data_origin).to_owned(),
        record_count: 1,
        retrieved_at: Some(utcnow()),
        note: listing.disclaimer.clone(),
    }];
    let profile = PropertyDigitalProfile {
        property_id: identity.property_id,
        project_id,
        property_identity: identity,
        commercial_listing: Some(listing),
        interior_assets: interior,
        exterior_assets: exterior,
        permit_records: Vec::new(),
        building_observations: Vec::new(),
        geometry_reference: None,
        infrastructure_assessments: Vec::new(),
        missing_information: missing,
        source_manifest: manifest,
        last_analyzed_at: None,
    };
    profile.validate()?;
    Ok(profile)
}

/// `PropertyScoutService`.
#[derive(Clone)]
pub struct PropertyScoutService {
    pub provider: Arc<dyn CoStarProvider>,
}

impl PropertyScoutService {
    pub fn new(provider: Arc<dyn CoStarProvider>) -> Self {
        Self { provider }
    }

    pub fn search(
        &self,
        project_id: Uuid,
        request: &PropertySearchRequest,
    ) -> Result<Vec<PropertyDigitalProfile>, DomainError> {
        self.provider
            .search(request)?
            .iter()
            .map(|r| profile_from_listing(project_id, r))
            .collect()
    }
}

/// `attach_permits`: replace the permit records and the `la_open_data` manifest entry.
/// `origin` is the caller's provider origin string (`"LIVE"` for the live permit provider; the
/// Python workflow passes `"FIXTURE"` otherwise).
pub fn attach_permits(
    prop: &PropertyDigitalProfile,
    permits: Vec<PermitRecord>,
    datasets: &[String],
    origin: &str,
) -> PropertyDigitalProfile {
    let mut manifest: Vec<SourceManifestEntry> = prop
        .source_manifest
        .iter()
        .filter(|m| m.provider != "la_open_data")
        .cloned()
        .collect();
    let joined = datasets.join(",");
    manifest.push(SourceManifestEntry {
        provider: "la_open_data".into(),
        dataset: (!joined.is_empty()).then_some(joined),
        status: if origin == "LIVE" { "LIVE" } else { "MOCK" }.to_owned(),
        record_count: permits.len() as i64,
        retrieved_at: Some(utcnow()),
        note: Some("LADBS permits 2020-present; pre-2020 history not covered".into()),
    });
    let mut missing: Vec<String> = prop
        .missing_information
        .iter()
        .filter(|m| !m.starts_with("No permit"))
        .cloned()
        .collect();
    if permits.is_empty() {
        missing.push("No permit records matched".into());
    }
    PropertyDigitalProfile {
        permit_records: permits,
        source_manifest: manifest,
        missing_information: missing,
        ..prop.clone()
    }
}
