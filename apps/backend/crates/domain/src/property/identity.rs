//! Port of `apps/api/app/domains/property/identity.py`: canonical identity and permit/provider
//! record matching.
//!
//! Rules (docs/DECISIONS.md D-07):
//! - Never match on street name alone: house number + street name + street type must agree.
//! - Conflicting street directions (E vs W) are a hard non-match.
//! - Different unit numbers are a non-match (a neighboring suite's permit is not ours).
//! - Building-level records (no unit) attach at BUILDING scope; floor-level records attach at
//!   FLOOR scope with reduced confidence and NEEDS_REVIEW.

use indexmap::IndexMap;
use ss_core::{new_id, Contract, MatchStatus};

use super::address::{parse_street, AddressError, ParsedAddress};
use crate::contracts::{PermitScope, PropertyIdentity};
use crate::error::DomainError;
use crate::py;

#[derive(Debug, Clone, PartialEq)]
pub struct MatchResult {
    pub matched: bool,
    pub status: MatchStatus,
    pub confidence: f64,
    pub scope: PermitScope,
    pub explanation: String,
}

impl MatchResult {
    fn no(status: MatchStatus, scope: PermitScope, explanation: String) -> Self {
        Self {
            matched: false,
            status,
            confidence: 0.0,
            scope,
            explanation,
        }
    }
}

fn truthy(s: Option<&str>) -> Option<&str> {
    s.filter(|x| !x.is_empty())
}

/// `build_identity`. The result is validated like the Pydantic model (state length, coordinate
/// ranges).
#[allow(clippy::too_many_arguments)]
pub fn build_identity(
    street_address: &str,
    unit: Option<&str>,
    city: &str,
    state: &str,
    postal_code: Option<&str>,
    parcel_id: Option<&str>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    provider_ids: Option<IndexMap<String, String>>,
) -> Result<PropertyIdentity, DomainError> {
    let p = parse_street(street_address, unit, Some(city), Some(state), postal_code)?;
    let has_parcel = truthy(parcel_id).is_some();
    let identity = PropertyIdentity {
        property_id: new_id(),
        provider_property_ids: provider_ids.unwrap_or_default(),
        normalized_address: p.normalized(true),
        street_address: p.street_line(),
        unit_or_suite: p.unit.clone(),
        city: city.to_owned(),
        state: py::upper(state),
        postal_code: postal_code.map(str::to_owned),
        parcel_id: parcel_id.map(str::to_owned),
        latitude,
        longitude,
        identity_match_status: if has_parcel {
            MatchStatus::Probable
        } else {
            MatchStatus::NeedsReview
        },
        identity_match_confidence: Some(if has_parcel { 0.8 } else { 0.5 }),
    };
    identity.validate()?;
    Ok(identity)
}

/// `parsed_identity`.
pub fn parsed_identity(identity: &PropertyIdentity) -> Result<ParsedAddress, AddressError> {
    parse_street(
        &identity.street_address,
        identity.unit_or_suite.as_deref(),
        Some(&identity.city),
        Some(&identity.state),
        identity.postal_code.as_deref(),
    )
}

/// `match_record`. Fails only when the target identity's own street cannot be parsed (Python
/// raises `ValueError` there too); an unparseable record address is an UNMATCHED result.
pub fn match_record(
    target: &PropertyIdentity,
    record_address: &str,
    record_zip: Option<&str>,
    record_parcel: Option<&str>,
) -> Result<MatchResult, AddressError> {
    let t = parsed_identity(target)?;
    let r = match parse_street(record_address, None, None, None, None) {
        Ok(r) => r,
        Err(_) => {
            return Ok(MatchResult::no(
                MatchStatus::Unmatched,
                PermitScope::Unknown,
                "unparseable record address".into(),
            ))
        }
    };
    let mut why: Vec<String> = Vec::new();
    if (&t.house_number, &t.street_name, &t.street_type)
        != (&r.house_number, &r.street_name, &r.street_type)
    {
        return Ok(MatchResult::no(
            MatchStatus::Unmatched,
            PermitScope::Unknown,
            format!(
                "street mismatch: {} vs {}",
                py::str_repr(&t.street_line()),
                py::str_repr(&r.street_line())
            ),
        ));
    }
    let mut conf: f64 = 0.6;
    why.push("house number, street name and type agree".into());
    match (
        truthy(t.direction.as_deref()),
        truthy(r.direction.as_deref()),
    ) {
        (Some(td), Some(rd)) => {
            if td != rd {
                return Ok(MatchResult::no(
                    MatchStatus::Conflicting,
                    PermitScope::Unknown,
                    format!("street direction conflict {td} vs {rd}"),
                ));
            }
            conf += 0.15;
            why.push(format!("direction {td} agrees"));
        }
        _ => why.push("direction missing on one side (not used as evidence)".into()),
    }
    if let (Some(tz), Some(rz)) = (truthy(t.postal_code.as_deref()), truthy(record_zip)) {
        if tz != rz {
            return Ok(MatchResult::no(
                MatchStatus::Conflicting,
                PermitScope::Unknown,
                format!("ZIP conflict {tz} vs {rz}"),
            ));
        }
        conf += 0.1;
        why.push("ZIP agrees".into());
    }
    if let (Some(tp), Some(rp)) = (truthy(target.parcel_id.as_deref()), truthy(record_parcel)) {
        if tp != rp {
            return Ok(MatchResult::no(
                MatchStatus::Conflicting,
                PermitScope::Unknown,
                format!("parcel (APN) conflict {tp} vs {rp}"),
            ));
        }
        conf += 0.15;
        why.push("parcel APN agrees".into());
    }

    let scope;
    if let Some(ru) = truthy(r.unit.as_deref()) {
        match t.unit.as_deref() {
            None => {
                return Ok(MatchResult::no(
                    MatchStatus::Unmatched,
                    PermitScope::Unit,
                    format!("record is unit-specific ({ru}); target is the whole building"),
                ))
            }
            Some(tu) if tu != ru => {
                return Ok(MatchResult::no(
                    MatchStatus::Unmatched,
                    PermitScope::Unit,
                    format!("different unit ({ru} vs {tu})"),
                ))
            }
            Some(_) => {}
        }
        scope = PermitScope::Unit;
        why.push(format!("unit {ru} agrees"));
    } else if let Some(rf) = truthy(r.floor.as_deref()) {
        if truthy(t.unit.as_deref()).is_some() && t.implied_floor().as_deref() == Some(rf) {
            scope = PermitScope::Floor;
            conf -= 0.25;
            why.push(format!(
                "floor-level record (floor {rf}); suite floor inferred from unit number — review"
            ));
        } else {
            return Ok(MatchResult::no(
                MatchStatus::Unmatched,
                PermitScope::Floor,
                format!("floor-level record for floor {rf} not tied to target"),
            ));
        }
    } else if let Some(tr) = truthy(r.trailing.as_deref()) {
        scope = PermitScope::Unknown;
        conf -= 0.3;
        why.push(format!(
            "unclassified designator {} — review",
            py::str_repr(tr)
        ));
    } else {
        scope = PermitScope::Building;
        why.push("building-level record (no unit)".into());
    }

    let conf = py::round(conf.clamp(0.0, 1.0), 3);
    let status = if matches!(scope, PermitScope::Floor | PermitScope::Unknown) {
        MatchStatus::NeedsReview
    } else if conf >= 0.9 {
        MatchStatus::Exact
    } else if conf >= 0.7 {
        MatchStatus::Probable
    } else {
        MatchStatus::Ambiguous
    };
    Ok(MatchResult {
        matched: true,
        status,
        confidence: conf,
        scope,
        explanation: why.join("; "),
    })
}
