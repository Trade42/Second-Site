//! Interior (Matterport) and exterior (Vexcel) enrichment against Python's own output
//! (`tests-golden/server/enrichment.json`, from `gen_enrichment.py`, which runs Python's
//! `enrich_interior` / `enrich_exterior` with scripted fake providers). Ports
//! `test_interior_enrichment_marks_only_suite_scans` and `test_exterior_enrichment` of
//! `test_sponsor_providers.py` (as vectors, plus their assertions below), and the two kit tests
//! the brief names (`test_unit_letter_is_kept`, `test_missing_net_lease_opex_is_estimated_and_
//! flagged`) at the server level.
//!
//! Ignored in the comparison: `retrieved_at` of the new manifest entry and `observation_id` of
//! new observations (fresh on both sides). Everything else is compared strictly.

mod common;

use std::sync::Mutex;

use indexmap::IndexMap;
use serde_json::{Map, Value};
use ss_core::Contract;
use ss_domain::contracts::{PropertyDigitalProfile, PropertySearchRequest, ProviderStatusEntry};
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};
use ss_domain::integrations::costar::{CoStarProvider, KitCoStarProvider};
use ss_server::integrations::matterport::{MatterportProvider, ModelSummary};
use ss_server::integrations::vexcel::{summarize_property, VexcelProvider};
use ss_server::workflows::enrichment::{enrich_exterior, enrich_interior};

use common::*;

fn kind(s: &str) -> ProviderErrorKind {
    s.parse().expect("error kind")
}

struct FakeMp {
    spec: Value,
    calls: Mutex<Vec<Vec<String>>>,
}

impl MatterportProvider for FakeMp {
    fn mode(&self) -> &str {
        self.spec["mode"].as_str().unwrap_or("live")
    }
    fn get_floor_geometry(&self, _asset_id: &str) -> Result<Option<Value>, ProviderError> {
        Ok(None)
    }
    fn model_summaries(
        &self,
        ids: &[String],
    ) -> Result<IndexMap<String, ModelSummary>, ProviderError> {
        self.calls.lock().unwrap().push(ids.to_vec());
        if let Some(k) = self.spec["error"].as_str() {
            return Err(ProviderError::new("matterport", kind(k), "scripted failure"));
        }
        let mut out = IndexMap::new();
        for (k, v) in self.spec["summaries"].as_object().unwrap() {
            if ids.contains(k) {
                out.insert(
                    k.clone(),
                    ModelSummary {
                        name: v["name"].as_str().map(str::to_owned),
                        area_sqm: v["area_sqm"].as_f64(),
                        floor_count: v["floor_count"].as_u64().unwrap() as usize,
                    },
                );
            }
        }
        Ok(out)
    }
    fn status(&self) -> ProviderStatusEntry {
        unreachable!("not used")
    }
}

struct FakeVx {
    spec: Value,
}

impl VexcelProvider for FakeVx {
    fn mode(&self) -> &str {
        self.spec["mode"].as_str().unwrap_or("live")
    }
    fn ortho_image(&self, _: f64, _: f64, _: i64, _: i64) -> Result<Option<Vec<u8>>, ProviderError> {
        Ok(None)
    }
    fn capture_dates(&self, _: f64, _: f64) -> Result<Vec<Map<String, Value>>, ProviderError> {
        if let Some(k) = self.spec["captures_error"].as_str() {
            return Err(ProviderError::new("vexcel", kind(k), "scripted failure"));
        }
        Ok(self.spec["captures"]
            .as_array()
            .map(|a| a.iter().map(|c| c.as_object().unwrap().clone()).collect())
            .unwrap_or_default())
    }
    fn property_elements(&self, _: f64, _: f64) -> Result<Option<Map<String, Value>>, ProviderError> {
        if let Some(k) = self.spec["elements_error"].as_str() {
            return Err(ProviderError::new("vexcel", kind(k), "scripted failure"));
        }
        Ok(self.spec["properties"]
            .as_object()
            .map(summarize_property))
    }
    fn status(&self) -> ProviderStatusEntry {
        unreachable!("not used")
    }
}

/// Blank the fresh fields: the new manifest entries' `retrieved_at`, new observations' ids.
fn blank(v: &Value, input: &Value) -> Value {
    let mut v = v.clone();
    let old_times: Vec<Value> = input["source_manifest"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["retrieved_at"].clone())
        .collect();
    for m in v["source_manifest"].as_array_mut().unwrap() {
        if !old_times.contains(&m["retrieved_at"]) {
            m["retrieved_at"] = Value::String("<ts>".into());
        }
    }
    for o in v["building_observations"].as_array_mut().unwrap() {
        o["observation_id"] = Value::String("<id>".into());
    }
    v
}

#[test]
fn interior_enrichment_matches_python() {
    let g = golden("enrichment.json");
    let cases = g["interior"].as_array().unwrap();
    assert_eq!(cases.len(), 7);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let input = PropertyDigitalProfile::from_json_value(c["input"].clone()).unwrap();
        let fake = FakeMp {
            spec: c["provider"].clone(),
            calls: Mutex::new(Vec::new()),
        };
        let out = enrich_interior(input, &fake);
        out.validate().unwrap();
        let got = serde_json::to_value(&out).unwrap();
        assert_eq!(
            blank(&got, &c["input"]),
            blank(&c["output"], &c["input"]),
            "{name}"
        );
        let calls = serde_json::to_value(&*fake.calls.lock().unwrap()).unwrap();
        assert_eq!(calls, c["requested"], "{name}: requested model ids");
    }
}

#[test]
fn exterior_enrichment_matches_python() {
    let g = golden("enrichment.json");
    let media = g["media_base"].as_str().unwrap();
    let cases = g["exterior"].as_array().unwrap();
    assert_eq!(cases.len(), 8);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let input = PropertyDigitalProfile::from_json_value(c["input"].clone()).unwrap();
        let fake = FakeVx {
            spec: c["provider"].clone(),
        };
        let once = enrich_exterior(input, &fake, media);
        once.validate().unwrap();
        let got = serde_json::to_value(&once).unwrap();
        assert_eq!(blank(&got, &c["input"]), blank(&c["output"], &c["input"]), "{name}");
        let twice = serde_json::to_value(enrich_exterior(once, &fake, media)).unwrap();
        assert_eq!(
            blank(&twice, &c["input"]),
            blank(&c["output_twice"], &c["input"]),
            "{name} (enriched twice)"
        );
    }
}

/// The assertions of `test_interior_enrichment_marks_only_suite_scans` and
/// `test_exterior_enrichment`, on the Rust output.
#[test]
fn sponsor_enrichment_assertions() {
    let g = golden("enrichment.json");
    let c = &g["interior"][0];
    let fake = FakeMp {
        spec: c["provider"].clone(),
        calls: Mutex::new(Vec::new()),
    };
    let out = enrich_interior(
        PropertyDigitalProfile::from_json_value(c["input"].clone()).unwrap(),
        &fake,
    );
    let mp: Vec<_> = out
        .interior_assets
        .iter()
        .filter(|a| a.provider == "matterport")
        .collect();
    assert!(mp[0].asset_id == "MODEL1" && mp[0].matches_listing_unit == Some(true) && mp[0].has_measured_geometry);
    assert!(mp[1].label.as_deref() == Some("Lobby") && !mp[1].has_measured_geometry);
    assert!(out
        .source_manifest
        .iter()
        .any(|m| m.provider == "matterport" && m.status == "LIVE"));
    // The scan's measured area never becomes the listing's area.
    let listed = out.commercial_listing.as_ref().unwrap().available_area_sqm;
    assert_ne!(listed, mp[0].area_sqm);

    let c = &g["exterior"][0];
    let prop = PropertyDigitalProfile::from_json_value(c["input"].clone()).unwrap();
    let pid = prop.property_id;
    let out = enrich_exterior(
        prop,
        &FakeVx {
            spec: c["provider"].clone(),
        },
        "http://api.test",
    );
    let last = out.exterior_assets.last().unwrap();
    assert_eq!(
        last.url.as_deref(),
        Some(format!("http://api.test/api/v1/media/vexcel/ortho/{pid}").as_str())
    );
    assert_eq!(last.captured_at.unwrap().to_string(), "2025-12-01");
    let hvac = out
        .building_observations
        .iter()
        .find(|o| o.observation_type == "rooftop_hvac_units")
        .unwrap();
    assert_eq!(hvac.value.as_ref().and_then(|v| v.as_f64()), Some(6.0));
    assert_eq!(hvac.classification.as_str(), "VISUALLY_OBSERVED");
    let defects = out
        .building_observations
        .iter()
        .find(|o| o.observation_type == "roof_defects")
        .unwrap();
    assert!(defects.value.as_ref().and_then(|v| v.as_str()).unwrap().contains("water pooling"));
    assert!(!out.missing_information.iter().any(|m| m == "No exterior imagery"));
    // Aerial detections are visual evidence about the building, never measured capacity.
    assert!(out
        .building_observations
        .iter()
        .all(|o| o.classification.as_str() == "VISUALLY_OBSERVED"));
}

/// `test_unit_letter_is_kept`.
#[test]
fn unit_letter_is_kept() {
    use ss_domain::property::address::norm_unit;
    assert_eq!(norm_unit(Some("110 B")).as_deref(), Some("110B"));
    assert_eq!(norm_unit(Some("Ste 110 B")).as_deref(), Some("110B"));
    assert_eq!(norm_unit(Some("302")).as_deref(), Some("302"));
}

/// `test_missing_net_lease_opex_is_estimated_and_flagged`, on the synthetic opex kit domain part 1
/// generated from that Python test (synthetic data, not CoStar data).
#[test]
fn missing_net_lease_opex_is_estimated_and_flagged() {
    let kit = repo_root().join("apps/backend/tests-golden/costar_kit_synthetic_opex");
    let p = KitCoStarProvider::new(&kit);
    let req = PropertySearchRequest {
        limit: 50,
        ..Default::default()
    };
    let res: std::collections::HashMap<String, _> = p
        .search(&req)
        .unwrap()
        .into_iter()
        .map(|r| (r.provider_listing_id.clone(), r))
        .collect();
    let s16 = &res["1:16"].listing;
    assert_eq!(s16["nnn_is_estimate"], Value::Bool(true));
    assert!(s16["disclaimer"].as_str().unwrap().contains("median CAM"));
    assert_eq!(s16["nnn_charges_monthly"], serde_json::json!(105417));
    assert_eq!(res["900:900"].listing["nnn_is_estimate"], Value::Bool(false));
    assert_eq!(res["1:12"].listing["nnn_charges_monthly"], serde_json::json!(0));
}
