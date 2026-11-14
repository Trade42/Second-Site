//! Port of `apps/api/app/domains/property/enrichment.py`: interior (Matterport) and exterior
//! (Vexcel) enrichment of a `PropertyDigitalProfile`.
//!
//! Truthfulness rules applied here:
//! - A Matterport scan counts as the listed space only when its title names the listed suite.
//!   Other scans in the building are kept as context (`matches_listing_unit = false`) and never
//!   drive layouts. The scan's measured `areaFloor` is kept on the asset; it is never used as the
//!   listing's area.
//! - Vexcel element values are model detections from aerial imagery: `VISUALLY_OBSERVED`, with
//!   their scores as limitations. They describe the building or parcel, not the suite, and are
//!   never measured capacity.
//!
//! Provider failures degrade to an explicit `missing_information` entry naming the error kind;
//! they never fail the search and are never turned into "no data".
//!
//! Both functions are synchronous (they call blocking providers); the workflow runs them on the
//! blocking pool.
//!
//! Differences from Python, on provider data Python cannot handle either: a Matterport area that
//! is not a positive finite number is dropped (Python stores it and later fails to read the
//! property back); a Vexcel element of the wrong JSON type is skipped (Python raises
//! `TypeError` or a `ValidationError` and fails the search).

use chrono::{NaiveDate, Weekday};
use serde_json::{Map, Value};
use ss_core::{utcnow, DataOrigin, EvidenceClassification};
use ss_domain::contracts::{
    ExteriorAsset, InfrastructureObservation, InteriorAsset, PropertyDigitalProfile, ScalarValue,
    SourceManifestEntry,
};
use ss_domain::py;
use uuid::Uuid;

use crate::integrations::matterport::{suite_matches, MatterportProvider};
use crate::integrations::vexcel::VexcelProvider;
use crate::logging;

pub const MAX_MODELS: usize = 40;

/// `_manifest`: replace the provider's manifest entry.
fn manifest(prop: &PropertyDigitalProfile, entry: SourceManifestEntry) -> Vec<SourceManifestEntry> {
    let mut out: Vec<SourceManifestEntry> = prop
        .source_manifest
        .iter()
        .filter(|m| m.provider != entry.provider)
        .cloned()
        .collect();
    out.push(entry);
    out
}

fn warn(msg: &str, err: &dyn std::fmt::Display) {
    logging::warning(
        "app.domains.property.enrichment",
        msg,
        &[("error", Value::String(err.to_string()))],
    );
}

/// `enrich_interior`.
pub fn enrich_interior(
    prop: PropertyDigitalProfile,
    matterport: &dyn MatterportProvider,
) -> PropertyDigitalProfile {
    let mp: Vec<&InteriorAsset> = prop
        .interior_assets
        .iter()
        .filter(|a| a.provider == "matterport")
        .collect();
    if mp.is_empty() || matterport.mode() != "live" {
        return prop;
    }
    let others: Vec<InteriorAsset> = prop
        .interior_assets
        .iter()
        .filter(|a| a.provider != "matterport")
        .cloned()
        .collect();
    let mut missing = prop.missing_information.clone();
    let ids: Vec<String> = mp
        .iter()
        .take(MAX_MODELS)
        .map(|a| a.asset_id.clone())
        .collect();
    let summaries = match matterport.model_summaries(&ids) {
        Ok(s) => s,
        Err(e) => {
            warn("matterport enrichment failed", &e);
            missing.push(format!("Matterport scans not retrieved ({})", e.kind));
            return PropertyDigitalProfile {
                missing_information: missing,
                ..prop
            };
        }
    };
    let suite = prop.property_identity.unit_or_suite.clone();
    let listed_area = prop
        .commercial_listing
        .as_ref()
        .and_then(|l| l.available_area_sqm);
    let mut enriched: Vec<InteriorAsset> = Vec::new();
    for a in &mp {
        // Not accessible with this token.
        let Some(s) = summaries.get(&a.asset_id) else {
            continue;
        };
        let matched = suite_matches(s.name.as_deref(), suite.as_deref());
        enriched.push(InteriorAsset {
            label: s.name.clone(),
            // `s.get("area_sqm") or None`; never the listing's area.
            area_sqm: s.area_sqm.filter(|x| x.is_finite() && *x > 0.0),
            matches_listing_unit: Some(matched),
            has_measured_geometry: matched && s.floor_count > 0,
            data_origin: DataOrigin::Live,
            ..(*a).clone()
        });
    }
    let listed = listed_area.filter(|x| *x != 0.0);
    // Stable sort: suite scans first, then the closest measured area to the listed area.
    enriched.sort_by(|x, y| {
        let key = |a: &InteriorAsset| {
            (
                a.matches_listing_unit != Some(true),
                match listed {
                    Some(l) => (a.area_sqm.unwrap_or(0.0) - l).abs(),
                    None => 0.0,
                },
            )
        };
        let (kx, ky) = (key(x), key(y));
        kx.0.cmp(&ky.0).then(kx.1.total_cmp(&ky.1))
    });
    let matched: Vec<InteriorAsset> = enriched
        .iter()
        .filter(|a| a.matches_listing_unit == Some(true))
        .cloned()
        .collect();
    missing.retain(|m| !m.starts_with("No interior model"));
    if matched.is_empty() {
        let suite_label = suite
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("(unspecified)");
        missing.push(format!(
            "No Matterport scan of suite {suite_label}; {} scan(s) of other spaces in the building",
            enriched.len()
        ));
    }
    let note = if matched.is_empty() {
        "building scans only; listed suite not scanned".to_owned()
    } else {
        format!("{} scan(s) of the listed suite", matched.len())
    };
    let entry = SourceManifestEntry {
        provider: "matterport".into(),
        dataset: Some("model_api".into()),
        status: "LIVE".into(),
        record_count: enriched.len() as i64,
        retrieved_at: Some(utcnow()),
        note: Some(note),
    };
    let mut interior = matched;
    interior.extend(
        enriched
            .iter()
            .filter(|a| a.matches_listing_unit != Some(true))
            .take(6)
            .cloned(),
    );
    interior.extend(others);
    PropertyDigitalProfile {
        interior_assets: interior,
        missing_information: missing,
        source_manifest: manifest(&prop, entry),
        ..prop
    }
}

/// `_obs`: a visual observation of the building.
fn obs(
    prop_id: Uuid,
    typ: &str,
    value: ScalarValue,
    unit: Option<&str>,
    desc: String,
    limitations: &[String],
) -> InfrastructureObservation {
    InfrastructureObservation {
        observation_id: ss_core::new_id(),
        property_id: Some(prop_id),
        observation_type: typ.to_owned(),
        value: Some(value),
        unit: unit.map(str::to_owned),
        classification: EvidenceClassification::VisuallyObserved,
        evidence_ids: Vec::new(),
        limitations: limitations.to_vec(),
        description: desc,
    }
}

/// A JSON value as Pydantic's `float | str | bool` (ints become floats).
fn scalar(v: &Value) -> Option<ScalarValue> {
    match v {
        Value::Bool(b) => Some(ScalarValue::Bool(*b)),
        Value::Number(n) => n.as_f64().map(ScalarValue::Number),
        Value::String(s) => Some(ScalarValue::Text(s.clone())),
        _ => None,
    }
}

fn get<'a>(el: &'a Map<String, Value>, k: &str) -> &'a Value {
    el.get(k).unwrap_or(&Value::Null)
}

/// `datetime.date.fromisoformat` (CPython 3.12): `YYYY-MM-DD`, `YYYYMMDD` and the ISO week forms
/// `YYYY-Www`, `YYYYWww`, `YYYY-Www-D`, `YYYYWwwD`; ASCII digits only, year >= 1.
pub fn date_fromisoformat(s: &str) -> Option<NaiveDate> {
    if !s.is_ascii() {
        return None;
    }
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let part = b.get(r)?;
        if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    let year = num(0..4)? as i32;
    if year < 1 {
        return None;
    }
    let rest = &s[4..];
    let (week_part, extended) = match rest.as_bytes() {
        [b'-', b'W', ..] => (Some(&rest[2..]), true),
        [b'W', ..] => (Some(&rest[1..]), false),
        _ => (None, false),
    };
    if let Some(w) = week_part {
        let wb = w.as_bytes();
        let week: u32 = std::str::from_utf8(wb.get(0..2)?).ok()?.parse().ok()?;
        if !wb[..2].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let day = match (&wb[2..], extended) {
            ([], _) => 1,
            ([b'-', d], true) | ([d], false) if d.is_ascii_digit() => u32::from(*d - b'0'),
            _ => return None,
        };
        let wd = match day {
            1 => Weekday::Mon,
            2 => Weekday::Tue,
            3 => Weekday::Wed,
            4 => Weekday::Thu,
            5 => Weekday::Fri,
            6 => Weekday::Sat,
            7 => Weekday::Sun,
            _ => return None,
        };
        return NaiveDate::from_isoywd_opt(year, week, wd);
    }
    let (month, day) = match b.len() {
        10 if b[4] == b'-' && b[7] == b'-' => (num(5..7)?, num(8..10)?),
        8 => (num(4..6)?, num(6..8)?),
        _ => return None,
    };
    NaiveDate::from_ymd_opt(year, month, day)
}

/// `enrich_exterior`.
pub fn enrich_exterior(
    prop: PropertyDigitalProfile,
    vexcel: &dyn VexcelProvider,
    media_base: &str,
) -> PropertyDigitalProfile {
    let ident = &prop.property_identity;
    let (Some(lat), Some(lon)) = (ident.latitude, ident.longitude) else {
        return prop;
    };
    if vexcel.mode() != "live" {
        return prop;
    }
    let missing: Vec<String> = prop
        .missing_information
        .iter()
        .filter(|m| m.as_str() != "No exterior imagery")
        .cloned()
        .collect();
    let el = match vexcel.property_elements(lat, lon) {
        Ok(el) => el,
        Err(e) => {
            warn("vexcel enrichment failed", &e);
            let mut m = missing;
            m.push(format!("Vexcel imagery not retrieved ({})", e.kind));
            return PropertyDigitalProfile {
                missing_information: m,
                ..prop
            };
        }
    };
    let el = el.filter(|m| !m.is_empty());
    let mut raw_date: Option<Value> = el
        .as_ref()
        .map(|e| get(e, "image_date").clone())
        .filter(py::truthy);
    if raw_date.is_none() {
        // No structure elements here: fall back to the latest urban ortho capture covering the
        // point.
        raw_date = match vexcel.capture_dates(lat, lon) {
            Ok(caps) => caps
                .iter()
                .filter(|c| c.get("layer").and_then(Value::as_str) == Some("urban"))
                .map(|c| match c.get("last-capture-date") {
                    Some(Value::String(s)) => s.clone(),
                    _ => String::new(),
                })
                .max()
                .filter(|s| !s.is_empty())
                .map(Value::String),
            Err(_) => None,
        };
    }
    let img_date = raw_date.as_ref().and_then(|d| {
        let text: String = py::value_str(d).chars().take(10).collect();
        date_fromisoformat(&text)
    });
    let asset = ExteriorAsset {
        provider: "vexcel".into(),
        asset_id: format!("ortho:{lat:.6},{lon:.6}"),
        asset_type: "orthomosaic".into(),
        data_origin: DataOrigin::Live,
        captured_at: img_date,
        url: Some(format!(
            "{media_base}/api/v1/media/vexcel/ortho/{}",
            prop.property_id
        )),
    };
    let mut observations: Vec<InfrastructureObservation> = Vec::new();
    if let Some(el) = &el {
        let mut lim = vec![
            "Automated detection from aerial imagery; describes the building, not the suite."
                .to_owned(),
        ];
        if let Some(dist) = get(el, "structure_distance_m").as_f64() {
            if dist != 0.0 && dist > 3.0 {
                lim.push(format!(
                    "Nearest structure is {dist:.0} m from the geocoded point; it may be a neighbouring building."
                ));
            }
        }
        let collection = match get(el, "collection") {
            v if py::truthy(v) => py::value_str(v),
            _ => String::new(),
        };
        let image_date: String = match get(el, "image_date") {
            v if py::truthy(v) => py::value_str(v).chars().take(10).collect(),
            _ => String::new(),
        };
        let src = format!("Vexcel {collection} {image_date}");
        let src = py::strip(&src).to_owned();
        let pid = prop.property_id;
        let footprint = get(el, "footprint_area_sqm");
        if py::truthy(footprint) {
            if let Some(x) = footprint.as_f64() {
                observations.push(obs(
                    pid,
                    "building_footprint_area",
                    ScalarValue::Number(py::round(x, 1)),
                    Some("m2"),
                    format!("Building footprint from {src}"),
                    &lim,
                ));
            }
        }
        if let Some(v) = scalar(get(el, "roof_condition")) {
            observations.push(obs(
                pid,
                "roof_condition",
                v,
                Some("score_1_5"),
                format!("Roof condition score (1 poor - 5 excellent) from {src}"),
                &lim,
            ));
        }
        let material = get(el, "roof_material");
        if py::truthy(material) {
            if let Some(v) = scalar(material) {
                observations.push(obs(
                    pid,
                    "roof_material",
                    v,
                    None,
                    format!(
                        "Roof material (detector score {})",
                        py::value_str(get(el, "roof_material_score"))
                    ),
                    &lim,
                ));
            }
        }
        if let Some(v) = scalar(get(el, "rooftop_ac_units")) {
            observations.push(obs(
                pid,
                "rooftop_hvac_units",
                v,
                Some("count"),
                "Rooftop HVAC/AC units detected (existing equipment, not spare capacity)".into(),
                &lim,
            ));
        }
        let damage = py::truthy(get(el, "structural_damage_detected"));
        let pooling = py::truthy(get(el, "water_pooling_detected"));
        if damage || pooling {
            let parts: Vec<&str> = [
                if damage { "structural damage" } else { "" },
                if pooling { "water pooling" } else { "" },
            ]
            .into_iter()
            .filter(|k| !k.is_empty())
            .collect();
            observations.push(obs(
                pid,
                "roof_defects",
                ScalarValue::Text(parts.join("; ")),
                None,
                "Roof defects flagged by imagery model; ask landlord about roof repairs".into(),
                &lim,
            ));
        }
    }
    let record_count = 1 + observations.len() as i64;
    let mut exterior: Vec<ExteriorAsset> = prop
        .exterior_assets
        .iter()
        .filter(|a| a.provider != "vexcel")
        .cloned()
        .collect();
    exterior.push(asset);
    let mut building: Vec<InfrastructureObservation> = prop
        .building_observations
        .iter()
        .filter(|o| {
            !(o.description.starts_with("Building footprint from Vexcel")
                || o.description.starts_with("Roof")
                || o.description.starts_with("Rooftop"))
        })
        .cloned()
        .collect();
    building.extend(observations);
    let entry = SourceManifestEntry {
        provider: "vexcel".into(),
        dataset: Some("ortho+property".into()),
        status: "LIVE".into(),
        record_count,
        retrieved_at: Some(utcnow()),
        note: Some(format!(
            "Imagery {}",
            img_date.map_or_else(|| "date unknown".to_owned(), |d| d.to_string())
        )),
    };
    PropertyDigitalProfile {
        exterior_assets: exterior,
        building_observations: building,
        missing_information: missing,
        source_manifest: manifest(&prop, entry),
        ..prop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_fromisoformat_forms() {
        let d = |s| date_fromisoformat(s).map(|d| d.to_string());
        assert_eq!(d("2025-12-01").as_deref(), Some("2025-12-01"));
        assert_eq!(d("20251201").as_deref(), Some("2025-12-01"));
        assert_eq!(d("2025-W01-1").as_deref(), Some("2024-12-30"));
        assert_eq!(d("2025W013").as_deref(), Some("2025-01-01"));
        assert_eq!(d("2025-W01").as_deref(), Some("2024-12-30"));
        for bad in [
            "2025-12-0",
            "2025-13-01",
            "0000-01-01",
            "20251201T1",
            "",
            "2025/12/01",
            "２０２５-12-01",
        ] {
            assert_eq!(date_fromisoformat(bad), None, "{bad}");
        }
    }
}
