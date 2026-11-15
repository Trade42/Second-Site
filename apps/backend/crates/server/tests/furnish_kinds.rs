//! The bill of materials follows the kind of business of the project's stored profile
//! (`orchestrator/prompts/reference/furnish-kinds-spec.md`), through the HTTP routes and on both
//! stores: a dental office, a café, a facial studio (the bill as it was) and a project with no
//! profile. What reads the bill downstream (the fixture catalog's offers, the Nessie move-in
//! plan's furnishing amount) works with the new ids.
//!
//! No test calls a provider: the bank is the in-process fixture.

mod common;

use std::collections::HashMap;

use axum::http::StatusCode;
use serde_json::{json, Value};
use ss_core::Contract;
use ss_domain::contracts::DesignVersion;
use ss_domain::integrations::catalog::{
    illustrative_unit_price, FixtureCatalogProvider, ProductCatalogProvider,
};
use ss_domain::procurement::generate_bom;
use ss_nessie::plan::ITEM_FURNISHING;
use ss_server::services::design::{DesignService, ESTIMATE_QUANTITY_NOTE};
use ss_server::services::nessie::{install, NessieState};
use uuid::Uuid;

use common::*;

const HOLLYWOOD: &str = "6922 W HOLLYWOOD BLVD";
const ALHAMBRA: &str = "5380 E ALHAMBRA AVE";
const NOTE: &str = "Illustrative planning price (fixture), not a supplier quote.";

struct Demo {
    tok: String,
    pid: String,
    hollywood: String,
    alhambra: String,
}

/// The fixture project, its profile saying `business_type` (`None`: the fixture's own
/// `facial_studio`), and the fixture search.
async fn demo(t: &TestApp, business_type: Option<&str>) -> Demo {
    let (tok, _) = t.dev_token().await;
    let mut body = demo_project_body();
    if let Some(b) = business_type {
        body["business_profile"]["business_type"] = json!(b);
    }
    let created = t.create_project(&tok, &body).await;
    assert_eq!(
        created["business_profile"]["business_type"],
        business_type.unwrap_or("facial_studio")
    );
    let pid = created["project"]["project_id"]
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

fn program() -> Value {
    json!([
        {"room_type": "treatment", "count": 3, "min_area_sqm": 10, "min_width_m": 2.8, "needs_plumbing": true},
        {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
        {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5},
    ])
}

/// Generates layouts for the property; returns the first one's document (as the list route
/// gives it) and every design version id made.
async fn layout(t: &TestApp, d: &Demo, property: &str, candidates: i64) -> (Value, Vec<String>) {
    let r = t
        .post(
            &format!("/api/v1/projects/{}/layouts/generate", d.pid),
            Some(&d.tok),
            &json!({"property_id": property, "room_requirements": program(), "candidate_count": candidates}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let ids: Vec<String> = r.body["task"]["output_reference"]["design_version_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let all = t
        .get(&format!("/api/v1/projects/{}/layouts", d.pid), Some(&d.tok))
        .await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.text);
    let doc = all
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["design_version_id"] == ids[0].as_str())
        .unwrap()
        .clone();
    (doc, ids)
}

async fn bom_of(t: &TestApp, d: &Demo, version: &str) -> Value {
    let r = t
        .get(
            &format!("/api/v1/projects/{}/layouts/{version}/bom", d.pid),
            Some(&d.tok),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_conforms("BillOfMaterials", &r.body);
    r.body
}

/// `int(quantity * unit_price)` of one line, from the wire's decimal string, in integers.
fn line_total(item: &Value) -> i64 {
    let q = item["quantity"].as_str().unwrap();
    let (whole, frac) = q.split_once('.').unwrap_or((q, ""));
    let digits: i64 = format!("{whole}{frac}").parse().unwrap();
    let scale = 10i64.pow(frac.len() as u32);
    digits * item["unit_price_estimate"].as_i64().unwrap() / scale
}

/// The bill's furniture is exactly `per_room` for each room of the layout, in its order, with
/// the illustrative prices; flooring follows; the total is the sum of the lines.
fn assert_furnished(bom: &Value, layout: &Value, per_room: &[(&str, &[(&str, i64)])], note: &str) {
    let items = bom["items"].as_array().unwrap();
    let mut expected: Vec<(String, &str, i64)> = Vec::new();
    for room in layout["room_polygons"].as_array().unwrap() {
        let lines = per_room
            .iter()
            .find(|(t, _)| room["room_type"] == *t)
            .map_or(&[][..], |(_, l)| *l);
        for (item_type, quantity) in lines {
            expected.push((
                room["room_id"].as_str().unwrap().to_owned(),
                item_type,
                *quantity,
            ));
        }
    }
    let furniture: Vec<&Value> = items
        .iter()
        .filter(|i| i["item_type"] != "flooring_lvp_sqm")
        .collect();
    let got: Vec<(String, &str, i64)> = furniture
        .iter()
        .map(|i| {
            (
                i["room_id"].as_str().unwrap().to_owned(),
                i["item_type"].as_str().unwrap(),
                i["quantity"].as_str().unwrap().parse().unwrap(),
            )
        })
        .collect();
    assert_eq!(got, expected);
    for i in &furniture {
        let item_type = i["item_type"].as_str().unwrap();
        let room = i["room_id"].as_str().unwrap();
        assert_eq!(
            i["unit_price_estimate"].as_i64(),
            illustrative_unit_price(item_type),
            "{item_type}"
        );
        assert!(i["unit_price_estimate"].as_i64().is_some(), "{item_type}");
        assert_eq!(i["unit"], "ea");
        assert_eq!(
            i["description"],
            format!("{} for {room}", item_type.replace('_', " "))
        );
        assert_eq!(i["design_element_ref"], format!("room:{room}"));
        assert_eq!(i["price_note"], note, "{item_type}");
    }
    let floors = items.len() - furniture.len();
    assert!(floors > 0, "no flooring line");
    assert!(items[furniture.len()..]
        .iter()
        .all(|i| i["item_type"] == "flooring_lvp_sqm"));
    for i in items {
        assert_eq!(i["price_origin"], "FIXTURE_SYNTHETIC", "{i}");
        assert!(
            i["price_note"]
                .as_str()
                .unwrap()
                .contains("not a supplier quote"),
            "{i}"
        );
    }
    let sum: i64 = items.iter().map(line_total).sum();
    assert_eq!(bom["estimated_total"].as_i64(), Some(sum));
    assert_eq!(bom["currency"], "USD");
    assert_eq!(bom["status"], "DRAFT");
}

/// With `FURNISH_PRINT=1`: the bill as `item_type x quantity @ price` lines (for a report).
fn print_bill(name: &str, bom: &Value) {
    if std::env::var_os("FURNISH_PRINT").is_none() {
        return;
    }
    let mut lines: Vec<(String, f64, i64, i64)> = Vec::new();
    for i in bom["items"].as_array().unwrap() {
        let key = i["item_type"].as_str().unwrap().to_owned();
        let q: f64 = i["quantity"].as_str().unwrap().parse().unwrap();
        let price = i["unit_price_estimate"].as_i64().unwrap();
        match lines.iter_mut().find(|l| l.0 == key) {
            Some(l) => {
                l.1 += q;
                l.3 += line_total(i);
            }
            None => lines.push((key, q, price, line_total(i))),
        }
    }
    for (item_type, q, price, total) in lines {
        eprintln!("BILL {name}: {item_type} x {q:.1} @ {price} = {total}");
    }
    eprintln!("BILL {name}: total {}", bom["estimated_total"]);
}

fn room_count(layout: &Value, room_type: &str) -> i64 {
    layout["room_polygons"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["room_type"] == room_type)
        .count() as i64
}

fn without_ids(mut bom: Value) -> Value {
    bom.as_object_mut().unwrap().remove("bom_id");
    for it in bom["items"].as_array_mut().unwrap() {
        it.as_object_mut().unwrap().remove("bom_item_id");
    }
    bom
}

const DENTAL: &[(&str, &[(&str, i64)])] = &[
    (
        "treatment",
        &[
            ("dental_chair", 1),
            ("dentist_stool", 1),
            ("instrument_cabinet", 1),
            ("hand_sink", 1),
        ],
    ),
    ("reception", &[("reception_desk", 1)]),
    ("storage", &[("shelving_unit", 2)]),
];

const CAFE: &[(&str, &[(&str, i64)])] = &[
    ("treatment", &[("cafe_table", 2), ("cafe_chair", 4)]),
    (
        "reception",
        &[
            ("service_counter", 1),
            ("espresso_machine", 1),
            ("display_case", 1),
        ],
    ),
    ("storage", &[("shelving_unit", 2), ("refrigerator", 1)]),
];

const TREATMENT: &[(&str, &[(&str, i64)])] = &[
    (
        "treatment",
        &[
            ("treatment_bed", 1),
            ("storage_cabinet", 1),
            ("task_lighting", 1),
            ("hand_sink", 1),
        ],
    ),
    ("reception", &[("reception_desk", 1)]),
    ("storage", &[("shelving_unit", 2)]),
];

both_backends!(
    a_dental_office_gets_a_dental_bill_and_what_reads_it_works,
    |db| {
        let t = TestApp::new(db, &[]);
        install(&t.app, NessieState::fixture());
        let d = demo(&t, Some("dental_office")).await;
        let (doc, ids) = layout(&t, &d, &d.hollywood, 1).await;
        assert_eq!(room_count(&doc, "treatment"), 3);
        assert_eq!(room_count(&doc, "reception"), 1);
        assert_eq!(room_count(&doc, "storage"), 1);
        let bom = bom_of(&t, &d, &ids[0]).await;
        assert_furnished(&bom, &doc, DENTAL, NOTE);
        print_bill("dental_office", &bom);
        let items = bom["items"].as_array().unwrap();
        for (item_type, n, price) in [
            ("dental_chair", 3, 650000),
            ("dentist_stool", 3, 35000),
            ("instrument_cabinet", 3, 180000),
            ("hand_sink", 3, 60000),
            ("reception_desk", 1, 150000),
            ("shelving_unit", 1, 22000),
            ("treatment_bed", 0, 0),
            ("storage_cabinet", 0, 0),
            ("task_lighting", 0, 0),
        ] {
            let lines: Vec<&Value> = items
                .iter()
                .filter(|i| i["item_type"] == item_type)
                .collect();
            assert_eq!(lines.len(), n, "{item_type}");
            assert!(
                lines.iter().all(|i| i["unit_price_estimate"] == price),
                "{item_type}"
            );
        }
        // The furniture alone: 3 x (6,500 + 350 + 1,800 + 600) + 1,500 + 2 x 220 dollars.
        let furniture: i64 = items
            .iter()
            .filter(|i| i["item_type"] != "flooring_lvp_sqm")
            .map(line_total)
            .sum();
        assert_eq!(furniture, 3 * 925_000 + 150_000 + 44_000);

        // The stored bill is the one served again (no second bill, no change of kind).
        assert_eq!(bom_of(&t, &d, &ids[0]).await, bom);

        // Sourcing: the fixture catalog has one illustrative offer per line, at the line's price.
        for i in items {
            let id: Uuid = i["bom_item_id"].as_str().unwrap().parse().unwrap();
            let item_type = i["item_type"].as_str().unwrap();
            let offers = FixtureCatalogProvider.offers_for(id, item_type);
            assert_eq!(offers.len(), 1, "{item_type}");
            offers[0].validate().unwrap();
            let o = serde_json::to_value(&offers[0]).unwrap();
            assert_conforms("SupplierOffer", &o);
            assert_eq!(o["bom_item_id"], i["bom_item_id"]);
            assert_eq!(o["price"], i["unit_price_estimate"], "{item_type}");
            assert_eq!(o["data_origin"], "FIXTURE_SYNTHETIC");
            assert_eq!(o["supplier"], "Illustrative Supplier (fixture)");
            assert_eq!(o["availability_status"], "UNKNOWN");
            assert_eq!(o["shipping_estimate_days"], Value::Null);
        }

        // The Nessie move-in plan's furnishing amount is this bill's total.
        let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
        let r = t
            .post(
                &format!("/api/v1/projects/{pid}/nessie/sandbox"),
                Some(tok),
                &json!({}),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        let plan = t
            .get(
                &format!(
                    "/api/v1/projects/{pid}/properties/{}/nessie/plan",
                    d.hollywood
                ),
                Some(tok),
            )
            .await;
        assert_eq!(plan.status, StatusCode::OK, "{}", plan.text);
        assert_eq!(
            plan.body["inputs"]["bill_of_materials"]["bom_id"],
            bom["bom_id"]
        );
        assert_eq!(plan.body["plan"]["bom_id"], bom["bom_id"]);
        let furnishing = plan.body["plan"]["payments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["plan_item_id"] == ITEM_FURNISHING)
            .unwrap();
        assert_eq!(furnishing["amount_expected_minor"], bom["estimated_total"]);

        // The sketch on a listed area is furnished by the same kind, its quantities labelled.
        let (sketch, sketch_ids) = layout(&t, &d, &d.alhambra, 1).await;
        assert!(sketch["floor_estimate"].is_object(), "{sketch}");
        let sketch_bom = bom_of(&t, &d, &sketch_ids[0]).await;
        assert_furnished(
            &sketch_bom,
            &sketch,
            DENTAL,
            &format!("{NOTE} {ESTIMATE_QUANTITY_NOTE}"),
        );
    }
);

both_backends!(a_cafe_gets_a_cafe_bill, |db| {
    let t = TestApp::new(db, &[]);
    // `caf`: what the page sends for a café.
    let d = demo(&t, Some("caf")).await;
    let (doc, ids) = layout(&t, &d, &d.hollywood, 1).await;
    let bom = bom_of(&t, &d, &ids[0]).await;
    assert_furnished(&bom, &doc, CAFE, NOTE);
    print_bill("caf", &bom);
    let items = bom["items"].as_array().unwrap();
    let room_type = |i: &Value| {
        doc["room_polygons"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["room_id"] == i["room_id"])
            .unwrap()["room_type"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    for (item_type, room, n, quantity, price) in [
        ("cafe_table", "treatment", 3, "2", 22000),
        ("cafe_chair", "treatment", 3, "4", 9000),
        ("service_counter", "reception", 1, "1", 420000),
        ("espresso_machine", "reception", 1, "1", 850000),
        ("display_case", "reception", 1, "1", 260000),
        ("shelving_unit", "storage", 1, "2", 22000),
        ("refrigerator", "storage", 1, "1", 240000),
    ] {
        let lines: Vec<&Value> = items
            .iter()
            .filter(|i| i["item_type"] == item_type)
            .collect();
        assert_eq!(lines.len(), n, "{item_type}");
        for i in lines {
            assert_eq!(room_type(i), room, "{item_type}");
            assert_eq!(i["quantity"], quantity, "{item_type}");
            assert_eq!(i["unit_price_estimate"], price, "{item_type}");
        }
    }
    for gone in [
        "treatment_bed",
        "hand_sink",
        "reception_desk",
        "task_lighting",
    ] {
        assert!(items.iter().all(|i| i["item_type"] != gone), "{gone}");
    }
    let furniture: i64 = items
        .iter()
        .filter(|i| i["item_type"] != "flooring_lvp_sqm")
        .map(line_total)
        .sum();
    assert_eq!(
        furniture,
        3 * (2 * 22_000 + 4 * 9_000) + 420_000 + 850_000 + 260_000 + 2 * 22_000 + 240_000
    );
});

both_backends!(
    a_facial_studio_and_a_project_without_a_profile_get_the_bill_as_it_was,
    |db| {
        let t = TestApp::new(db, &[]);
        let d = demo(&t, None).await;
        let (doc, ids) = layout(&t, &d, &d.hollywood, 2).await;
        assert!(ids.len() >= 2, "two candidates: {ids:?}");
        let bom = bom_of(&t, &d, &ids[0]).await;
        assert_furnished(&bom, &doc, TREATMENT, NOTE);
        // Byte for byte what `generate_bom` (Python's port) makes of the stored layout, ids apart.
        let dv = DesignVersion::from_json_value(doc.clone()).unwrap();
        let as_it_was = serde_json::to_value(generate_bom(&dv).unwrap()).unwrap();
        assert_eq!(without_ids(bom.clone()), without_ids(as_it_was));
        assert_eq!(
            bom["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|i| i["item_type"] == "treatment_bed")
                .count(),
            3
        );

        // No profile stored: `treatment`. A project made without one cannot search, so the bill of
        // the second candidate is made in its name through the service, and not kept.
        let (tok, _) = t.dev_token().await;
        let bare = t
        .create_project(
            &tok,
            &json!({"project_name": "No profile", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
        assert_eq!(bare["business_profile"], Value::Null);
        let bare_id: Uuid = bare["project"]["project_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let all = t
            .get(&format!("/api/v1/projects/{}/layouts", d.pid), Some(&d.tok))
            .await;
        let second = all
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["design_version_id"] == ids[1].as_str())
            .unwrap()
            .clone();
        let dv2 = DesignVersion::from_json_value(second.clone()).unwrap();
        let mut tx = t.app.store.begin().await.unwrap();
        let made = DesignService::new(&t.app)
            .bill_of_materials(&mut *tx, bare_id, &dv2)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let made = serde_json::to_value(&made).unwrap();
        assert_furnished(&made, &second, TREATMENT, NOTE);
        assert_eq!(
            without_ids(made),
            without_ids(serde_json::to_value(generate_bom(&dv2).unwrap()).unwrap())
        );
    }
);
