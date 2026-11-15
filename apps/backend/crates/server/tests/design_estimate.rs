//! A listing with no scan of its own suite gets a layout sketch on an assumed floor of its stated
//! area (`LISTED_AREA_ESTIMATE`), labelled everywhere it appears; a measured floor is unchanged.
//! Both stores.

mod common;

use std::collections::HashMap;

use axum::http::StatusCode;
use serde_json::{json, Value};
use ss_core::Contract;
use ss_domain::contracts::geometry::{FloorEstimate, LISTED_AREA_NOT_VERIFIED};
use ss_domain::contracts::{InteriorAsset, SiteGeometry};
use ss_server::services::design::{
    assumed_site, floor_estimate, ASSUMED_COORDINATE_REFERENCE, ESTIMATE_QUANTITY_NOTE,
    NO_FLOOR_NO_AREA,
};
use ss_server::store::repo::{GEOMETRIES, PROPERTIES};
use ss_server::store::Query;
use uuid::Uuid;

use common::*;

const HOLLYWOOD: &str = "6922 W HOLLYWOOD BLVD";
const ALHAMBRA: &str = "5380 E ALHAMBRA AVE";
/// The fixture Alhambra listing's stated area and id (`fixtures/properties/la_candidates.json`).
const ALHAMBRA_AREA: f64 = 148.6;
const ALHAMBRA_LISTING: &str = "fixture-alhambra-1";

fn reqs() -> Value {
    json!([
        {"room_type": "treatment", "count": 3, "min_area_sqm": 10, "min_width_m": 2.8, "needs_plumbing": true},
        {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
        {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5},
    ])
}

struct Demo {
    tok: String,
    pid: String,
    hollywood: String,
    alhambra: String,
}

async fn demo(t: &TestApp) -> Demo {
    let (tok, _) = t.dev_token().await;
    let pid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    let by: HashMap<String, String> = props
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["property_identity"]["street_address"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                p["property_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    Demo {
        tok,
        pid,
        hollywood: by[HOLLYWOOD].clone(),
        alhambra: by[ALHAMBRA].clone(),
    }
}

async fn generate(t: &TestApp, d: &Demo, property: &str, rooms: Value) -> Value {
    let r = t
        .post(
            &format!("/api/v1/projects/{}/layouts/generate", d.pid),
            Some(&d.tok),
            &json!({"property_id": property, "room_requirements": rooms}),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    r.body["task"].clone()
}

async fn layouts(t: &TestApp, d: &Demo) -> Vec<Value> {
    let r = t
        .get(&format!("/api/v1/projects/{}/layouts", d.pid), Some(&d.tok))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    r.body.as_array().unwrap().clone()
}

async fn geometry(t: &TestApp, d: &Demo, id: &str) -> Value {
    let r = t
        .get(
            &format!("/api/v1/projects/{}/layouts/geometry/{id}", d.pid),
            Some(&d.tok),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    r.body
}

/// Change the stored property (its listing or its assets) as an enrichment would.
async fn edit_property(
    t: &TestApp,
    d: &Demo,
    property: &str,
    f: impl FnOnce(&mut ss_domain::contracts::PropertyDigitalProfile),
) {
    let mut tx = t.app.store.begin().await.unwrap();
    let mut prop = PROPERTIES
        .require_in_project(
            &mut *tx,
            property.parse().unwrap(),
            d.pid.parse().unwrap(),
            "property not found",
        )
        .await
        .unwrap();
    f(&mut prop);
    PROPERTIES.upsert(&mut *tx, &prop).await.unwrap();
    tx.commit().await.unwrap();
}

async fn stored_geometries(t: &TestApp, property: &str) -> Vec<SiteGeometry> {
    let mut tx = t.app.store.begin().await.unwrap();
    let id: Uuid = property.parse().unwrap();
    let g = GEOMETRIES
        .list(&mut *tx, Query::new().eq("property_id", id))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    g
}

async fn geometry_reference(t: &TestApp, d: &Demo, property: &str) -> Value {
    let r = t
        .get(
            &format!("/api/v1/projects/{}/properties/{property}", d.pid),
            Some(&d.tok),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    r.body["geometry_reference"].clone()
}

fn assert_label(e: &Value) {
    assert_eq!(e["geometry_source"], "LISTED_AREA_ESTIMATE", "{e}");
    assert_eq!(e["shape_assumed"], true, "{e}");
    assert_eq!(e["area_measured"], false, "{e}");
    assert_eq!(e["listed_area"], json!(ALHAMBRA_AREA), "{e}");
    assert_eq!(e["listed_area_unit"], "sqm", "{e}");
    assert_eq!(
        e["unknown"],
        json!(["columns", "fixed_walls", "plumbing", "windows", "exits"]),
        "{e}"
    );
    assert!(e["shows"]
        .as_str()
        .unwrap()
        .contains("not where rooms can really go"));
}

// ---- the assumed floor --------------------------------------------------------------------------

both_backends!(a_listing_without_a_scan_gets_a_labelled_sketch, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let task = generate(&t, &d, &d.alhambra, reqs()).await;
    assert_eq!(task["status"], "COMPLETED", "{task}");
    let out = &task["output_reference"];
    assert_label(&out["floor_estimate"]);
    let ids = out["design_version_ids"].as_array().unwrap();
    assert!(!ids.is_empty(), "{task}");

    let lv = layouts(&t, &d).await;
    assert_eq!(lv.len(), ids.len());
    let mut geometry_ids = Vec::new();
    for l in &lv {
        assert_eq!(l["approval_status"], "PROPOSED", "{l}");
        assert_eq!(l["validation_report"]["valid"], true, "{l}");
        assert_label(&l["floor_estimate"]);
        geometry_ids.push(l["geometry_id"].as_str().unwrap().to_owned());
    }
    geometry_ids.dedup();
    assert_eq!(
        geometry_ids.len(),
        1,
        "one assumed floor for the generation"
    );

    // The site as the existing route serves it.
    let g = geometry(&t, &d, &geometry_ids[0]).await;
    assert_eq!(g["geometry_source"], "LISTED_AREA_ESTIMATE", "{g}");
    assert_eq!(g["data_origin"], "DERIVED", "{g}");
    assert_eq!(g["scale_verified"], false, "{g}");
    assert_eq!(g["source_id"], ALHAMBRA_LISTING, "{g}");
    assert_eq!(
        g["coordinate_reference"], ASSUMED_COORDINATE_REFERENCE,
        "{g}"
    );
    assert_eq!(g["fixed_walls"], json!([]));
    assert_eq!(g["fixed_obstacles"], json!([]));
    assert_eq!(g["windows"], json!([]));
    assert_eq!(g["exits"], json!([]));
    assert_eq!(g["fixture_locations"], json!([]));
    assert_eq!(g["entrances"].as_array().unwrap().len(), 1);
    let site = SiteGeometry::from_json_value(g.clone()).unwrap();
    let (w, h) = (site.floor_boundary[1].0, site.floor_boundary[2].1);
    assert!((w * h - ALHAMBRA_AREA).abs() < 1e-6, "{w} x {h}");
    assert!(
        (w - (ALHAMBRA_AREA / 1.5).sqrt()).abs() <= 0.005,
        "{w} x {h}"
    );
    let door = &site.entrances[0];
    assert_eq!(door.start.1, 0.0);
    assert!(((door.start.0 + door.end.0) / 2.0 - w / 2.0).abs() < 1e-9);
    // Every room lies inside the listed area.
    let rooms: f64 = lv[0]["room_polygons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["area_sqm"].as_f64().unwrap())
        .sum();
    assert!(rooms <= ALHAMBRA_AREA, "{rooms}");

    // The property is not recorded as having measured geometry.
    assert_eq!(geometry_reference(&t, &d, &d.alhambra).await, Value::Null);

    // A second generation reuses the same assumed floor.
    let again = generate(&t, &d, &d.alhambra, reqs()).await;
    assert_eq!(again["status"], "COMPLETED", "{again}");
    assert_eq!(stored_geometries(&t, &d.alhambra).await.len(), 1);

    // The bill of materials says its quantities come from the sketch.
    let bom = t
        .get(
            &format!(
                "/api/v1/projects/{}/layouts/{}/bom",
                d.pid,
                ids[0].as_str().unwrap()
            ),
            Some(&d.tok),
        )
        .await;
    assert_eq!(bom.status, StatusCode::OK, "{}", bom.text);
    for item in bom.body["items"].as_array().unwrap() {
        assert!(
            item["price_note"]
                .as_str()
                .unwrap()
                .ends_with(ESTIMATE_QUANTITY_NOTE),
            "{item}"
        );
    }
});

both_backends!(a_listing_with_its_own_scan_uses_the_scan, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let task = generate(&t, &d, &d.hollywood, reqs()).await;
    assert_eq!(task["status"], "COMPLETED", "{task}");
    assert!(task["output_reference"].get("floor_estimate").is_none());
    for l in layouts(&t, &d).await {
        assert!(l.get("floor_estimate").is_none(), "{l}");
        let g = geometry(&t, &d, l["geometry_id"].as_str().unwrap()).await;
        assert_ne!(g["geometry_source"], "LISTED_AREA_ESTIMATE", "{g}");
        assert_eq!(g["source_id"], "fixture-matterport-hollywood-302", "{g}");
    }
});

both_backends!(a_scan_that_arrives_later_replaces_the_assumed_floor, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let first = generate(&t, &d, &d.alhambra, reqs()).await;
    assert_eq!(first["status"], "COMPLETED", "{first}");
    // A scan of the suite is matched later (the fixture scan stands in for it).
    edit_property(&t, &d, &d.alhambra, |p| {
        p.interior_assets.push(InteriorAsset {
            provider: "fixture".into(),
            asset_id: "fixture-matterport-hollywood-302".into(),
            asset_type: "matterport_model".into(),
            data_origin: ss_core::DataOrigin::FixtureSynthetic,
            url: None,
            label: None,
            area_sqm: None,
            matches_listing_unit: Some(true),
            has_measured_geometry: true,
        })
    })
    .await;
    let second = generate(&t, &d, &d.alhambra, reqs()).await;
    assert_eq!(second["status"], "COMPLETED", "{second}");
    assert!(second["output_reference"].get("floor_estimate").is_none());
    let stored = stored_geometries(&t, &d.alhambra).await;
    assert_eq!(stored.len(), 2);
    let measured = stored.last().unwrap();
    assert_ne!(measured.geometry_source.as_str(), "LISTED_AREA_ESTIMATE");
    assert_eq!(
        geometry_reference(&t, &d, &d.alhambra).await,
        json!(measured.site_geometry_id.to_string())
    );
    // From now on the measured floor is used, never the assumed one.
    let third = generate(&t, &d, &d.alhambra, reqs()).await;
    assert!(third["output_reference"].get("floor_estimate").is_none());
    assert_eq!(stored_geometries(&t, &d.alhambra).await.len(), 2);
});

// ---- what is refused -----------------------------------------------------------------------------

both_backends!(no_scan_and_no_area_fails_with_the_reason, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    edit_property(&t, &d, &d.alhambra, |p| {
        p.commercial_listing.as_mut().unwrap().available_area_sqm = None
    })
    .await;
    let task = generate(&t, &d, &d.alhambra, reqs()).await;
    assert_eq!(task["status"], "FAILED", "{task}");
    assert_eq!(task["error_code"], "NotFoundError", "{task}");
    assert_eq!(task["error_message"], NO_FLOOR_NO_AREA, "{task}");
    assert!(layouts(&t, &d).await.is_empty());
    assert!(stored_geometries(&t, &d.alhambra).await.is_empty());
});

both_backends!(an_absurd_area_is_refused, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    edit_property(&t, &d, &d.alhambra, |p| {
        p.commercial_listing.as_mut().unwrap().available_area_sqm = Some(1e12)
    })
    .await;
    let task = generate(&t, &d, &d.alhambra, reqs()).await;
    assert_eq!(task["status"], "FAILED", "{task}");
    let msg = task["error_message"].as_str().unwrap();
    assert!(msg.contains("cannot be a suite's floor"), "{msg}");
    assert!(layouts(&t, &d).await.is_empty());
    assert!(stored_geometries(&t, &d.alhambra).await.is_empty());
});

both_backends!(
    a_huge_program_on_a_small_listed_area_fails_saying_assumed,
    |db| {
        let t = TestApp::new(db, &[]);
        let d = demo(&t).await;
        edit_property(&t, &d, &d.alhambra, |p| {
            p.commercial_listing.as_mut().unwrap().available_area_sqm = Some(40.0)
        })
        .await;
        let rooms = json!([{"room_type": "treatment", "count": 12, "min_area_sqm": 12, "min_width_m": 2.8, "needs_plumbing": true}]);
        let task = generate(&t, &d, &d.alhambra, rooms).await;
        assert_eq!(task["status"], "FAILED", "{task}");
        assert_eq!(task["error_code"], "LayoutInfeasibleError", "{task}");
        let msg = task["error_message"].as_str().unwrap();
        assert!(
            msg.starts_with("No layout fits the assumed floor: ")
                && msg.contains("stated 40 m2 (not measured)")
                && msg.ends_with("No layout was stored."),
            "{msg}"
        );
        assert!(layouts(&t, &d).await.is_empty());
    }
);

#[test]
fn non_finite_zero_negative_and_absurd_areas_are_refused() {
    let id = Uuid::new_v4();
    for a in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -5.0,
        0.5,
        1e12,
        10_000.001,
    ] {
        let e = assumed_site(id, "l-1", a).unwrap_err();
        assert!(e.contains("cannot be a suite's floor"), "{a}: {e}");
    }
    // The bounds themselves and ordinary areas are laid out (the solver decides whether rooms
    // fit): exactly the area, short side on the centimetre, ratio 3:2 to within half a
    // centimetre on the short side.
    for a in [1.0, 10_000.0, 148.6, 139.35456, 92.9] {
        let s = assumed_site(id, "l-1", a).unwrap();
        let (w, h) = (s.floor_boundary[1].0, s.floor_boundary[2].1);
        assert!((w * h - a).abs() < 1e-6, "{a}");
        assert!(((w * 100.0).round() - w * 100.0).abs() < 1e-9, "{a}");
        assert!((w - (a / 1.5).sqrt()).abs() <= 0.005 + 1e-12, "{a}");
        assert!(!s.scale_verified);
    }
}

#[test]
fn the_estimate_source_can_never_be_scale_verified() {
    let mut v = serde_json::to_value(assumed_site(Uuid::new_v4(), "l-1", 100.0).unwrap()).unwrap();
    assert!(SiteGeometry::from_json_value(v.clone()).is_ok());
    v["scale_verified"] = json!(true);
    let e = SiteGeometry::from_json_value(v).unwrap_err().to_string();
    assert!(e.contains(LISTED_AREA_NOT_VERIFIED), "{e}");
}

#[test]
fn a_floor_estimate_label_cannot_claim_a_measurement() {
    let ok = serde_json::to_value(floor_estimate(148.6)).unwrap();
    assert!(FloorEstimate::from_json_value(ok.clone()).is_ok());
    for (k, v) in [
        ("area_measured", json!(true)),
        ("shape_assumed", json!(false)),
        ("geometry_source", json!("MATTERPORT_MEASURED")),
        ("listed_area", json!(0)),
    ] {
        let mut bad = ok.clone();
        bad[k] = v;
        assert!(FloorEstimate::from_json_value(bad).is_err(), "{k}");
    }
}
