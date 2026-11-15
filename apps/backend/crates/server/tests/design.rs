//! Layout routes and the design service (`api/routes/design.py`, `domains/design/service.py`),
//! on both stores. Ports the design-studio half of `test_api_smoke.py`'s
//! `test_end_to_end_property_slice`, then covers what the brief lists: measured geometry from the
//! fixture Matterport provider, a property without geometry, an infeasible program, applied,
//! infeasible, stale and unstructured revisions, the bill of materials generated once, project
//! and owner isolation for every route, request-check order, and (PostgreSQL only) unique version
//! numbers and a single bill under concurrency.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use ss_domain::contracts::{DesignApprovalStatus, DesignRevisionRequest, DesignVersion};
use ss_server::services::design::{
    bom_lock_key, lock_key, room_requirements_from_profile, version_lock_key, DesignService,
};
use ss_server::store::repo::{rooms, BOMS, DESIGNS, GEOMETRIES, PROPERTIES};
use ss_server::store::{schema, Query};
use uuid::Uuid;

use common::*;

const HOLLYWOOD: &str = "6922 W HOLLYWOOD BLVD";
const ALHAMBRA: &str = "5380 E ALHAMBRA AVE";
const NOT_APPROVED: &str = "Not a code-compliance review or professionally approved plan.";

/// The room program of `test_api_smoke.py`.
fn reqs() -> Value {
    json!([
        {"room_type": "treatment", "count": 3, "min_area_sqm": 10, "min_width_m": 2.8, "needs_plumbing": true},
        {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
        {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5},
    ])
}

fn reqs_with_treatments(n: i64) -> Value {
    let mut r = reqs();
    r[0]["count"] = json!(n);
    r
}

fn twelve_rooms() -> Value {
    json!([{"room_type": "treatment", "count": 12, "min_area_sqm": 12, "min_width_m": 2.8, "needs_plumbing": true}])
}

struct Demo {
    tok: String,
    pid: String,
    hollywood: String,
    alhambra: String,
}

/// Dev token, the demo project with its profile, and the fixture search (two properties).
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

fn base(pid: &str) -> String {
    format!("/api/v1/projects/{pid}/layouts")
}

async fn generate(t: &TestApp, tok: &str, pid: &str, body: &Value) -> Resp {
    t.post(&format!("{}/generate", base(pid)), Some(tok), body)
        .await
}

async fn layouts(t: &TestApp, tok: &str, pid: &str) -> Vec<Value> {
    let r = t.get(&base(pid), Some(tok)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    r.body.as_array().unwrap().clone()
}

async fn revise(t: &TestApp, tok: &str, pid: &str, layout: &str, body: &Value) -> Resp {
    t.post(
        &format!("{}/{layout}/revisions", base(pid)),
        Some(tok),
        body,
    )
    .await
}

async fn events(t: &TestApp, tok: &str, pid: &str) -> Vec<Value> {
    let r = t
        .get(
            &format!("/api/v1/projects/{pid}/events?limit=500"),
            Some(tok),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    r.body["events"].as_array().unwrap().clone()
}

fn of_type<'a>(events: &'a [Value], t: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["event_type"] == t).collect()
}

fn uuid(v: &Value) -> Uuid {
    v.as_str().unwrap().parse().unwrap()
}

async fn count(t: &TestApp, table: &'static schema::TableDef) -> usize {
    let mut tx = t.app.store.begin().await.unwrap();
    let n = tx.select(table, &Query::new()).await.unwrap().len();
    tx.rollback().await.unwrap();
    n
}

async fn design(t: &TestApp, id: Uuid) -> (DesignVersion, i64) {
    let mut tx = t.app.store.begin().await.unwrap();
    let got = DESIGNS.get_versioned(&mut *tx, id).await.unwrap().unwrap();
    tx.rollback().await.unwrap();
    got
}

// ---- test_api_smoke.py: the design studio ---------------------------------------------------------

both_backends!(design_studio_end_to_end, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());

    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": d.hollywood, "room_requirements": reqs()}),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_conforms("TaskAccepted", &r.body);
    let task = &r.body["task"];
    assert_eq!(task["status"], "COMPLETED", "{task}");
    assert_eq!(task["task_type"], "generate_layout");
    assert_eq!(task["assigned_subsystem"], "property-spatial");
    // `body.model_dump(mode="json")`: defaults filled in, numbers as floats.
    assert_eq!(
        task["input_reference"],
        json!({
            "property_id": d.hollywood,
            "site_geometry_id": null,
            "room_requirements": [
                {"room_type": "treatment", "count": 3, "min_area_sqm": 10.0, "min_width_m": 2.8, "needs_plumbing": true},
                {"room_type": "reception", "count": 1, "min_area_sqm": 12.0, "min_width_m": 3.0, "needs_plumbing": false},
                {"room_type": "storage", "count": 1, "min_area_sqm": 4.0, "min_width_m": 1.5, "needs_plumbing": false},
            ],
            "preferences": {"style": null, "prefer_rooms_near_plumbing": true, "circulation_width_m": 1.2},
            "candidate_count": 2,
        })
    );
    let status_url = r.body["status_url"].as_str().unwrap();
    assert_eq!(
        status_url,
        format!(
            "/api/v1/projects/{pid}/tasks/{}",
            task["task_id"].as_str().unwrap()
        )
    );
    assert_eq!(
        t.get(status_url, Some(tok)).await.body["status"],
        "COMPLETED"
    );

    let lv = layouts(&t, tok, pid).await;
    assert_eq!(lv.len(), 2);
    let created: Vec<&Value> = task["output_reference"]["design_version_ids"]
        .as_array()
        .unwrap()
        .iter()
        .collect();
    for (i, l) in lv.iter().enumerate() {
        assert_conforms("DesignVersion", l);
        assert_eq!(l["validation_report"]["valid"], true);
        // Generated layouts are proposals, never approved plans.
        assert_eq!(l["approval_status"], "PROPOSED");
        assert!(l["validation_report"]["disclaimer"]
            .as_str()
            .unwrap()
            .contains(NOT_APPROVED));
        assert_eq!(l["version_number"], json!(i + 1));
        assert_eq!(l["parent_version_id"], Value::Null);
        assert_eq!(l["project_id"], pid);
        assert_eq!(l["property_id"], d.hollywood.as_str());
        assert_eq!(&l["design_version_id"], created[i]);
        assert_eq!(l["room_polygons"].as_array().unwrap().len(), 5);
        assert_eq!(l["geometry_id"], lv[0]["geometry_id"]);
    }
    let v1 = lv[0]["design_version_id"].as_str().unwrap().to_owned();

    let bom = t.get(&format!("{}/{v1}/bom", base(pid)), Some(tok)).await;
    assert_eq!(bom.status, StatusCode::OK, "{}", bom.text);
    assert_conforms("BillOfMaterials", &bom.body);
    assert_eq!(bom.body["design_version_id"], v1.as_str());
    let items = bom.body["items"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i["item_type"] == "treatment_bed")
            .count(),
        3
    );
    for i in items {
        assert!(i["room_id"].as_str().is_some_and(|s| !s.is_empty()), "{i}");
        assert_eq!(i["price_origin"], "FIXTURE_SYNTHETIC");
        assert!(i["price_note"]
            .as_str()
            .unwrap()
            .contains("not a supplier quote"));
    }

    // A fourth treatment room: applied as version 3, a child of version 1.
    let r = revise(
        &t,
        tok,
        pid,
        &v1,
        &json!({"requested_changes": ["add a fourth treatment room"], "room_requirement_overrides": reqs_with_treatments(4)}),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_conforms("TaskAccepted", &r.body);
    let task = &r.body["task"];
    assert_eq!(task["task_type"], "revise_layout");
    assert_eq!(task["assigned_subsystem"], "property-spatial");
    let rev_id = uuid(&task["input_reference"]["revision_request_id"]);
    assert_eq!(
        task["input_reference"].as_object().unwrap().len(),
        1,
        "{task}"
    );
    let out = &task["output_reference"];
    assert_eq!(out["revision_status"], "APPLIED", "{task}");
    assert_eq!(out["design_version_ids"].as_array().unwrap().len(), 1);
    assert_eq!(
        out["current_valid_version_id"],
        out["design_version_ids"][0]
    );
    let v3 = layouts(&t, tok, pid)
        .await
        .into_iter()
        .find(|l| l["design_version_id"] == out["design_version_ids"][0])
        .unwrap();
    assert_eq!(v3["parent_version_id"], v1.as_str());
    assert_eq!(v3["version_number"], 3);
    assert_eq!(v3["approval_status"], "PROPOSED");
    assert_eq!(v3["created_from_revision_request"], rev_id.to_string());
    assert_eq!(v3["room_polygons"].as_array().unwrap().len(), 6);

    // Twelve rooms do not fit: INFEASIBLE, version 1 stays current and untouched.
    let (before, before_version) = design(&t, uuid(&json!(v1))).await;
    let r = revise(
        &t,
        tok,
        pid,
        &v1,
        &json!({"requested_changes": ["twelve rooms"], "room_requirement_overrides": twelve_rooms()}),
    )
    .await;
    let out = &r.body["task"]["output_reference"];
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    assert_eq!(out["revision_status"], "INFEASIBLE");
    assert_eq!(out["design_version_ids"], json!([]));
    assert_eq!(out["current_valid_version_id"], v1.as_str());
    assert_eq!(layouts(&t, tok, pid).await.len(), 3);
    assert_eq!(design(&t, uuid(&json!(v1))).await, (before, before_version));
    assert_eq!(before_version, 1);
});

// ---- geometry -------------------------------------------------------------------------------------

both_backends!(measured_geometry_is_stored_once_and_labelled, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let body = json!({"property_id": d.hollywood, "room_requirements": reqs()});
    assert_eq!(
        generate(&t, tok, pid, &body).await.body["task"]["status"],
        "COMPLETED"
    );
    let lv = layouts(&t, tok, pid).await;
    let gid = lv[0]["geometry_id"].as_str().unwrap().to_owned();

    let g = t
        .get(&format!("{}/geometry/{gid}", base(pid)), Some(tok))
        .await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.text);
    assert_conforms("SiteGeometry", &g.body);
    // `fixtures/geometry/hollywood_302_synthetic.json`, labelled as the fixture it is.
    assert_eq!(g.body["site_geometry_id"], gid.as_str());
    assert_eq!(g.body["property_id"], d.hollywood.as_str());
    assert_eq!(g.body["source_id"], "fixture-matterport-hollywood-302");
    assert_eq!(g.body["data_origin"], "FIXTURE_SYNTHETIC");
    assert_eq!(g.body["geometry_source"], "FIXTURE");
    assert_eq!(g.body["scale_verified"], false);
    assert_eq!(g.body["units"], "m");
    assert_eq!(g.body["coordinate_reference"], "local_floor_origin");
    assert_eq!(
        g.body["floor_boundary"],
        json!([[0.0, 0.0], [12.0, 0.0], [12.0, 11.6], [0.0, 11.6]])
    );
    assert_eq!(g.body["measurement_uncertainty_m"], 0.15);
    assert_eq!(
        g.body["fixed_obstacles"],
        json!([[[0.2, 0.2], [0.8, 0.2], [0.8, 0.8], [0.2, 0.8]]])
    );

    // The property records the geometry it was measured with.
    let p = t
        .get(
            &format!("/api/v1/projects/{pid}/properties/{}", d.hollywood),
            Some(tok),
        )
        .await;
    assert_eq!(p.body["geometry_reference"], gid.as_str());

    // A second generation reuses the stored geometry.
    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": d.hollywood, "room_requirements": reqs(), "candidate_count": 1}),
    )
    .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let lv = layouts(&t, tok, pid).await;
    assert_eq!(lv.len(), 3);
    assert_eq!(lv[2]["geometry_id"], gid.as_str());
    assert_eq!(lv[2]["version_number"], 3);
    assert_eq!(count(&t, &schema::SITE_GEOMETRIES).await, 1);

    // One `rooms` row per room polygon, carrying the polygon's fields.
    let mut tx = t.app.store.begin().await.unwrap();
    let mut total = 0;
    for l in &lv {
        let stored = rooms::for_design_version(&mut *tx, uuid(&l["design_version_id"]))
            .await
            .unwrap();
        let polys = l["room_polygons"].as_array().unwrap();
        assert_eq!(stored.len(), polys.len());
        let keys: BTreeSet<&str> = stored.iter().map(|r| r.room_key.as_str()).collect();
        for poly in polys {
            let row = stored
                .iter()
                .find(|r| r.room_key == poly["room_id"].as_str().unwrap())
                .unwrap();
            assert_eq!(row.room_type, poly["room_type"].as_str().unwrap());
            assert_eq!(row.area_sqm, poly["area_sqm"].as_f64().unwrap());
            assert_eq!(row.polygon, poly["polygon"]);
        }
        assert_eq!(keys.len(), polys.len());
        total += stored.len();
    }
    tx.rollback().await.unwrap();
    assert_eq!(count(&t, &schema::ROOMS).await, total);
});

both_backends!(a_property_without_geometry_fails_the_task, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": d.alhambra, "room_requirements": reqs()}),
    )
    .await;
    // Python records the service's 404 in the task: 202, FAILED, `NotFoundError`.
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let task = &r.body["task"];
    assert_eq!(task["status"], "FAILED");
    assert_eq!(task["error_code"], "NotFoundError");
    assert_eq!(
        task["error_message"],
        "no measured geometry available for this property"
    );
    assert!(layouts(&t, tok, pid).await.is_empty());
    assert_eq!(count(&t, &schema::SITE_GEOMETRIES).await, 0);
    let ev = events(&t, tok, pid).await;
    assert_eq!(of_type(&ev, "task.failed").len(), 1);
    assert!(of_type(&ev, "layout.version_created").is_empty());

    // The service itself: 404 GEOMETRY_UNAVAILABLE (nothing stored, no asset has a document).
    let svc = DesignService::new(&t.app);
    let mut tx = t.app.store.begin().await.unwrap();
    let source = svc
        .site_source(&mut *tx, uuid(&json!(pid)), uuid(&json!(d.alhambra)))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let err = svc
        .prepare_site(uuid(&json!(d.alhambra)), source)
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
    assert_eq!(err.error_code(), "GEOMETRY_UNAVAILABLE");
    assert_eq!(
        err.to_string(),
        "no measured geometry available for this property"
    );
});

// ---- generation events ------------------------------------------------------------------------------

both_backends!(an_infeasible_program_stores_no_design_version, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": d.hollywood, "room_requirements": twelve_rooms()}),
    )
    .await;
    // Deliberately different from Python (which completes with `design_version_ids: []` and
    // says nothing): the task fails and says why, with the measured sizes.
    let task = &r.body["task"];
    assert_eq!(task["status"], "FAILED", "{}", r.text);
    assert_eq!(task["error_code"], "LayoutInfeasibleError");
    assert_eq!(task["output_reference"], json!({}));
    let msg = task["error_message"].as_str().unwrap();
    assert!(
        msg.starts_with("No layout fits the measured floor: the floor is ")
            && msg.contains("largest rectangular area is ")
            && msg.contains("the room program needs 12 room(s), at least 144.0 m2 in all")
            && msg.contains("with minimum room widths up to 2.8 m.")
            && msg.contains("floor is 139 m2 and its largest rectangular area is 12.0 x 11.6 m")
            && msg.contains("No spine layout satisfies the room program")
            && msg.ends_with("No layout was stored."),
        "{msg}"
    );
    let polled = t
        .get(r.body["status_url"].as_str().unwrap(), Some(tok))
        .await;
    assert_eq!(polled.body["status"], "FAILED", "{}", polled.text);
    assert!(layouts(&t, tok, pid).await.is_empty());
    assert_eq!(count(&t, &schema::DESIGN_VERSIONS).await, 0);
    assert_eq!(count(&t, &schema::ROOMS).await, 0);
    let ev = events(&t, tok, pid).await;
    let failed = of_type(&ev, "layout.validation_failed");
    assert_eq!(failed.len(), 2, "one per candidate (spine, front)");
    let corr = task["correlation_id"].as_str().unwrap();
    for (e, name) in failed.iter().zip(["spine", "front"]) {
        assert_eq!(
            e["idempotency_key"],
            format!("layout-failed:{}:{corr}:{name}", d.hollywood)
        );
        assert_eq!(e["correlation_id"], corr);
        assert_eq!(e["actor_type"], "agent");
        assert_eq!(e["actor_id"], "floor-plan-architect");
        assert_eq!(e["payload"]["design_version_id"], Value::Null);
        assert_eq!(e["payload"]["property_id"], d.hollywood.as_str());
        assert_eq!(e["payload"]["valid"], false);
        let codes = e["payload"]["issue_codes"].as_array().unwrap();
        assert!(!codes.is_empty(), "{e}");
        assert_conforms("LayoutPayload", &e["payload"]);
    }
    assert!(of_type(&ev, "layout.version_created").is_empty());
    // The task's last event says it failed; the floor it measured is still stored.
    let task_events: Vec<&Value> = ev
        .iter()
        .filter(|e| e["payload"]["task_id"] == task["task_id"])
        .collect();
    assert_eq!(task_events.last().unwrap()["event_type"], "task.failed");
    assert!(of_type(&ev, "task.completed")
        .iter()
        .all(|e| e["payload"]["task_id"] != task["task_id"]));
    assert_eq!(count(&t, &schema::SITE_GEOMETRIES).await, 1);
});

both_backends!(each_stored_version_emits_one_created_event, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": d.hollywood, "room_requirements": reqs()}),
    )
    .await;
    let task = &r.body["task"];
    let corr = task["correlation_id"].as_str().unwrap();
    let ids = task["output_reference"]["design_version_ids"]
        .as_array()
        .unwrap()
        .clone();
    let ev = events(&t, tok, pid).await;
    let created = of_type(&ev, "layout.version_created");
    assert_eq!(created.len(), ids.len());
    for (e, id) in created.iter().zip(&ids) {
        assert_eq!(
            e["idempotency_key"],
            format!("layout:{}", id.as_str().unwrap())
        );
        assert_eq!(e["correlation_id"], corr);
        assert_eq!(e["actor_type"], "agent");
        assert_eq!(e["actor_id"], "floor-plan-architect");
        assert_eq!(
            e["payload"],
            json!({"design_version_id": id, "property_id": d.hollywood, "valid": true, "issue_codes": []})
        );
    }
    // The task's own events share the correlation id.
    for t_ev in ["task.started", "task.completed"] {
        assert!(of_type(&ev, t_ev)
            .iter()
            .any(|e| e["correlation_id"] == corr));
    }
});

// ---- revisions ------------------------------------------------------------------------------------

async fn first_layout(t: &TestApp, d: &Demo) -> String {
    let r = generate(
        t,
        &d.tok,
        &d.pid,
        &json!({"property_id": d.hollywood, "room_requirements": reqs(), "candidate_count": 1}),
    )
    .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    r.body["task"]["output_reference"]["design_version_ids"][0]
        .as_str()
        .unwrap()
        .to_owned()
}

both_backends!(a_stale_base_is_refused, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let v1 = first_layout(&t, &d).await;
    let v1_id = uuid(&json!(v1));
    // Supersede the base (no route does this yet; the owner approval flow will).
    let mut tx = t.app.store.begin().await.unwrap();
    let mut base = DESIGNS.require(&mut *tx, v1_id).await.unwrap();
    base.approval_status = DesignApprovalStatus::Superseded;
    DESIGNS.upsert(&mut *tx, &base).await.unwrap();
    tx.commit().await.unwrap();
    let stale = design(&t, v1_id).await;

    let r = revise(
        &t,
        tok,
        pid,
        &v1,
        &json!({"requested_changes": ["one more room"], "room_requirement_overrides": reqs_with_treatments(4)}),
    )
    .await;
    // Python: the service's 409 is recorded in the task (202, FAILED, ConflictError).
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let task = &r.body["task"];
    assert_eq!(task["status"], "FAILED");
    assert_eq!(task["error_code"], "ConflictError");
    assert_eq!(
        task["error_message"],
        "base design version is stale (superseded)"
    );
    assert_eq!(layouts(&t, tok, pid).await.len(), 1);
    assert_eq!(design(&t, v1_id).await, stale);

    // The service: 409 STALE_DESIGN_VERSION.
    let rev = revision_request(&t, &d, v1_id, true).await;
    let mut tx = t.app.store.begin().await.unwrap();
    let err = DesignService::new(&t.app)
        .revision_plan(&mut *tx, &rev)
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(err.status(), StatusCode::CONFLICT);
    assert_eq!(err.error_code(), "STALE_DESIGN_VERSION");
});

async fn revision_request(
    t: &TestApp,
    d: &Demo,
    base: Uuid,
    overrides: bool,
) -> DesignRevisionRequest {
    let mut tx = t.app.store.begin().await.unwrap();
    let b = DESIGNS.require(&mut *tx, base).await.unwrap();
    tx.rollback().await.unwrap();
    let room_requirement_overrides = if overrides {
        serde_json::from_value(reqs_with_treatments(4)).unwrap()
    } else {
        vec![]
    };
    DesignRevisionRequest {
        revision_request_id: Uuid::new_v4(),
        project_id: uuid(&json!(d.pid)),
        property_id: b.property_id,
        base_design_version_id: base,
        requested_changes: vec!["change".into()],
        room_requirement_overrides,
        preserved_constraints: vec![],
        requested_by: Uuid::new_v4(),
        source_channel: "web".into(),
        status: Default::default(),
    }
}

both_backends!(an_unstructured_revision_is_refused, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let v1 = first_layout(&t, &d).await;
    let r = revise(
        &t,
        tok,
        pid,
        &v1,
        &json!({"requested_changes": ["make it brighter"], "source_channel": "photon"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let task = &r.body["task"];
    assert_eq!(task["status"], "FAILED");
    assert_eq!(task["error_code"], "ConflictError");
    assert_eq!(
        task["error_message"],
        "revision needs room_requirement_overrides in the foundation build"
    );
    assert_eq!(layouts(&t, tok, pid).await.len(), 1);

    let rev = revision_request(&t, &d, uuid(&json!(v1)), false).await;
    let mut tx = t.app.store.begin().await.unwrap();
    let err = DesignService::new(&t.app)
        .revision_plan(&mut *tx, &rev)
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(err.status(), StatusCode::CONFLICT);
    assert_eq!(err.error_code(), "REVISION_NOT_STRUCTURED");
});

// ---- bill of materials ------------------------------------------------------------------------------

both_backends!(the_bill_is_generated_once_then_read_back, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": d.hollywood, "room_requirements": reqs()}),
    )
    .await;
    let ids = r.body["task"]["output_reference"]["design_version_ids"].clone();
    let (v1, v2) = (ids[0].as_str().unwrap(), ids[1].as_str().unwrap());
    let first = t.get(&format!("{}/{v1}/bom", base(pid)), Some(tok)).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    let second = t.get(&format!("{}/{v1}/bom", base(pid)), Some(tok)).await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(first.body, second.body, "the stored bill is returned");
    assert_eq!(first.body["project_id"], pid);
    assert_eq!(first.body["property_id"], d.hollywood.as_str());
    assert_eq!(first.body["status"], "DRAFT");
    assert_eq!(first.body["currency"], "USD");
    assert_eq!(first.body["approved_total"], Value::Null);
    let ev = events(&t, tok, pid).await;
    let generated = of_type(&ev, "bom.generated");
    assert_eq!(generated.len(), 1);
    let e = generated[0];
    assert_eq!(
        e["idempotency_key"],
        format!("bom:{}", first.body["bom_id"].as_str().unwrap())
    );
    assert_eq!(e["actor_type"], "agent");
    assert_eq!(e["actor_id"], "procurement");
    assert_eq!(
        e["payload"],
        json!({"bom_id": first.body["bom_id"], "design_version_id": v1, "estimated_total": first.body["estimated_total"]})
    );
    assert_conforms("BomPayload", &e["payload"]);
    assert_eq!(count(&t, &schema::BILLS_OF_MATERIALS).await, 1);

    // Each version has its own bill.
    let other = t.get(&format!("{}/{v2}/bom", base(pid)), Some(tok)).await;
    assert_eq!(other.body["design_version_id"], v2);
    assert_ne!(other.body["bom_id"], first.body["bom_id"]);
    assert_eq!(count(&t, &schema::BILLS_OF_MATERIALS).await, 2);
    assert_eq!(
        of_type(&events(&t, tok, pid).await, "bom.generated").len(),
        2
    );

    // The stored document is exactly what was returned.
    let mut tx = t.app.store.begin().await.unwrap();
    let stored = BOMS
        .list(
            &mut *tx,
            Query::new().eq("design_version_id", uuid(&json!(v1))),
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(serde_json::to_value(&stored[0]).unwrap(), first.body);
});

// ---- isolation ------------------------------------------------------------------------------------

both_backends!(every_route_is_scoped_to_the_project_and_owner, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": d.hollywood, "room_requirements": reqs(), "candidate_count": 1}),
    )
    .await;
    let v1 = r.body["task"]["output_reference"]["design_version_ids"][0]
        .as_str()
        .unwrap()
        .to_owned();
    let gid = layouts(&t, tok, pid).await[0]["geometry_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let gen_body = json!({"property_id": d.hollywood, "room_requirements": reqs()});
    let rev_body = json!({"requested_changes": ["x"], "room_requirement_overrides": reqs()});

    // Another owner: every route answers 404 for the first owner's project.
    let stranger = t.token_for(Uuid::new_v4());
    let s = Some(stranger.as_str());
    assert_problem(
        &t.post(&format!("{}/generate", base(pid)), s, &gen_body)
            .await,
        404,
        "NOT_FOUND",
    );
    assert_problem(
        &t.get(&format!("{}/geometry/{gid}", base(pid)), s).await,
        404,
        "NOT_FOUND",
    );
    assert_problem(&t.get(&base(pid), s).await, 404, "NOT_FOUND");
    assert_problem(
        &t.post(&format!("{}/{v1}/revisions", base(pid)), s, &rev_body)
            .await,
        404,
        "NOT_FOUND",
    );
    assert_problem(
        &t.get(&format!("{}/{v1}/bom", base(pid)), s).await,
        404,
        "NOT_FOUND",
    );
    for r in [
        t.get(&base(pid), s).await,
        t.get(&format!("{}/{v1}/bom", base(pid)), s).await,
    ] {
        assert_eq!(r.body["message"], "project not found");
    }

    // The same owner's second project: records of the first project are not found under it.
    let other = t
        .create_project(
            tok,
            &json!({"project_name": "other", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let o = other.as_str();
    let r = t.get(&format!("{}/{v1}/bom", base(o)), Some(tok)).await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "layout not found");
    let r = t
        .post(&format!("{}/{v1}/revisions", base(o)), Some(tok), &rev_body)
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "layout not found");
    let r = t
        .get(&format!("{}/geometry/{gid}", base(o)), Some(tok))
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "geometry not found");
    assert!(layouts(&t, tok, o).await.is_empty());
    // The Python defect: generation with another project's property. Refused here, and nothing
    // is created in either project.
    let tasks_before = count(&t, &schema::TASKS).await;
    let r = generate(&t, tok, o, &gen_body).await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "property not found");
    assert_eq!(count(&t, &schema::TASKS).await, tasks_before);
    assert_eq!(count(&t, &schema::DESIGN_VERSIONS).await, 1);

    // Unknown ids.
    let unknown = Uuid::new_v4();
    let r = t
        .get(&format!("{}/{unknown}/bom", base(pid)), Some(tok))
        .await;
    assert_eq!(r.body["message"], "layout not found");
    let r = t
        .get(&format!("{}/geometry/{unknown}", base(pid)), Some(tok))
        .await;
    assert_eq!(r.body["message"], "geometry not found");
    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": unknown, "room_requirements": reqs()}),
    )
    .await;
    assert_eq!(r.body["message"], "property not found");
    let r = revise(&t, tok, pid, &unknown.to_string(), &rev_body).await;
    assert_eq!(r.body["message"], "layout not found");

    // No token.
    assert_problem(&t.get(&base(pid), None).await, 401, "UNAUTHORIZED");
    assert_problem(
        &t.post(&format!("{}/generate", base(pid)), None, &gen_body)
            .await,
        401,
        "UNAUTHORIZED",
    );
});

both_backends!(the_service_scopes_the_property_to_the_project, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let other = Uuid::new_v4();
    let mut tx = t.app.store.begin().await.unwrap();
    let err = DesignService::new(&t.app)
        .site_source(&mut *tx, other, uuid(&json!(d.hollywood)))
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
    assert_eq!(err.to_string(), "property not found");
    // Nothing was stored for the property.
    assert_eq!(count(&t, &schema::SITE_GEOMETRIES).await, 0);
    let mut tx = t.app.store.begin().await.unwrap();
    let prop = PROPERTIES
        .require(&mut *tx, uuid(&json!(d.hollywood)))
        .await
        .unwrap();
    let geos = GEOMETRIES
        .list(&mut *tx, Query::new().eq("property_id", prop.property_id))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(prop.geometry_reference.is_none());
    assert!(geos.is_empty());
});

// ---- request checks -----------------------------------------------------------------------------

async fn raw_post(t: &TestApp, path: &str, token: Option<&str>, body: &'static str) -> Resp {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(tok) = token {
        b = b.header("authorization", format!("Bearer {tok}"));
    }
    t.send(b.body(Body::from(body)).unwrap()).await
}

both_backends!(request_checks_follow_fastapi_order, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let v1 = first_layout(&t, &d).await;
    let gen = format!("{}/generate", base(pid));
    let rev = format!("{}/{v1}/revisions", base(pid));

    // Invalid JSON is refused before authentication, as FastAPI decodes the body first.
    for path in [&gen, &rev] {
        let r = raw_post(&t, path, None, "{\"property_id\": ").await;
        assert_problem(&r, 422, "VALIDATION_ERROR");
        assert_eq!(r.body["details"]["errors"][0]["type"], "json_invalid");
    }
    // Then the token, then the project, then the body model.
    let r = raw_post(&t, &gen, None, "{}").await;
    assert_problem(&r, 401, "UNAUTHORIZED");
    let stranger = t.token_for(Uuid::new_v4());
    let r = raw_post(&t, &gen, Some(&stranger), "{}").await;
    assert_problem(&r, 404, "NOT_FOUND");
    let r = raw_post(&t, &gen, Some(tok), "{}").await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(r.body["details"]["errors"][0]["type"], "missing");

    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": d.hollywood, "room_requirements": reqs(), "candidate_count": 6}),
    )
    .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(
        r.body["details"]["errors"],
        json!([{"type": "less_than_equal", "loc": ["body", "candidate_count"], "msg": "Input should be less than or equal to 5", "input": 6, "ctx": {"le": 5}}])
    );
    let r = generate(
        &t,
        tok,
        pid,
        &json!({"property_id": d.hollywood, "room_requirements": reqs(), "colour": "red"}),
    )
    .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(r.body["details"]["errors"][0]["type"], "extra_forbidden");
    let r = generate(&t, tok, pid, &json!({"property_id": d.hollywood, "room_requirements": [{"room_type": "treatment", "count": 0, "min_area_sqm": 10, "min_width_m": 2.8}]})).await;
    assert_problem(&r, 422, "VALIDATION_ERROR");

    // The revision body.
    let r = t
        .post(&rev, Some(tok), &json!({"requested_changes": []}))
        .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(
        r.body["details"]["errors"],
        json!([{"type": "too_short", "loc": ["body", "requested_changes"], "msg": "List should have at least 1 item after validation, not 0", "input": [], "ctx": {"field_type": "List", "min_length": 1, "actual_length": 0}}])
    );
    let r = t
        .post(
            &rev,
            Some(tok),
            &json!({"requested_changes": ["x"], "why": 1}),
        )
        .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    // A malformed layout id is a 422 after the project check; a stranger still gets 404.
    let bad = format!("{}/not-a-uuid/revisions", base(pid));
    let r = t
        .post(&bad, Some(tok), &json!({"requested_changes": ["x"]}))
        .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(
        r.body["details"]["errors"][0]["loc"],
        json!(["path", "layout_id"])
    );
    let r = t
        .post(&bad, Some(&stranger), &json!({"requested_changes": ["x"]}))
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    for path in [
        format!("{}/not-a-uuid/bom", base(pid)),
        format!("{}/geometry/not-a-uuid", base(pid)),
    ] {
        assert_problem(&t.get(&path, Some(tok)).await, 422, "VALIDATION_ERROR");
    }
    // Nothing above created a design version.
    assert_eq!(layouts(&t, tok, pid).await.len(), 1);
});

// ---- pure parts against Python ------------------------------------------------------------------

#[test]
fn room_requirements_and_lock_keys_match_python() {
    let g = golden("design_service.json");
    for case in g["room_requirements_from_profile"].as_array().unwrap() {
        // `[name, count]` pairs in the profile's order.
        let pairs: Vec<(String, i64)> = serde_json::from_value(case["rooms"].clone()).unwrap();
        let rooms: indexmap::IndexMap<String, i64> = pairs.into_iter().collect();
        let plumbing: Option<Vec<String>> =
            serde_json::from_value(case["plumbing_rooms"].clone()).unwrap();
        let plumbing_refs: Option<Vec<&str>> = plumbing
            .as_ref()
            .map(|p| p.iter().map(String::as_str).collect());
        let got = room_requirements_from_profile(&rooms, plumbing_refs.as_deref()).unwrap();
        assert_eq!(
            serde_json::to_value(&got).unwrap(),
            case["result"],
            "{case}"
        );
    }
    for k in g["lock_keys"].as_array().unwrap() {
        let id: Uuid = k["id"].as_str().unwrap().parse().unwrap();
        let want = k["key"].as_i64().unwrap();
        assert_eq!(lock_key(k["namespace"].as_str().unwrap(), id), want);
        match k["namespace"].as_str().unwrap() {
            "design_versions" => assert_eq!(version_lock_key(id), want),
            "bills_of_materials" => assert_eq!(bom_lock_key(id), want),
            "activity_events" => {
                assert_eq!(ss_server::services::events::event_lock_key(Some(id)), want)
            }
            other => panic!("{other}"),
        }
    }
}

// ---- concurrency (PostgreSQL) -------------------------------------------------------------------
//
// The in-memory store serializes whole transactions, so these run on PostgreSQL only.

/// Advisory locks this test's sessions are waiting for (`application_name` = the test schema).
async fn waiting_advisory_locks(schema: &str) -> i64 {
    use sqlx::ConnectOptions;
    let url = pg_url().unwrap();
    let opts = <sqlx::postgres::PgConnectOptions as std::str::FromStr>::from_str(
        &ss_server::config::normalize_database_url(&url),
    )
    .unwrap();
    let mut c = opts.connect().await.unwrap();
    let row: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid \
         WHERE l.locktype = 'advisory' AND NOT l.granted AND a.application_name = $1",
    )
    .bind(schema)
    .fetch_one(&mut c)
    .await
    .unwrap();
    row.0
}

fn spawn_generate(
    t: &TestApp,
    tok: &str,
    pid: &str,
    body: Value,
) -> tokio::task::JoinHandle<Value> {
    let router = t.router.clone();
    let path = format!("{}/generate", base(pid));
    let auth = format!("Bearer {tok}");
    tokio::spawn(async move {
        let req = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("authorization", auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    })
}

fn version_numbers(lv: &[Value]) -> Vec<i64> {
    let mut v: Vec<i64> = lv
        .iter()
        .map(|l| l["version_number"].as_i64().unwrap())
        .collect();
    v.sort_unstable();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn concurrent_generations_number_versions_uniquely() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let t = TestApp::new(&db, &[]);
    let d = demo(&t).await;
    let body = json!({"property_id": d.hollywood, "room_requirements": reqs()});
    let handles: Vec<_> = (0..4)
        .map(|_| spawn_generate(&t, &d.tok, &d.pid, body.clone()))
        .collect();
    for h in handles {
        let r = h.await.unwrap();
        assert_eq!(r["task"]["status"], "COMPLETED", "{r}");
        assert_eq!(
            r["task"]["output_reference"]["design_version_ids"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    let lv = layouts(&t, &d.tok, &d.pid).await;
    assert_eq!(version_numbers(&lv), (1..=8).collect::<Vec<_>>());
    db.finish().await;
}

/// Two writers that bypass the routes (no event, so no event lock): the second waits on the
/// per-property version lock, not on the unique index, and numbers after the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_version_lock_serializes_numbering() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let t = TestApp::new(&db, &[]);
    let d = demo(&t).await;
    let v1 = first_layout(&t, &d).await;
    let (template, _) = design(&t, uuid(&json!(v1))).await;

    let mut first = t.app.store.begin().await.unwrap();
    let mut a = template.clone();
    a.design_version_id = Uuid::new_v4();
    DesignService::new(&t.app)
        .persist(&mut *first, &mut a)
        .await
        .unwrap();
    assert_eq!(a.version_number, 2);

    let app = t.app.clone();
    let mut b = template.clone();
    b.design_version_id = Uuid::new_v4();
    let second = tokio::spawn(async move {
        let mut tx = app.store.begin().await.unwrap();
        DesignService::new(&app)
            .persist(&mut *tx, &mut b)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        b.version_number
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!second.is_finished());
    assert_eq!(waiting_advisory_locks(db.schema_name().unwrap()).await, 1);
    first.commit().await.unwrap();
    assert_eq!(second.await.unwrap(), 3);
    let lv = layouts(&t, &d.tok, &d.pid).await;
    assert_eq!(version_numbers(&lv), vec![1, 2, 3]);
    db.finish().await;
}

/// A writer that does not take the lock (as a Python process would not) inserts the number the
/// route is about to use and commits while the route waits on the unique index: the route
/// renumbers instead of failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_without_the_lock_is_renumbered_around() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let t = TestApp::new(&db, &[]);
    let d = demo(&t).await;
    let v1 = first_layout(&t, &d).await;
    let (mut foreign, _) = design(&t, uuid(&json!(v1))).await;
    foreign.design_version_id = Uuid::new_v4();
    foreign.version_number = 2;
    let mut other = t.app.store.begin().await.unwrap();
    DESIGNS.add(&mut *other, &foreign).await.unwrap();

    let route = spawn_generate(
        &t,
        &d.tok,
        &d.pid,
        json!({"property_id": d.hollywood, "room_requirements": reqs(), "candidate_count": 1}),
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !route.is_finished(),
        "the route did not wait on the unique index"
    );
    other.commit().await.unwrap();
    let r = route.await.unwrap();
    assert_eq!(r["task"]["status"], "COMPLETED", "{r}");
    let id = r["task"]["output_reference"]["design_version_ids"][0].clone();
    let lv = layouts(&t, &d.tok, &d.pid).await;
    assert_eq!(version_numbers(&lv), vec![1, 2, 3]);
    let mine = lv.iter().find(|l| l["design_version_id"] == id).unwrap();
    assert_eq!(mine["version_number"], 3);
    db.finish().await;
}

/// A first bill being written holds the per-version lock: a concurrent first request waits and
/// returns that bill instead of storing a second one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_bills_store_one() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let t = TestApp::new(&db, &[]);
    let d = demo(&t).await;
    let v1 = first_layout(&t, &d).await;
    let (dv, _) = design(&t, uuid(&json!(v1))).await;
    let pid = uuid(&json!(d.pid));

    let mut first = t.app.store.begin().await.unwrap();
    let held = DesignService::new(&t.app)
        .bill_of_materials(&mut *first, pid, &dv)
        .await
        .unwrap();

    let router = t.router.clone();
    let path = format!("{}/{v1}/bom", base(&d.pid));
    let auth = format!("Bearer {}", d.tok);
    let second = tokio::spawn(async move {
        let req = Request::builder()
            .uri(path)
            .header("authorization", auth)
            .body(Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!second.is_finished(), "the second request did not wait");
    first.commit().await.unwrap();
    let got = second.await.unwrap();
    assert_eq!(got["bom_id"], held.bom_id.to_string(), "{got}");
    assert_eq!(count(&t, &schema::BILLS_OF_MATERIALS).await, 1);
    assert_eq!(
        of_type(&events(&t, &d.tok, &d.pid).await, "bom.generated").len(),
        1
    );
    db.finish().await;
}

// ---- the SiteGeometry a provider document becomes -------------------------------------------------

/// `DesignService.site_geometry`'s construction against Python's, for the fixture floor, the
/// LIVE documents `floor_geometry_from_model` returns (normalized first) and malformed documents
/// (the exception class a failed task records).
#[test]
fn site_geometry_documents_match_python() {
    use ss_domain::design::geometry::framing::normalize_plan;
    use ss_server::error::AppError;
    use ss_server::services::design::site_geometry_from_doc;
    let g = golden("design_service.json");
    let sg = &g["site_geometry"];
    let pid = uuid(&sg["property_id"]);
    let source = sg["source_id"].as_str().unwrap();
    let cases = sg["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 14);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let doc = c["doc"].as_object().unwrap().clone();
        let live = doc.get("data_origin").and_then(Value::as_str) == Some("LIVE");
        let got = (|| -> Result<Value, AppError> {
            let obj = if live {
                normalize_plan(&Value::Object(doc))?
                    .as_object()
                    .cloned()
                    .unwrap()
            } else {
                doc
            };
            let geo = site_geometry_from_doc(pid, source, &obj, live)?;
            let mut v = serde_json::to_value(geo).unwrap();
            v.as_object_mut().unwrap().remove("site_geometry_id");
            Ok(v)
        })();
        match (&c["error"], got) {
            (Value::Null, Ok(v)) => {
                assert_eq!(v, c["result"], "{name}");
                assert_conforms("SiteGeometry", &{
                    let mut full = v.clone();
                    full["site_geometry_id"] = json!(Uuid::new_v4());
                    full
                });
            }
            (Value::String(class), Err(e)) => {
                assert_eq!(e.python_class_name(), class, "{name}: {e:?}")
            }
            (want, got) => panic!("{name}: python {want}, rust {got:?}"),
        }
    }
}

/// A Matterport provider that serves one LIVE document (a measured scan) for the Hollywood
/// suite's asset, as the live provider would after `floor_geometry_from_model`.
struct LiveScan {
    asset: String,
    doc: Value,
}

impl ss_server::integrations::matterport::MatterportProvider for LiveScan {
    fn mode(&self) -> &str {
        "live"
    }
    fn get_floor_geometry(
        &self,
        asset_id: &str,
    ) -> Result<Option<Value>, ss_domain::integrations::base::ProviderError> {
        Ok((asset_id == self.asset).then(|| self.doc.clone()))
    }
    fn model_summaries(
        &self,
        _ids: &[String],
    ) -> Result<
        indexmap::IndexMap<String, ss_server::integrations::matterport::ModelSummary>,
        ss_domain::integrations::base::ProviderError,
    > {
        Ok(indexmap::IndexMap::new())
    }
    fn status(&self) -> ss_server::contracts::ProviderStatusEntry {
        ss_domain::integrations::base::status_entry(
            "matterport",
            "live",
            ss_core::IntegrationStatus::Live,
            true,
            "test double",
        )
    }
}

both_backends!(a_live_scan_is_normalized_and_stored_as_live, |db| {
    let t = TestApp::new(db, &[]);
    let golden = golden("design_service.json");
    let case = golden["site_geometry"]["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "live_rotated_with_partition_and_bad_edges")
        .unwrap()
        .clone();
    let mut integrations = ss_server::integrations::Integrations::from_settings(&t.app.settings);
    integrations.matterport = std::sync::Arc::new(LiveScan {
        asset: golden["site_geometry"]["source_id"]
            .as_str()
            .unwrap()
            .into(),
        doc: case["doc"].clone(),
    });
    t.app.extensions.insert(std::sync::Arc::new(integrations));
    let d = demo(&t).await;
    // Only a `provider: "matterport"` asset is read through the configured (here live) provider;
    // the fixture listing's stand-in says `provider: "fixture"` and would be read from the
    // fixture geometry (`tests/design_source.rs`). Mark it a Matterport scan, id unchanged.
    let mut tx = t.app.store.begin().await.unwrap();
    let mut prop = PROPERTIES
        .get(&mut *tx, uuid(&json!(d.hollywood)))
        .await
        .unwrap()
        .unwrap();
    prop.interior_assets[0].provider = "matterport".into();
    PROPERTIES.upsert(&mut *tx, &prop).await.unwrap();
    tx.commit().await.unwrap();
    let r = generate(
        &t,
        &d.tok,
        &d.pid,
        &json!({"property_id": d.hollywood, "room_requirements": [{"room_type": "treatment", "count": 1, "min_area_sqm": 6, "min_width_m": 2.4}], "candidate_count": 1}),
    )
    .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let mut tx = t.app.store.begin().await.unwrap();
    let stored = GEOMETRIES
        .list(
            &mut *tx,
            Query::new().eq("property_id", uuid(&json!(d.hollywood))),
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(stored.len(), 1);
    let mut got = serde_json::to_value(&stored[0]).unwrap();
    got.as_object_mut().unwrap().remove("site_geometry_id");
    // What Python stores for the same document, apart from the property id.
    let mut want = case["result"].clone();
    want["property_id"] = json!(d.hollywood);
    assert_eq!(got, want);
    assert_eq!(got["data_origin"], "LIVE");
    assert_eq!(got["geometry_source"], "MATTERPORT_MEASURED");
    assert_eq!(
        got["coordinate_reference"],
        "local_floor_origin (rotated 45 deg from provider frame)"
    );
    // Whatever the solver made of it, nothing generated is approved.
    for l in layouts(&t, &d.tok, &d.pid).await {
        assert_eq!(l["approval_status"], "PROPOSED");
    }
});

// ---- F1 of the part 3 review: nothing is held while the floor is fetched and solved ------------
//
// The provider fetch and the solve used to run inside the request's transaction, after the
// task's first event had taken the project's event lock: every other writer of the project
// waited for the whole solve, and queued requests held pool connections until other owners'
// requests failed. Now a short read transaction does the checks, the fetch and the solve run
// with no transaction, connection or lock held, and one write transaction records the task.

/// A gate that provider calls wait at: the test sees how many calls arrived and opens it.
#[derive(Default)]
struct Gate {
    state: std::sync::Mutex<(usize, bool)>,
    cv: std::sync::Condvar,
}

impl Gate {
    /// Called by the provider (on the blocking pool): count the call, wait until opened (at most
    /// 30 s, so a broken test cannot hang the suite).
    fn enter_and_wait(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut s = self.state.lock().unwrap();
        s.0 += 1;
        self.cv.notify_all();
        while !s.1 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            s = self.cv.wait_timeout(s, left).unwrap().0;
        }
    }

    /// Wait (without blocking the runtime) until `n` calls are waiting at the gate.
    async fn entered(&self, n: usize) {
        for _ in 0..2000 {
            if self.state.lock().unwrap().0 >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let started = self.calls();
        panic!("only {started} of {n} geometry fetches started");
    }

    fn open(&self) {
        self.state.lock().unwrap().1 = true;
        self.cv.notify_all();
    }

    fn calls(&self) -> usize {
        self.state.lock().unwrap().0
    }
}

/// The configured (fixture) Matterport provider, except that every floor-geometry call first
/// waits at the gate: a slow live scan.
struct GatedScan {
    inner: std::sync::Arc<dyn ss_server::integrations::matterport::MatterportProvider>,
    gate: std::sync::Arc<Gate>,
}

impl ss_server::integrations::matterport::MatterportProvider for GatedScan {
    fn mode(&self) -> &str {
        self.inner.mode()
    }
    fn get_floor_geometry(
        &self,
        asset_id: &str,
    ) -> Result<Option<Value>, ss_domain::integrations::base::ProviderError> {
        self.gate.enter_and_wait();
        self.inner.get_floor_geometry(asset_id)
    }
    fn model_summaries(
        &self,
        ids: &[String],
    ) -> Result<
        indexmap::IndexMap<String, ss_server::integrations::matterport::ModelSummary>,
        ss_domain::integrations::base::ProviderError,
    > {
        self.inner.model_summaries(ids)
    }
    fn status(&self) -> ss_server::contracts::ProviderStatusEntry {
        self.inner.status()
    }
}

fn gate_geometry_fetches(t: &TestApp) -> std::sync::Arc<Gate> {
    let gate = std::sync::Arc::new(Gate::default());
    let mut integrations = ss_server::integrations::Integrations::from_settings(&t.app.settings);
    integrations.matterport = std::sync::Arc::new(GatedScan {
        inner: integrations.matterport.clone(),
        gate: gate.clone(),
    });
    t.app.extensions.insert(std::sync::Arc::new(integrations));
    gate
}

/// This test's sessions (`application_name` = its schema): advisory locks granted to them, and
/// how many are inside a transaction.
async fn locks_and_open_transactions(schema: &str) -> (i64, i64) {
    use sqlx::ConnectOptions;
    let url = pg_url().unwrap();
    let opts = <sqlx::postgres::PgConnectOptions as std::str::FromStr>::from_str(
        &ss_server::config::normalize_database_url(&url),
    )
    .unwrap();
    let mut c = opts.connect().await.unwrap();
    let locks: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid \
         WHERE l.locktype = 'advisory' AND l.granted AND a.application_name = $1",
    )
    .bind(schema)
    .fetch_one(&mut c)
    .await
    .unwrap();
    let open: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE application_name = $1 AND xact_start IS NOT NULL AND pid <> pg_backend_pid()",
    )
    .bind(schema)
    .fetch_one(&mut c)
    .await
    .unwrap();
    (locks.0, open.0)
}

both_backends!(a_slow_geometry_fetch_holds_nothing, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let gate = gate_geometry_fetches(&t);
    let first = spawn_generate(
        &t,
        &d.tok,
        &d.pid,
        json!({"property_id": d.hollywood, "room_requirements": reqs()}),
    );
    // The first generation of the property is fetching its floor (the solve comes after it, in
    // the same phase).
    gate.entered(1).await;
    if let Some(schema) = db.schema_name() {
        assert_eq!(
            locks_and_open_transactions(schema).await,
            (0, 0),
            "the generation holds a lock or a transaction while it fetches"
        );
    }
    // Other writers of the same project do not wait for it: a profile update, an analysis and
    // a generation that fails at once (no measured geometry). Before the fix each of them waited
    // for the whole fetch and solve (the project's event lock on PostgreSQL, the store's lock in
    // memory).
    let quick = Duration::from_secs(10);
    let mut profile = demo_project_body()["business_profile"].clone();
    profile["confirmation_status"] = json!("CONFIRMED");
    let put = tokio::time::timeout(
        quick,
        t.put(
            &format!("/api/v1/projects/{}/business-profile", d.pid),
            Some(&d.tok),
            &profile,
        ),
    )
    .await
    .expect("the profile update waited for the layout's geometry fetch");
    assert_eq!(put.status, StatusCode::OK, "{}", put.text);
    let analyze = tokio::time::timeout(
        quick,
        t.post(
            &format!(
                "/api/v1/projects/{}/properties/{}/analyze",
                d.pid, d.alhambra
            ),
            Some(&d.tok),
            &json!({}),
        ),
    )
    .await
    .expect("the analysis waited for the layout's geometry fetch");
    assert_eq!(
        analyze.body["task"]["status"], "COMPLETED",
        "{}",
        analyze.text
    );
    let other = tokio::time::timeout(
        quick,
        generate(
            &t,
            &d.tok,
            &d.pid,
            &json!({"property_id": d.alhambra, "room_requirements": reqs()}),
        ),
    )
    .await
    .expect("another generation waited for the layout's geometry fetch");
    assert_eq!(other.body["task"]["status"], "FAILED", "{}", other.text);
    assert!(!first.is_finished());

    gate.open();
    let r = first.await.unwrap();
    assert_eq!(r["task"]["status"], "COMPLETED", "{r}");
    assert_eq!(r["task"]["task_type"], "generate_layout");
    assert_eq!(
        r["task"]["output_reference"]["design_version_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(count(&t, &schema::SITE_GEOMETRIES).await, 1);
    assert_eq!(layouts(&t, &d.tok, &d.pid).await.len(), 2);
    // The task's events are in Python's order, in one transaction.
    let task_id = r["task"]["task_id"].as_str().unwrap();
    let ev = events(&t, &d.tok, &d.pid).await;
    let mine: Vec<&str> = ev
        .iter()
        .filter(|e| {
            e["correlation_id"] == r["task"]["correlation_id"] || e["payload"]["task_id"] == task_id
        })
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        mine,
        [
            "task.started",
            "layout.version_created",
            "layout.version_created",
            "task.completed"
        ]
    );
});

both_backends!(concurrent_first_generations_share_one_geometry, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let gate = gate_geometry_fetches(&t);
    let body = json!({"property_id": d.hollywood, "room_requirements": reqs()});
    let a = spawn_generate(&t, &d.tok, &d.pid, body.clone());
    let b = spawn_generate(&t, &d.tok, &d.pid, body);
    // Both found no stored geometry and are fetching one at the same time.
    gate.entered(2).await;
    gate.open();
    let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
    for r in [&ra, &rb] {
        assert_eq!(r["task"]["status"], "COMPLETED", "{r}");
        assert_eq!(
            r["task"]["output_reference"]["design_version_ids"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    // One geometry is stored; the request that lost the race solved again on it (it did not
    // fetch a third time), so every version refers to the stored geometry.
    let mut tx = t.app.store.begin().await.unwrap();
    let geos = GEOMETRIES
        .list(
            &mut *tx,
            Query::new().eq("property_id", uuid(&json!(d.hollywood))),
        )
        .await
        .unwrap();
    let prop = PROPERTIES
        .require(&mut *tx, uuid(&json!(d.hollywood)))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(geos.len(), 1);
    assert_eq!(gate.calls(), 2);
    assert_eq!(prop.geometry_reference, Some(geos[0].site_geometry_id));
    let lv = layouts(&t, &d.tok, &d.pid).await;
    assert_eq!(version_numbers(&lv), [1, 2, 3, 4]);
    for l in &lv {
        assert_eq!(uuid(&l["geometry_id"]), geos[0].site_geometry_id);
    }
    let ev = events(&t, &d.tok, &d.pid).await;
    assert_eq!(of_type(&ev, "layout.version_created").len(), 4);
    assert_eq!(
        of_type(&ev, "task.completed").len(),
        3,
        "search + two generations"
    );
    assert!(of_type(&ev, "task.failed").is_empty());
});

// The review's cross-user failure: queued generations of one project held pool connections
// (the test pool has 4) until another owner's request failed. Twelve first generations now wait
// in their fetch holding nothing; another owner is served at once; afterwards eleven of them
// find the geometry the first one stored and solve again on it.
both_backends!(queued_generations_do_not_starve_other_owners, |db| {
    let t = TestApp::new(db, &[]);
    let d = demo(&t).await;
    let gate = gate_geometry_fetches(&t);
    let body = json!({"property_id": d.hollywood, "room_requirements": reqs()});
    let n = 12;
    let tasks: Vec<_> = (0..n)
        .map(|_| spawn_generate(&t, &d.tok, &d.pid, body.clone()))
        .collect();
    gate.entered(n).await;
    let stranger = t.token_for(Uuid::new_v4());
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        t.get("/api/v1/projects", Some(&stranger)),
    )
    .await
    .expect("another owner waited for the queued generations");
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    gate.open();
    for task in tasks {
        let r = task.await.unwrap();
        assert_eq!(r["task"]["status"], "COMPLETED", "{r}");
    }
    assert_eq!(
        gate.calls(),
        n,
        "a retry fetched again instead of using the stored geometry"
    );
    assert_eq!(count(&t, &schema::SITE_GEOMETRIES).await, 1);
    let lv = layouts(&t, &d.tok, &d.pid).await;
    assert_eq!(version_numbers(&lv), (1..=2 * n as i64).collect::<Vec<_>>());
    let geometry = lv[0]["geometry_id"].clone();
    assert!(lv.iter().all(|l| l["geometry_id"] == geometry));
});
