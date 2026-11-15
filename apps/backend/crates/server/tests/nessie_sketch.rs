//! The move-in payment plan of a property whose bill of materials was priced from an approximate
//! sketch on the listed area (the layout review's F1): the furnishing line says that it comes from
//! a sketch, the amount is the bill's, a bill on the fixture's measured floor reads as before, and
//! a bill whose design version is not in the project reads as unknown, never as measured.

mod common;

use std::collections::HashMap;

use axum::http::StatusCode;
use serde_json::{json, Value};
use ss_nessie::plan::{
    BASIS_BOM_FLOOR_UNKNOWN, BASIS_BOM_SKETCH, ITEM_FURNISHING, NOTE_BOM_FLOOR_UNKNOWN,
    NOTE_BOM_SKETCH,
};
use ss_server::services::nessie::{install, NessieState};
use ss_server::store::repo::BOMS;
use uuid::Uuid;

use common::*;

const HOLLYWOOD: &str = "6922 W HOLLYWOOD BLVD";
const ALHAMBRA: &str = "5380 E ALHAMBRA AVE";
const MEASURED_BASIS: &str =
    "bill of materials, estimated total (not approved; illustrative prices)";
const OVERLAP_NOTE: &str = "The bill of materials may price items (for example flooring or hand sinks) that renovation work items also cover; check for overlap.";

struct Demo {
    tok: String,
    pid: String,
    /// The fixture's measured floor.
    hollywood: String,
    /// 148.6 m2 stated, no scan: its layout is a sketch on the listed area.
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

/// Generates a layout and its bill of materials; returns the task output and the bill.
async fn layout_with_bom(t: &TestApp, d: &Demo, property: &str) -> (Value, Value) {
    let rooms = json!([
        {"room_type": "treatment", "count": 3, "min_area_sqm": 10, "min_width_m": 2.8, "needs_plumbing": true},
        {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
        {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5},
    ]);
    let r = t
        .post(
            &format!("/api/v1/projects/{}/layouts/generate", d.pid),
            Some(&d.tok),
            &json!({"property_id": property, "room_requirements": rooms, "candidate_count": 1}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let out = r.body["task"]["output_reference"].clone();
    let v = out["design_version_ids"][0].as_str().unwrap().to_owned();
    let bom = t
        .get(
            &format!("/api/v1/projects/{}/layouts/{v}/bom", d.pid),
            Some(&d.tok),
        )
        .await;
    assert_eq!(bom.status, StatusCode::OK, "{}", bom.text);
    (out, bom.body)
}

async fn furnishing(t: &TestApp, d: &Demo, property: &str) -> (Value, Value) {
    let r = t
        .get(
            &format!(
                "/api/v1/projects/{}/nessie/properties/{property}/plan",
                d.pid
            ),
            Some(&d.tok),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let line = r.body["plan"]["payments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["plan_item_id"] == ITEM_FURNISHING)
        .cloned()
        .unwrap_or_else(|| panic!("no furnishing line: {}", r.text));
    (r.body.clone(), line)
}

fn app(db: &TestDb) -> TestApp {
    let t = TestApp::new(db, &[]);
    install(&t.app, NessieState::fixture());
    t
}

both_backends!(a_plan_on_a_sketchs_bill_says_that_it_is_a_sketch, |db| {
    let t = app(db);
    let d = demo(&t).await;
    let (out, bom) = layout_with_bom(&t, &d, &d.alhambra).await;
    assert!(out["floor_estimate"].is_object(), "{out}");
    let (view, line) = furnishing(&t, &d, &d.alhambra).await;
    assert_eq!(view["plan"]["bom_id"], bom["bom_id"]);
    assert_eq!(view["plan"]["design_version_id"], bom["design_version_id"]);
    assert_eq!(line["basis"], BASIS_BOM_SKETCH);
    assert_eq!(line["notes"], json!([OVERLAP_NOTE, NOTE_BOM_SKETCH]));
    // The amount is the bill's total, unchanged.
    for k in [
        "amount_low_minor",
        "amount_expected_minor",
        "amount_high_minor",
    ] {
        assert_eq!(line[k], bom["estimated_total"], "{k}");
    }
    println!("SKETCH LINE {line}");

    // The approval the owner is asked for carries the same amount; its description (the text
    // that would go to the bank) is built from the label, not the basis.
    let pid = &d.pid;
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/nessie/sandbox"),
            Some(&d.tok),
            &json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let a = t
        .call(
            axum::http::Method::POST,
            &format!(
                "/api/v1/projects/{pid}/nessie/properties/{}/payments/{ITEM_FURNISHING}/approval",
                d.alhambra
            ),
            Some(&d.tok),
            None,
        )
        .await;
    assert!(a.status.is_success(), "{}", a.text);
    let payload = &a.body["approval"]["action_payload"];
    assert_eq!(payload["amount_minor"], bom["estimated_total"]);
    let description = payload["description"].as_str().unwrap();
    assert!(
        description.ends_with("Build-out: furnishing and equipment (simulated)"),
        "{description}"
    );
    assert!(description.chars().count() <= 200);
    println!("SKETCH DESCRIPTION {description}");
});

both_backends!(a_plan_on_a_measured_floors_bill_reads_as_before, |db| {
    let t = app(db);
    let d = demo(&t).await;
    let (out, bom) = layout_with_bom(&t, &d, &d.hollywood).await;
    assert!(!out["floor_estimate"].is_object(), "{out}");
    let (_, line) = furnishing(&t, &d, &d.hollywood).await;
    // Recorded from plan.rs before the change: every key of the line, with today's words.
    assert_eq!(
        line,
        json!({
            "plan_item_id": "buildout:furnishing",
            "sequence": line["sequence"],
            "label": "Build-out: furnishing and equipment (simulated)",
            "payee_role": "FURNISHING_SUPPLIER",
            "operation": "PURCHASE",
            "approval_action": "PURCHASE",
            "moves_money": true,
            "amount_low_minor": bom["estimated_total"],
            "amount_expected_minor": bom["estimated_total"],
            "amount_high_minor": bom["estimated_total"],
            "recurring_day": null,
            "depends_on": null,
            "source_line_item_ids": [],
            "basis": MEASURED_BASIS,
            "notes": [OVERLAP_NOTE],
        })
    );
    println!("MEASURED LINE {line}");
});

both_backends!(
    a_bill_whose_version_is_not_in_the_project_is_unknown_not_measured,
    |db| {
        let t = app(db);
        let d = demo(&t).await;
        // Another project's sketch: its design version exists, but not in this project.
        let other = demo(&t).await;
        assert_ne!(other.pid, d.pid);
        let (out, other_bom) = layout_with_bom(&t, &other, &other.alhambra).await;
        assert!(out["floor_estimate"].is_object());
        let foreign_version: Uuid = other_bom["design_version_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();

        let (_, bom) = layout_with_bom(&t, &d, &d.hollywood).await;
        let (_, line) = furnishing(&t, &d, &d.hollywood).await;
        assert_eq!(line["basis"], MEASURED_BASIS);

        // A later bill of this property whose design version belongs to the other project (and, a
        // second time, to nothing at all).
        for version in [foreign_version, Uuid::new_v4()] {
            let mut tx = t.app.store.begin().await.unwrap();
            let mut later: ss_domain::contracts::BillOfMaterials =
                serde_json::from_value(bom.clone()).unwrap();
            later.bom_id = Uuid::new_v4();
            later.design_version_id = version;
            BOMS.add(&mut *tx, &later).await.unwrap();
            tx.commit().await.unwrap();

            let (view, line) = furnishing(&t, &d, &d.hollywood).await;
            assert_eq!(view["plan"]["bom_id"], json!(later.bom_id));
            assert_eq!(line["basis"], BASIS_BOM_FLOOR_UNKNOWN);
            assert_ne!(line["basis"], MEASURED_BASIS);
            // Not the other project's fact either: the lookup is project-checked.
            assert_ne!(line["basis"], BASIS_BOM_SKETCH);
            assert_eq!(line["notes"], json!([OVERLAP_NOTE, NOTE_BOM_FLOOR_UNKNOWN]));
            assert_eq!(line["amount_expected_minor"], bom["estimated_total"]);
        }
    }
);
