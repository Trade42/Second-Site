//! The evidence routes for the Python supervisor (`http/routes/evidence.rs`), on both stores.
//!
//! - After a search and an analysis on the fixture project the evidence lists are non-empty,
//!   schema-valid and in `EvidenceService::for_property` order, and the validation answers
//!   `{"problems": [...]}`. That these equal what Python computes for the same demo data (its
//!   `EvidenceService.for_property` and its supervisor tool `validate_evidence`) is compared in
//!   `wire_parity.rs` (steps "evidence ..." of `property_slice.json`).
//! - Validation against Python on data that has problems: the ten cases of
//!   `tests-golden/server/evidence_validation.json` (Python's `validate_property` over constructed
//!   variants of the Python-written Hollywood property) are stored here as Rust-written rows, and
//!   the route must answer Python's problems exactly.
//! - Same authentication, check order and project scoping as the other property routes.

mod common;

use axum::http::StatusCode;
use serde_json::{json, Value};
use ss_core::Contract;
use ss_domain::contracts::{PropertyDigitalProfile, SourceEvidence};
use ss_server::services::evidence::EvidenceService;
use ss_server::store::repo::{EVIDENCE, PROPERTIES};
use uuid::Uuid;

use common::*;

async fn demo(t: &TestApp) -> (String, String, Vec<Value>) {
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
        .body
        .as_array()
        .unwrap()
        .clone();
    (tok, pid, props)
}

fn evidence_path(pid: &str, prop: &str) -> String {
    format!("/api/v1/projects/{pid}/properties/{prop}/evidence")
}

both_backends!(evidence_after_search_and_analysis, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, pid, props) = demo(&t).await;
    assert_eq!(props.len(), 2);
    // Re-analysis of the first property adds nothing to its evidence, but must not break it.
    let first = props[0]["property_id"].as_str().unwrap();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/{first}/analyze"),
            Some(&tok),
            &json!({}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    for p in &props {
        let prop = p["property_id"].as_str().unwrap();
        let r = t.get(&evidence_path(&pid, prop), Some(&tok)).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        let list = r.body.as_array().unwrap();
        assert!(!list.is_empty(), "no evidence for {prop}");
        for e in list {
            assert_conforms("SourceEvidence", e);
            assert_eq!(e["property_id"], prop);
            // Historical evidence always states its limitations.
            if e["evidence_classification"] == "DOCUMENTED_HISTORICAL" {
                assert!(!e["limitations"].as_array().unwrap().is_empty(), "{e}");
            }
        }
        // The service's order (feasibility reads evidence in this order).
        let mut tx = t.app.store.begin().await.unwrap();
        let service = EvidenceService
            .for_property(&mut *tx, prop.parse().unwrap())
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(r.body, serde_json::to_value(&service).unwrap());

        let r = t
            .get(
                &format!("{}/validation", evidence_path(&pid, prop)),
                Some(&tok),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        let keys: Vec<&String> = r.body.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["problems"]);
        // The fixture listing is labelled "fixture", not CoStar; every reference resolves.
        assert_eq!(r.body, json!({"problems": []}));
    }
});

both_backends!(evidence_routes_are_scoped_and_checked_in_order, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, pid, props) = demo(&t).await;
    let prop = props[0]["property_id"].as_str().unwrap().to_owned();
    // The same owner's other project: 404, as for every property route.
    let other = t
        .create_project(
            &tok,
            &json!({"project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let stranger = t.token_for(Uuid::new_v4());
    for suffix in ["", "/validation"] {
        let own = format!("{}{suffix}", evidence_path(&pid, &prop));
        // A second owner.
        let r = t.get(&own, Some(&stranger)).await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "project not found");
        // Another project of the same owner.
        let r = t
            .get(
                &format!("{}{suffix}", evidence_path(&other, &prop)),
                Some(&tok),
            )
            .await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "property not found");
        // An unknown property.
        let r = t
            .get(
                &format!(
                    "{}{suffix}",
                    evidence_path(&pid, &Uuid::new_v4().to_string())
                ),
                Some(&tok),
            )
            .await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "property not found");
        // No token, a bad token.
        assert_problem(&t.get(&own, None).await, 401, "UNAUTHORIZED");
        assert_problem(&t.get(&own, Some("nope")).await, 401, "UNAUTHORIZED");
        // FastAPI's order: the owned project before the endpoint's own path parameter ...
        let r = t
            .get(
                &format!("{}{suffix}", evidence_path(&pid, "not-a-uuid")),
                Some(&tok),
            )
            .await;
        assert_problem(&r, 422, "VALIDATION_ERROR");
        let r = t
            .get(
                &format!("{}{suffix}", evidence_path(&other, "not-a-uuid")),
                Some(&stranger),
            )
            .await;
        assert_problem(&r, 404, "NOT_FOUND");
        // ... and a malformed project id is a 422.
        let r = t
            .get(
                &format!("{}{suffix}", evidence_path("nope", &prop)),
                Some(&tok),
            )
            .await;
        assert_problem(&r, 422, "VALIDATION_ERROR");
    }
});

both_backends!(validation_matches_python_on_cases_with_problems, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let golden = golden("evidence_validation.json");
    let cases = golden["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 10);
    let mut with_problems = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        // One project per case: a project holds one property per address.
        let pid: Uuid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let mut prop = case["property"].clone();
        prop["project_id"] = json!(pid);
        let prop = PropertyDigitalProfile::from_json_value(prop).unwrap();
        let mut tx = t.app.store.begin().await.unwrap();
        PROPERTIES.add(&mut *tx, &prop).await.unwrap();
        let mut stored = Vec::new();
        for e in case["evidence"].as_array().unwrap() {
            let mut e = e.clone();
            e["project_id"] = json!(pid);
            let e = SourceEvidence::from_json_value(e).unwrap();
            EVIDENCE.add(&mut *tx, &e).await.unwrap();
            stored.push(serde_json::to_value(&e).unwrap());
        }
        tx.commit().await.unwrap();
        let path = evidence_path(&pid.to_string(), &prop.property_id.to_string());
        let r = t.get(&format!("{path}/validation"), Some(&tok)).await;
        assert_eq!(r.status, StatusCode::OK, "{name}: {}", r.text);
        assert_eq!(
            r.body,
            json!({"problems": case["problems"]}),
            "{name}: Rust and Python disagree"
        );
        with_problems += usize::from(!case["problems"].as_array().unwrap().is_empty());
        // The list route answers the rows as stored (Python's dumps, with this project's id).
        let r = t.get(&path, Some(&tok)).await;
        let mut got = r.body.as_array().unwrap().clone();
        let key = |v: &Value| v["evidence_id"].as_str().unwrap().to_owned();
        got.sort_by_key(key);
        stored.sort_by_key(key);
        assert_eq!(got, stored, "{name}");
    }
    assert_eq!(with_problems, 6);
});
