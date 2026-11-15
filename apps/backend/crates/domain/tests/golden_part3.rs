//! Golden vectors for part 3's smaller ports (finishes, BOM, catalog, timeline, IFC STEP-text
//! reader), the CPython built-ins fixed in part 3 (part 2 review items F2 and F3), the port of
//! `tests/test_ifc.py::test_mep_extraction_from_fixture`, and JSON Schema conformance of every
//! geometry and procurement contract type.
//!
//! Normalization (identical in `gen_domain3.py`): `bom_id` and `bom_item_id` are dropped from
//! bills of materials; `offer_id` and `retrieved_at` from supplier offers. Nothing else.

mod common;

use chrono::NaiveDate;
use serde_json::Value;
use ss_core::Contract;
use ss_domain::contracts::geometry::{
    DesignRevisionRequest, DesignVersion, FinishSpecification, Fixture, FurniturePlacement,
    GeometryValidationReport, LayoutGenerationRequest, LayoutPreferences, Opening, RoomPolygon,
    RoomRequirement, SiteGeometry, ValidationIssue,
};
use ss_domain::contracts::procurement::{BillOfMaterials, BomItem, SupplierOffer};
use ss_domain::contracts::values::ScalarValue;
use ss_domain::design::geometry::solver::generate_candidates;
use ss_domain::design::interior::finishes::finish_specifications;
use ss_domain::integrations::catalog::{
    FixtureCatalogProvider, ProductCatalogProvider, ILLUSTRATIVE_UNIT_PRICES,
};
use ss_domain::integrations::ifc::{read_mep_elements, read_step_text, status as ifc_status};
use ss_domain::logistics::timeline::opening_timeline;
use ss_domain::procurement::bom::generate_bom;
use ss_domain::py;
use ss_domain::DomainError;

fn g(name: &str) -> Value {
    common::golden(&format!("domain/{name}"))
}

fn without(mut v: Value, keys: &[&str]) -> Value {
    if let Value::Object(m) = &mut v {
        for k in keys {
            m.remove(*k);
        }
    }
    v
}

fn bom_json(bom: &BillOfMaterials) -> Value {
    let mut v = without(common::to_value(bom), &["bom_id"]);
    if let Some(items) = v["items"].as_array_mut() {
        for it in items {
            *it = without(it.take(), &["bom_item_id"]);
        }
    }
    v
}

// ---- part 2 review F2 / F3: CPython built-ins --------------------------------------------------

#[test]
fn str_of_lists_and_unicode_digits_match_cpython() {
    let doc = g("part3_misc.json");
    let p = &doc["py"];
    for c in p["str_of_value"].as_array().unwrap() {
        let v = &c["value"];
        // Dict key order: serde_json maps are sorted without `preserve_order`; the vectors use at
        // most one key per dict, so the text is unaffected.
        assert_eq!(py::value_str(v), c["str"].as_str().unwrap(), "str({v})");
    }
    for c in p["float_of_str"].as_array().unwrap() {
        let s = c["s"].as_str().unwrap();
        match c.get("ok") {
            Some(v) => assert_eq!(py::float_of_str(s), v.as_f64(), "float({s:?})"),
            None => assert_eq!(py::float_of_str(s), None, "float({s:?}) should fail"),
        }
    }
    for c in p["int_of_str"].as_array().unwrap() {
        let s = c["s"].as_str().unwrap();
        match c.get("ok") {
            Some(v) => assert_eq!(py::int_of_str(s).ok(), v.as_i64(), "int({s:?})"),
            None => assert!(py::int_of_str(s).is_err(), "int({s:?}) should fail"),
        }
    }
}

// ---- finishes and BOM ---------------------------------------------------------------------------

#[test]
fn finishes_match_python() {
    for c in g("part3_bom.json")["finishes"].as_array().unwrap() {
        let rooms: Vec<RoomPolygon> = serde_json::from_value(c["rooms"].clone()).unwrap();
        let ours = finish_specifications(&rooms, c["style"].as_str());
        assert_eq!(
            common::to_value(&ours),
            c["finishes"],
            "style {}",
            c["style"]
        );
    }
}

#[test]
fn bill_of_materials_matches_python() {
    let doc = g("part3_bom.json");
    let designs = doc["designs"].as_array().unwrap();
    assert!(designs.len() >= 40, "{} designs", designs.len());
    for c in designs {
        let name = c["name"].as_str().unwrap();
        let dv = DesignVersion::from_json_value(c["design"].clone()).expect(name);
        let bom = generate_bom(&dv).expect(name);
        common::assert_json_eq(&bom_json(&bom), &c["bom"], name);
        // Illustrative prices stay labelled illustrative.
        for it in &bom.items {
            assert_eq!(it.price_origin, ss_core::DataOrigin::FixtureSynthetic);
            assert!(it
                .price_note
                .as_deref()
                .unwrap_or("")
                .starts_with("Illustrative planning price (fixture)"));
        }
    }
    for c in doc["rounding"].as_array().unwrap() {
        let area = c["area"].as_f64().unwrap();
        let q = py::decimal_of_f64(py::round(area * 1.1, 1)).unwrap();
        assert_eq!(
            py::decimal_str(&q),
            c["quantity"].as_str().unwrap(),
            "area {area}"
        );
        let line = (q * rust_decimal::Decimal::from(4500)).trunc();
        assert_eq!(line.to_string(), c["line_total"].to_string(), "area {area}");
    }
    for c in doc["edge_cases"].as_array().unwrap() {
        let dv = DesignVersion::from_json_value(c["design"].clone()).unwrap();
        match (generate_bom(&dv), c.get("ok"), c.get("validation_error")) {
            (Ok(b), Some(theirs), _) => common::assert_json_eq(&bom_json(&b), theirs, "edge"),
            (Err(DomainError::Validation(e)), None, Some(theirs)) => {
                assert_eq!(e.title, "BomItem");
                assert_eq!(e.errors_json(), *theirs);
            }
            (ours, a, b) => panic!("edge case: Rust {ours:?}, Python {a:?} / {b:?}"),
        }
    }
    let prices: Vec<(String, i64)> = doc["illustrative_unit_prices"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_i64().unwrap()))
        .collect();
    let mut ours: Vec<(String, i64)> = ILLUSTRATIVE_UNIT_PRICES
        .iter()
        .map(|(k, v)| ((*k).to_owned(), *v))
        .collect();
    let mut theirs = prices;
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs);
}

// ---- catalog, timeline --------------------------------------------------------------------------

#[test]
fn catalog_and_timeline_match_python() {
    let doc = g("part3_misc.json");
    let cat = &doc["catalog"];
    let provider = FixtureCatalogProvider;
    assert_eq!(provider.mode(), cat["mode"]);
    assert_eq!(common::to_value(&provider.status()), cat["status"]);
    let bom_item: uuid::Uuid = cat["bom_item_id"].as_str().unwrap().parse().unwrap();
    for (item_type, theirs) in cat["offers"].as_object().unwrap() {
        let ours: Vec<Value> = provider
            .offers_for(bom_item, item_type)
            .iter()
            .map(|o| without(common::to_value(o), &["offer_id", "retrieved_at"]))
            .collect();
        assert_eq!(Value::Array(ours), *theirs, "{item_type}");
        for o in provider.offers_for(bom_item, item_type) {
            o.validate().unwrap();
            common::assert_conforms("SupplierOffer", &common::to_value(&o));
        }
    }
    for (start, theirs) in doc["timeline"].as_object().unwrap() {
        let d = NaiveDate::parse_from_str(start, "%Y-%m-%d").unwrap();
        assert_eq!(
            common::to_value(&opening_timeline(d).unwrap()),
            *theirs,
            "{start}"
        );
    }
    assert!(opening_timeline(NaiveDate::from_ymd_opt(9999, 12, 1).unwrap()).is_err());
}

// ---- IFC ----------------------------------------------------------------------------------------

/// Port of `tests/test_ifc.py::test_mep_extraction_from_fixture`.
#[test]
fn test_mep_extraction_from_fixture() {
    let path = common::repo_root().join("fixtures/ifc/mini_mep.ifc");
    let els: std::collections::HashMap<String, _> = read_mep_elements(&path)
        .unwrap()
        .into_iter()
        .map(|e| (e.ifc_type.clone(), e))
        .collect();
    let num = |t: &str, k: &str| match els[t].properties.get(k) {
        Some(ScalarValue::Number(n)) => *n,
        other => panic!("{t} {k}: {other:?}"),
    };
    assert!(
        (num(
            "IFCPIPESEGMENT",
            "Pset_PipeSegmentTypeCommon.NominalDiameter"
        ) - 0.0508)
            .abs()
            < 1e-9
    );
    assert!(
        (num(
            "IFCDUCTSEGMENT",
            "Pset_DuctSegmentOccurrence.NominalAirFlowRate"
        ) - 0.236)
            .abs()
            < 1e-9
    );
    assert_eq!(
        num(
            "IFCELECTRICDISTRIBUTIONBOARD",
            "Pset_ElectricalDeviceCommon.RatedCurrent"
        ),
        200.0
    );
    assert!(els.contains_key("IFCSANITARYTERMINAL"));
    assert!(els.values().all(|e| e.source_file == "mini_mep.ifc")); // provenance preserved
                                                                    // No IfcOpenShell in Rust: the status says the reader is the STEP-text fallback only.
    let st = ifc_status();
    assert_eq!(st.status, ss_core::IntegrationStatus::Partial);
    assert!(st.note.contains("no IfcOpenShell") && st.note.contains("Not a full IFC parser"));
}

#[test]
fn ifc_step_text_reader_matches_python() {
    let doc = g("part3_misc.json");
    let path = common::repo_root().join("fixtures/ifc/mini_mep.ifc");
    assert_eq!(
        common::to_value(&read_mep_elements(&path).unwrap()),
        doc["ifc_fixture"]
    );
    for (name, c) in doc["ifc_step_cases"].as_object().unwrap() {
        let ours = read_step_text(c["text"].as_str().unwrap(), &format!("{name}.ifc")).unwrap();
        assert_eq!(common::to_value(&ours), c["elements"], "{name}");
    }
    // Python raises IndexError on these; Rust returns an error.
    assert!(read_step_text("#1=IFCPROPERTYSINGLEVALUE($,$,IFCLABEL(x),$);\n", "x.ifc").is_err());
    assert!(read_step_text("#1=IFCRELDEFINESBYPROPERTIES($,$,$,$);\n", "x.ifc").is_err());
}

// ---- schema conformance -------------------------------------------------------------------------

#[test]
fn geometry_and_procurement_contracts_conform_to_schemas() {
    let site = SiteGeometry::from_json_value(
        common::read_json("apps/backend/tests-golden/domain/part3_solver.json")[0]["site"].clone(),
    )
    .unwrap();
    let reqs = vec![
        RoomRequirement {
            room_type: "treatment".into(),
            count: 3,
            min_area_sqm: 10.0,
            min_width_m: 2.8,
            needs_plumbing: true,
        },
        RoomRequirement {
            room_type: "reception".into(),
            count: 1,
            min_area_sqm: 12.0,
            min_width_m: 3.0,
            needs_plumbing: false,
        },
    ];
    let prefs = LayoutPreferences::default();
    let cands = generate_candidates(&site, &reqs, &prefs, 2).unwrap();
    let cand = &cands[0];
    let dv = DesignVersion {
        design_version_id: ss_core::new_id(),
        project_id: ss_core::new_id(),
        property_id: site.property_id,
        parent_version_id: None,
        version_number: 1,
        geometry_id: site.site_geometry_id,
        room_polygons: cand.rooms.clone(),
        furniture_placements: vec![FurniturePlacement {
            placement_id: "p-1".into(),
            room_id: cand.rooms[0].room_id.clone(),
            item_type: "treatment_bed".into(),
            footprint: vec![(1.0, 1.0), (2.0, 1.0), (2.0, 3.0), (1.0, 3.0)],
        }],
        proposed_walls: vec![((0.0, 0.0), (1.0, 0.0))],
        finish_specifications: finish_specifications(&cand.rooms, None),
        validation_report: cand.report.clone(),
        estimated_construction_cost: Some(1_000_000),
        created_from_revision_request: None,
        approval_status: ss_domain::contracts::geometry::DesignApprovalStatus::Proposed,
    };
    dv.validate().unwrap();
    let bom = generate_bom(&dv).unwrap();
    let rev = DesignRevisionRequest {
        revision_request_id: ss_core::new_id(),
        project_id: dv.project_id,
        property_id: dv.property_id,
        base_design_version_id: dv.design_version_id,
        requested_changes: vec!["make the reception larger".into()],
        room_requirement_overrides: reqs.clone(),
        preserved_constraints: vec![],
        requested_by: ss_core::new_id(),
        source_channel: "web".into(),
        status: ss_domain::contracts::geometry::RevisionStatus::Received,
    };
    let req = LayoutGenerationRequest {
        property_id: site.property_id,
        site_geometry_id: Some(site.site_geometry_id),
        room_requirements: reqs.clone(),
        preferences: prefs.clone(),
        candidate_count: 2,
    };
    let checks: Vec<(&str, Value)> = vec![
        ("SiteGeometry", common::to_value(&site)),
        ("Opening", common::to_value(&site.entrances[0])),
        (
            "Fixture",
            common::to_value(&Fixture {
                kind: "sink".into(),
                location: (1.0, 2.0),
                evidence_ids: vec![ss_core::new_id()],
            }),
        ),
        ("RoomRequirement", common::to_value(&reqs[0])),
        ("LayoutPreferences", common::to_value(&prefs)),
        ("RoomPolygon", common::to_value(&cand.rooms[0])),
        (
            "FurniturePlacement",
            common::to_value(&dv.furniture_placements[0]),
        ),
        (
            "FinishSpecification",
            common::to_value(&dv.finish_specifications[0]),
        ),
        (
            "ValidationIssue",
            common::to_value(&ValidationIssue::new(
                "ROOM_OVERLAP",
                "a overlaps b",
                vec!["a".into(), "b".into()],
            )),
        ),
        ("GeometryValidationReport", common::to_value(&cand.report)),
        ("DesignVersion", common::to_value(&dv)),
        ("DesignRevisionRequest", common::to_value(&rev)),
        ("LayoutGenerationRequest", common::to_value(&req)),
        ("BomItem", common::to_value(&bom.items[0])),
        ("BillOfMaterials", common::to_value(&bom)),
    ];
    for (name, v) in &checks {
        common::assert_conforms(name, v);
    }
    // The checks have teeth.
    let mut bad = common::to_value(&site);
    bad["units"] = Value::from("ft");
    common::assert_rejected_by_schema("SiteGeometry", &without(bad, &[]));
    let mut bad = common::to_value(&cand.rooms[0]);
    bad["polygon"][0] = serde_json::json!([1.0, 2.0, 3.0]);
    common::assert_rejected_by_schema("RoomPolygon", &bad);
    let mut bad = common::to_value(&bom);
    bad["estimated_total"] = Value::from(1.5);
    common::assert_rejected_by_schema("BillOfMaterials", &bad);
    let _ = (GeometryValidationReport::new(true, vec![]), Opening { kind: "exit".into(), start: (0.0, 0.0), end: (1.0, 0.0) }, FinishSpecification { room_id: None, surface: "floor".into(), material: "m".into(), area_sqm: 1.0 }, BomItem::from_json_value(serde_json::json!({"item_type": "x", "description": "d", "quantity": "1.5", "unit": "ea", "room_id": null})).unwrap(), SupplierOffer::from_json_value(serde_json::json!({"bom_item_id": ss_core::new_id(), "supplier": "s", "product_identifier": "p", "product_name": "n", "price": 1, "data_origin": "FIXTURE_SYNTHETIC"})).unwrap());
}
