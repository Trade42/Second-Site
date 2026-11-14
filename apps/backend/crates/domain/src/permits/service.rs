//! Port of the database-free part of `apps/api/app/domains/permits/service.py`:
//! `normalize_permit` and `PermitIntelligenceService.fetch_and_match` (PropertyIdentity ->
//! matched PermitRecords + SourceEvidence). The persisting `ingest` belongs to `ss-server`.
//!
//! Historical permits stay historical: every limitation string and evidence classification is
//! carried through unchanged; `SOURCE_CONFIRMED` means only that the record exists at the source.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::NaiveDate;
use serde_json::{Map, Value};
use ss_core::{
    new_id, utcnow, Contract, EvidenceClassification, MatchStatus, UtcDateTime, VerificationStatus,
};
use uuid::Uuid;

use super::extract::{category_for, extract_facts};
use crate::contracts::{PermitRecord, PropertyIdentity, SourceEvidence};
use crate::error::DomainError;
use crate::integrations::la_permits::{LaPermitProvider, RawPermit, PROVIDER};
use crate::property::address::parse_street;
use crate::property::identity::{match_record, parsed_identity};
use crate::py;

/// Permit statuses that set `finaled_date` (compared upper-case).
pub const FINALED: &[&str] = &["PERMIT FINALED"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PermitIngestResult {
    pub permits: Vec<PermitRecord>,
    pub evidence: Vec<SourceEvidence>,
    /// `(permit_nbr, reason)`.
    pub rejected: Vec<(String, String)>,
    pub datasets: Vec<String>,
}

fn bad(msg: impl Into<String>) -> DomainError {
    DomainError::BadRecord(msg.into())
}

/// A field used as `str | None`: null/absent is `None`; another JSON type is malformed.
fn opt_str<'a>(r: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, DomainError> {
    match r.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad(format!("{key} is not a string"))),
    }
}

/// `r.get(key, default)` used as a string: absent gives the default, null or another type is
/// malformed (Python would fail on it).
fn str_or<'a>(
    r: &'a Map<String, Value>,
    key: &str,
    default: &'a str,
) -> Result<&'a str, DomainError> {
    match r.get(key) {
        None => Ok(default),
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(bad(format!("{key} is not a string"))),
    }
}

/// `_date`: `datetime.fromisoformat(v).date() if v else None`.
fn date_of(r: &Map<String, Value>, key: &str) -> Result<Option<NaiveDate>, DomainError> {
    match r.get(key) {
        Some(v) if !py::truthy(v) => Ok(None),
        None => Ok(None),
        Some(Value::String(s)) => py::fromisoformat(s)
            .map(|(dt, _)| Some(dt.date()))
            .map_err(bad),
        Some(_) => Err(bad(format!("{key} is not a string"))),
    }
}

/// `_dt`: `datetime.fromisoformat(v).replace(tzinfo=UTC) if v else None` (the wall-clock time
/// is labelled UTC; an explicit offset is discarded, not converted, as in Python).
fn datetime_of(r: &Map<String, Value>, key: &str) -> Result<Option<UtcDateTime>, DomainError> {
    match r.get(key) {
        Some(v) if !py::truthy(v) => Ok(None),
        None => Ok(None),
        Some(Value::String(s)) => py::fromisoformat(s)
            .map(|(dt, _)| Some(UtcDateTime::from(dt.and_utc())))
            .map_err(bad),
        Some(_) => Err(bad(format!("{key} is not a string"))),
    }
}

/// `normalize_permit`: `(record or None, match explanation)`.
pub fn normalize_permit(
    raw: &RawPermit,
    identity: &PropertyIdentity,
) -> Result<(Option<PermitRecord>, String), DomainError> {
    let r = &raw.record;
    let address = str_or(r, "primary_address", "")?;
    let zip = opt_str(r, "zip_code")?;
    let apn = opt_str(r, "apn")?;
    let result = match_record(identity, address, zip, apn)?;
    if !result.matched {
        return Ok((None, result.explanation));
    }
    let category = category_for(opt_str(r, "permit_group")?, opt_str(r, "permit_type")?);
    let status = str_or(r, "status_desc", "UNKNOWN")?;
    let original = match r.get("primary_address") {
        Some(Value::String(s)) => s.as_str(),
        _ => return Err(bad("primary_address missing")),
    };
    let pa = parse_street(
        original,
        None,
        Some(&identity.city),
        Some(&identity.state),
        zip,
    )?;
    let permit_number = match r.get("permit_nbr") {
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(bad("permit_nbr is not a string")),
        None => return Err(bad("permit_nbr missing")),
    };
    let work = str_or(r, "work_desc", "")?;
    let finaled = if FINALED.contains(&py::upper(status).as_str()) {
        date_of(r, "status_date")?
    } else {
        None
    };
    let rec = PermitRecord {
        permit_record_id: new_id(),
        property_id: Some(identity.property_id),
        permit_number,
        permit_category: category,
        permit_status: status.to_owned(),
        issued_date: date_of(r, "issue_date")?,
        finaled_date: finaled,
        work_description: work.to_owned(),
        original_address: original.to_owned(),
        normalized_address: pa.normalized(true),
        unit_or_suite: pa.unit.clone(),
        scope: result.scope,
        parcel_id: apn.map(str::to_owned),
        source_dataset: raw.dataset.clone(),
        source_url: Some(raw.source_url.clone()),
        data_origin: raw.data_origin,
        matched_property_identity: Some(identity.property_id),
        match_status: result.status,
        match_confidence: Some(result.confidence),
        match_explanation: Some(result.explanation.clone()),
        extracted_infrastructure_facts: extract_facts(category, work)?,
        source_evidence_ids: Vec::new(),
    };
    rec.validate()?;
    Ok((Some(rec), result.explanation))
}

/// `PermitIntelligenceService` (fetch and match only).
#[derive(Clone)]
pub struct PermitIntelligenceService {
    pub provider: Arc<dyn LaPermitProvider>,
}

impl PermitIntelligenceService {
    pub fn new(provider: Arc<dyn LaPermitProvider>) -> Self {
        Self { provider }
    }

    pub fn fetch_and_match(
        &self,
        identity: &PropertyIdentity,
        project_id: Option<Uuid>,
    ) -> Result<PermitIngestResult, DomainError> {
        let p = parsed_identity(identity)?;
        let raws = self.provider.search_by_address_prefix(
            &p.house_number,
            p.direction.as_deref(),
            &p.street_name,
            identity.postal_code.as_deref(),
        )?;
        let datasets: BTreeSet<&str> = raws.iter().map(|r| r.dataset.as_str()).collect();
        let mut res = PermitIngestResult {
            datasets: datasets.into_iter().map(str::to_owned).collect(),
            ..Default::default()
        };
        for raw in &raws {
            let (rec, why) = normalize_permit(raw, identity)?;
            let mut rec = match rec {
                Some(rec) => rec,
                None => {
                    let nbr = raw
                        .record
                        .get("permit_nbr")
                        .map(py::value_str)
                        .unwrap_or_else(|| "?".to_owned());
                    res.rejected.push((nbr, why));
                    continue;
                }
            };
            // Evidence that the permit exists as described (record-level).
            let classification =
                if matches!(rec.match_status, MatchStatus::Exact | MatchStatus::Probable) {
                    EvidenceClassification::DocumentedHistorical
                } else {
                    EvidenceClassification::Unknown
                };
            let effective = datetime_of(&raw.record, "status_date")?;
            let mut ev_ids = Vec::new();
            for f in &rec.extracted_infrastructure_facts {
                let mut limitations = f.limitations.clone();
                limitations.push(format!("Permit status at source: {}", rec.permit_status));
                limitations.push(format!(
                    "Match: {}",
                    rec.match_explanation.as_deref().unwrap_or("None")
                ));
                limitations.push(format!("Scope: {}", rec.scope));
                let ev = SourceEvidence {
                    evidence_id: new_id(),
                    project_id,
                    property_id: Some(identity.property_id),
                    provider: PROVIDER.to_owned(),
                    dataset: Some(raw.dataset.clone()),
                    source_record_id: Some(rec.permit_number.clone()),
                    source_url: rec.source_url.clone(),
                    source_asset_reference: None,
                    data_origin: raw.data_origin,
                    retrieved_at: utcnow(),
                    source_effective_at: effective,
                    observation_type: f.fact_type.clone(),
                    extracted_value: f.value.to_json(),
                    unit: f.unit.clone(),
                    evidence_classification: classification,
                    match_confidence: rec.match_confidence,
                    verification_status: VerificationStatus::SourceConfirmed,
                    limitations,
                };
                ev.validate()?;
                ev_ids.push(ev.evidence_id);
                res.evidence.push(ev);
            }
            rec.source_evidence_ids = ev_ids;
            res.permits.push(rec);
        }
        Ok(res)
    }
}
