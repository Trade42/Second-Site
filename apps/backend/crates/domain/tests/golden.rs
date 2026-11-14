//! Golden-vector parity with the Python implementation. Vectors are produced by
//! `tests-golden/gen_domain.py` running the Python code over the repository fixtures.

mod common;

use std::sync::Arc;

use common::*;
use serde_json::{json, Map, Value};
use ss_core::{BusinessExpansionProfile, Contract};
use ss_domain::contracts::{PermitCategory, PropertyIdentity, PropertySearchRequest};
use ss_domain::integrations::costar::{
    CoStarProvider, FixtureCoStarProvider, KitCoStarProvider, RawListing,
};
use ss_domain::integrations::la_permits::FixtureLaPermitProvider;
use ss_domain::permits::extract::{category_for, extract_facts};
use ss_domain::permits::service::{PermitIngestResult, PermitIntelligenceService};
use ss_domain::property::address::{classify_designator, norm_unit, parse_street, ParsedAddress};
use ss_domain::property::identity::{build_identity, match_record};
use ss_domain::property::service::{attach_permits, profile_from_listing, PropertyScoutService};
use ss_domain::py;
use ss_domain::requirements::templates::{min_area_sqm, requirements_for};
use uuid::Uuid;

fn s(v: &Value) -> Option<&str> {
    v.as_str()
}

fn project_id() -> Uuid {
    Uuid::parse_str(PROJECT_ID).unwrap()
}

fn property_id() -> Uuid {
    Uuid::parse_str(PROPERTY_ID).unwrap()
}

fn parsed_json(p: &ParsedAddress) -> Value {
    json!({
        "house_number": p.house_number,
        "direction": p.direction,
        "street_name": p.street_name,
        "street_type": p.street_type,
        "unit": p.unit,
        "floor": p.floor,
        "trailing": p.trailing,
        "city": p.city,
        "state": p.state,
        "postal_code": p.postal_code,
        "street_line": p.street_line(),
        "normalized": p.normalized(true),
        "normalized_without_unit": p.normalized(false),
        "implied_floor": p.implied_floor(),
    })
}

#[test]
fn python_builtin_semantics() {
    let g = golden("domain/py_semantics.json");
    let floats = g["floats"].as_array().unwrap();
    assert!(floats.len() > 150);
    for f in floats {
        let x = f["x"].as_f64().unwrap();
        assert_eq!(py::float_repr(x), f["repr"].as_str().unwrap(), "repr({x})");
        assert_eq!(
            py::format_g(x),
            f["g"].as_str().unwrap(),
            "format({x}, 'g')"
        );
        assert_eq!(
            py::round(x, 1),
            f["round1"].as_f64().unwrap(),
            "round({x}, 1)"
        );
        assert_eq!(
            py::round(x, 3),
            f["round3"].as_f64().unwrap(),
            "round({x}, 3)"
        );
    }
    for c in g["str_repr"].as_array().unwrap() {
        assert_eq!(
            py::str_repr(c["s"].as_str().unwrap()),
            c["repr"].as_str().unwrap()
        );
    }
    for c in g["title"].as_array().unwrap() {
        assert_eq!(
            py::title(c["s"].as_str().unwrap()),
            c["title"].as_str().unwrap()
        );
    }
}

#[test]
fn address_normalization_matches_python() {
    let g = golden("domain/address.json");
    for l in g["listings"].as_array().unwrap() {
        let a = l["args"].as_array().unwrap();
        let p = parse_street(
            a[0].as_str().unwrap(),
            s(&a[1]),
            s(&a[2]),
            s(&a[3]),
            s(&a[4]),
        )
        .unwrap();
        assert_json_eq(&parsed_json(&p), &l["parsed"], "listing parse");
        let mut ids = indexmap::IndexMap::new();
        ids.insert("fixture".to_owned(), a[8].as_str().unwrap().to_owned());
        let ident = build_identity(
            a[0].as_str().unwrap(),
            s(&a[1]),
            a[2].as_str().unwrap(),
            a[3].as_str().unwrap(),
            s(&a[4]),
            s(&a[5]),
            a[6].as_f64(),
            a[7].as_f64(),
            Some(ids),
        )
        .unwrap();
        let mut v = to_value(&ident);
        v.as_object_mut().unwrap().remove("property_id");
        assert_json_eq(&v, &l["identity"], "build_identity");
    }
    let addresses = g["permit_addresses"].as_array().unwrap();
    assert!(addresses.len() > 117);
    for a in addresses {
        let street = a["street"].as_str().unwrap();
        let expected = &a["parsed"];
        let p = if expected["city"].is_null() {
            parse_street(street, None, None, None, None)
        } else {
            parse_street(street, None, Some("Los Angeles"), Some("ca"), Some("90012"))
        }
        .unwrap();
        assert_json_eq(&parsed_json(&p), expected, street);
    }
    for u in g["units"].as_array().unwrap() {
        assert_eq!(
            norm_unit(s(&u["unit"])).as_deref(),
            s(&u["norm"]),
            "_norm_unit({})",
            u["unit"]
        );
    }
    for d in g["designators"].as_array().unwrap() {
        let (unit, floor, trailing) = classify_designator(d["rest"].as_str().unwrap());
        assert_eq!(json!([unit, floor, trailing]), d["result"], "{}", d["rest"]);
    }
    for e in g["errors"].as_array().unwrap() {
        let err = parse_street(e["street"].as_str().unwrap(), None, None, None, None).unwrap_err();
        assert_eq!(err.to_string(), e["message"].as_str().unwrap());
    }
}

/// The identities the generator matched against (same arguments, in the same order).
fn identities() -> Vec<(String, PropertyIdentity)> {
    let mut out = vec![
        (
            "hollywood_302",
            build_identity(
                "6922 W Hollywood Blvd",
                Some("302"),
                "Los Angeles",
                "CA",
                Some("90028"),
                Some("5548006011"),
                None,
                None,
                None,
            ),
        ),
        (
            "alhambra",
            build_identity(
                "5380 E Alhambra Ave",
                None,
                "Los Angeles",
                "CA",
                Some("90032"),
                Some("5220031007"),
                None,
                None,
                None,
            ),
        ),
        (
            "hollywood_no_direction",
            build_identity(
                "6922 Hollywood Blvd",
                Some("Suite 302"),
                "Los Angeles",
                "CA",
                Some("90028"),
                None,
                None,
                None,
                None,
            ),
        ),
        (
            "hollywood_building",
            build_identity(
                "6922 W Hollywood Blvd",
                None,
                "Los Angeles",
                "CA",
                Some("90028"),
                None,
                None,
                None,
                None,
            ),
        ),
        (
            "hollywood_314",
            build_identity(
                "6922 W Hollywood Blvd",
                Some("314"),
                "Los Angeles",
                "CA",
                None,
                Some("5548006011"),
                None,
                None,
                None,
            ),
        ),
        (
            "nowhere",
            build_identity(
                "1 Nowhere St",
                None,
                "Los Angeles",
                "CA",
                Some("90001"),
                None,
                None,
                None,
                None,
            ),
        ),
    ]
    .into_iter()
    .map(|(k, i)| (k.to_owned(), i.unwrap()))
    .collect::<Vec<_>>();
    let candidates = read_json("fixtures/properties/la_candidates.json");
    for p in candidates["properties"].as_array().unwrap() {
        out.push((
            p["fixture_key"].as_str().unwrap().to_owned(),
            build_identity(
                p["street_address"].as_str().unwrap(),
                s(&p["unit_or_suite"]),
                p["city"].as_str().unwrap(),
                p["state"].as_str().unwrap(),
                s(&p["postal_code"]),
                s(&p["parcel_id"]),
                None,
                None,
                None,
            )
            .unwrap(),
        ));
    }
    for (_, i) in out.iter_mut() {
        i.property_id = property_id();
    }
    out
}

#[test]
fn identity_matching_matches_python_for_every_snapshot_permit() {
    let expected_identities = golden("domain/identities.json");
    let idents = identities();
    for (k, i) in &idents {
        assert_json_eq(&to_value(i), &expected_identities[k], k);
    }
    let vectors = golden("domain/matching.json");
    let vectors = vectors.as_array().unwrap();
    assert!(vectors.len() > 1000);
    for v in vectors {
        let ident = &idents
            .iter()
            .find(|(k, _)| k == v["identity"].as_str().unwrap())
            .unwrap()
            .1;
        let m = match_record(
            ident,
            v["address"].as_str().unwrap(),
            s(&v["zip"]),
            s(&v["apn"]),
        )
        .unwrap();
        let ours = json!({
            "identity": v["identity"], "address": v["address"], "zip": v["zip"], "apn": v["apn"],
            "matched": m.matched, "status": m.status, "confidence": m.confidence, "scope": m.scope,
            "explanation": m.explanation,
        });
        assert_json_eq(&ours, v, "match_record");
    }
}

#[test]
fn permit_extraction_matches_python() {
    let g = golden("domain/extraction.json");
    let facts = g["facts"].as_array().unwrap();
    assert!(facts.len() > 400);
    for f in facts {
        let cat: PermitCategory = f["category"].as_str().unwrap().parse().unwrap();
        let got = extract_facts(cat, f["text"].as_str().unwrap()).unwrap();
        assert_json_eq(&to_value(&got), &f["facts"], f["text"].as_str().unwrap());
    }
    for c in g["categories"].as_array().unwrap() {
        assert_eq!(
            category_for(s(&c["group"]), s(&c["type"])).as_str(),
            c["category"].as_str().unwrap()
        );
    }
    for f in g["failures"].as_array().unwrap() {
        let cat: PermitCategory = f["category"].as_str().unwrap().parse().unwrap();
        assert!(
            extract_facts(cat, f["text"].as_str().unwrap()).is_err(),
            "{f}"
        );
    }
}

fn fetch_json(res: &PermitIngestResult) -> Value {
    let ev_ids: Vec<String> = res
        .evidence
        .iter()
        .map(|e| e.evidence_id.to_string())
        .collect();
    let doc = json!({
        "permits": res.permits,
        "evidence": res.evidence,
        "rejected": res.rejected.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
        "datasets": res.datasets,
    });
    normalize(&doc, None, &ev_ids)
}

fn permit_service() -> PermitIntelligenceService {
    PermitIntelligenceService::new(Arc::new(FixtureLaPermitProvider::new().unwrap()))
}

#[test]
fn permit_fetch_and_match_matches_python() {
    let g = golden("domain/permits_fetch.json");
    let svc = permit_service();
    let idents = identities();
    for (k, ident) in &idents {
        let res = svc.fetch_and_match(ident, Some(project_id())).unwrap();
        assert_json_eq(&fetch_json(&res), &g[k], k);
    }
    let alhambra = &idents.iter().find(|(k, _)| k == "alhambra").unwrap().1;
    let res = svc.fetch_and_match(alhambra, None).unwrap();
    assert_json_eq(&fetch_json(&res), &g["alhambra_no_project"], "no project");
}

fn request(v: &Value) -> PropertySearchRequest {
    PropertySearchRequest::from_json_value(v.clone()).unwrap()
}

#[test]
fn property_profiles_match_python() {
    let g = golden("domain/property_profiles.json");
    let scout = PropertyScoutService::new(Arc::new(FixtureCoStarProvider::new()));
    let svc = permit_service();
    let mut total = 0;
    for (name, case) in g.as_object().unwrap() {
        let req = request(&case["request"]);
        let profiles = scout.search(project_id(), &req).unwrap();
        let expected = case["profiles"].as_array().unwrap();
        assert_eq!(profiles.len(), expected.len(), "{name}");
        for (prof, exp) in profiles.iter().zip(expected) {
            let pid = prof.property_id.to_string();
            assert_eq!(prof.property_identity.property_id, prof.property_id);
            assert_json_eq(
                &normalize(&to_value(prof), Some(&pid), &[]),
                &exp["search_profile"],
                name,
            );
            let res = svc
                .fetch_and_match(&prof.property_identity, Some(project_id()))
                .unwrap();
            let full = attach_permits(prof, res.permits.clone(), &res.datasets, "FIXTURE");
            full.validate().unwrap();
            let ev_ids: Vec<String> = res
                .evidence
                .iter()
                .map(|e| e.evidence_id.to_string())
                .collect();
            for p in &full.permit_records {
                assert_eq!(p.property_id, Some(prof.property_id));
                assert_eq!(p.matched_property_identity, Some(prof.property_id));
            }
            assert_json_eq(
                &normalize(&to_value(&full), Some(&pid), &ev_ids),
                &exp["profile"],
                name,
            );
            assert_json_eq(
                &normalize(&to_value(&res.evidence), Some(&pid), &ev_ids),
                &exp["evidence"],
                name,
            );
            total += 1;
        }
    }
    assert!(total >= 5, "{total}");
}

fn raw_json(r: &RawListing) -> Value {
    json!({
        "provider": r.provider,
        "provider_listing_id": r.provider_listing_id,
        "data_origin": r.data_origin,
        "address": r.address,
        "listing": r.listing,
        "interior_assets": r.interior_assets,
        "exterior_assets": r.exterior_assets,
    })
}

#[test]
fn costar_kit_provider_matches_python_on_the_synthetic_kit() {
    let g = golden("domain/costar_kit.json");
    for (label, dir) in [
        ("kit", "costar_kit_synthetic"),
        ("opex", "costar_kit_synthetic_opex"),
    ] {
        let prov = KitCoStarProvider::new(golden_dir(dir));
        for (name, case) in g[label].as_object().unwrap() {
            if name == "status" {
                assert_json_eq(&to_value(&prov.status()), case, "status");
                continue;
            }
            let res = prov.search(&request(&case["request"])).unwrap();
            assert_eq!(
                prov.skipped_no_rate() as u64,
                case["skipped_no_rate"].as_u64().unwrap(),
                "{label}/{name}"
            );
            let ours: Vec<Value> = res.iter().map(raw_json).collect();
            assert_json_eq(
                &Value::Array(ours),
                &case["listings"],
                &format!("{label}/{name}"),
            );
            let profiles: Vec<Value> = res
                .iter()
                .map(|r| {
                    let p = profile_from_listing(project_id(), r).unwrap();
                    normalize(&to_value(&p), Some(&p.property_id.to_string()), &[])
                })
                .collect();
            assert_json_eq(
                &Value::Array(profiles),
                &case["profiles"],
                &format!("{label}/{name} profiles"),
            );
        }
    }
    let missing = KitCoStarProvider::new(golden_dir("costar_kit_does_not_exist"));
    assert_json_eq(
        &to_value(&missing.status()),
        &g["missing_status"],
        "missing status",
    );
}

#[test]
fn requirement_templates_match_python() {
    let g = golden("domain/templates.json");
    for (name, case) in g.as_object().unwrap() {
        let prof = BusinessExpansionProfile::from_json_value(case["profile"].clone()).unwrap();
        assert_eq!(
            min_area_sqm(&prof),
            case["min_area_sqm"].as_f64().unwrap(),
            "{name}"
        );
        let reqs = requirements_for(&prof);
        assert_json_eq(&to_value(&reqs), &case["requirements"], name);
        for r in &reqs {
            assert_conforms("InfrastructureRequirement", &to_value(r));
        }
    }
}

#[test]
fn golden_files_never_claim_synthetic_data_is_costar() {
    // The fixture listings stay `fixture` / FIXTURE_SYNTHETIC end to end.
    let g = golden("domain/property_profiles.json");
    for case in g.as_object().unwrap().values() {
        for p in case["profiles"].as_array().unwrap() {
            let l = &p["profile"]["commercial_listing"];
            assert_eq!(l["provider"], "fixture");
            assert_eq!(l["data_origin"], "FIXTURE_SYNTHETIC");
            assert_eq!(p["profile"]["source_manifest"][0]["status"], "MOCK");
            for permit in p["profile"]["permit_records"].as_array().unwrap() {
                assert_eq!(permit["data_origin"], "FIXTURE_REAL");
            }
        }
    }
    let readme =
        std::fs::read_to_string(golden_dir("costar_kit_synthetic").join("README.md")).unwrap();
    assert!(readme.starts_with("SYNTHETIC TEST DATA"));
    let _unused: Map<String, Value> = Map::new();
}
