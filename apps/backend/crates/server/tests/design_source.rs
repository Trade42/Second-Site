//! Where a layout's floor comes from when nothing is stored yet: the asset's own provider, not
//! the configured mode (D1 of `orchestrator/reports/live-2.md`). On both stores.
//!
//! With `MATTERPORT_PROVIDER_MODE=live`, the fixture listing's stand-in scan
//! (`fixture-matterport-hollywood-302`, `provider: "fixture"`) used to be sent to the real
//! Matterport API, which answered `not.found`, and the layout task FAILED. A fixture asset is now
//! read from the fixture geometry whatever the mode, stays labelled as fixture data, and never
//! reaches the live provider; an asset whose provider is `"matterport"` still does.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::{json, Value};
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};
use ss_server::integrations::matterport::{MatterportProvider, ModelSummary};
use ss_server::store::repo::PROPERTIES;
use uuid::Uuid;

use common::*;

const HOLLYWOOD: &str = "6922 W HOLLYWOOD BLVD";
const FIXTURE_ASSET: &str = "fixture-matterport-hollywood-302";

/// A live-mode Matterport provider that records every model id it is asked for. Like the real
/// API for an id it does not know, it answers a provider error for any `fixture-*` id; it serves
/// one LIVE document for `asset`.
struct RecordingLive {
    asset: String,
    doc: Value,
    geometry_calls: Mutex<Vec<String>>,
    summary_calls: Mutex<Vec<String>>,
}

impl MatterportProvider for RecordingLive {
    fn mode(&self) -> &str {
        "live"
    }
    fn get_floor_geometry(&self, asset_id: &str) -> Result<Option<Value>, ProviderError> {
        self.geometry_calls
            .lock()
            .unwrap()
            .push(asset_id.to_owned());
        if asset_id.starts_with("fixture-") {
            return Err(ProviderError::new(
                "matterport",
                ProviderErrorKind::BadResponse,
                "Error fetching data (path: /model, code: not.found): Not Found",
            ));
        }
        Ok((asset_id == self.asset).then(|| self.doc.clone()))
    }
    fn model_summaries(
        &self,
        ids: &[String],
    ) -> Result<indexmap::IndexMap<String, ModelSummary>, ProviderError> {
        self.summary_calls
            .lock()
            .unwrap()
            .extend(ids.iter().cloned());
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

/// A model id shaped like a real one (the golden case's own id is the fixture stand-in's).
const LIVE_MODEL: &str = "SxQL3iGyoDo";

/// The golden LIVE document (a measured, rotated scan).
fn live_doc() -> Value {
    let golden = golden("design_service.json");
    let case = golden["site_geometry"]["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "live_rotated_with_partition_and_bad_edges")
        .unwrap()
        .clone();
    case["doc"].clone()
}

fn install(t: &TestApp) -> Arc<RecordingLive> {
    let stub = Arc::new(RecordingLive {
        asset: LIVE_MODEL.to_owned(),
        doc: live_doc(),
        geometry_calls: Mutex::new(Vec::new()),
        summary_calls: Mutex::new(Vec::new()),
    });
    let mut integrations = ss_server::integrations::Integrations::from_settings(&t.app.settings);
    integrations.matterport = stub.clone();
    t.app.extensions.insert(Arc::new(integrations));
    stub
}

/// Dev token, the demo project, the fixture search; the Hollywood property id.
async fn hollywood(t: &TestApp) -> (String, String, String) {
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
    (tok, pid, by[HOLLYWOOD].clone())
}

/// Generate one layout and return the task and the stored geometry the first layout uses.
async fn layout_geometry(t: &TestApp, tok: &str, pid: &str, property: &str) -> (Value, Value) {
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/layouts/generate"),
            Some(tok),
            &json!({"property_id": property, "room_requirements": [{"room_type": "treatment", "count": 1, "min_area_sqm": 6, "min_width_m": 2.4}], "candidate_count": 1}),
        )
        .await;
    let task = r.body["task"].clone();
    assert_eq!(task["status"], "COMPLETED", "{}", r.text);
    let lv = t
        .get(&format!("/api/v1/projects/{pid}/layouts"), Some(tok))
        .await;
    assert_eq!(lv.status, StatusCode::OK, "{}", lv.text);
    let gid = lv.body[0]["geometry_id"].as_str().unwrap().to_owned();
    let g = t
        .get(
            &format!("/api/v1/projects/{pid}/layouts/geometry/{gid}"),
            Some(tok),
        )
        .await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.text);
    (task, g.body)
}

both_backends!(
    a_fixture_asset_in_live_mode_never_reaches_the_provider,
    |db| {
        let t = TestApp::new(db, &[]);
        let stub = install(&t);
        let (tok, pid, property) = hollywood(&t).await;
        let (_task, g) = layout_geometry(&t, &tok, &pid, &property).await;
        // `fixtures/geometry/hollywood_302_synthetic.json`, labelled as the fixture it is.
        assert_eq!(g["source_id"], FIXTURE_ASSET);
        assert_eq!(g["data_origin"], "FIXTURE_SYNTHETIC");
        assert_eq!(g["geometry_source"], "FIXTURE");
        assert_eq!(g["scale_verified"], false);
        assert_eq!(
            g["floor_boundary"],
            json!([[0.0, 0.0], [12.0, 0.0], [12.0, 11.6], [0.0, 11.6]])
        );
        // The live provider was asked for nothing: no geometry, and (the search's enrichment only
        // reads `provider: "matterport"` assets) no summaries.
        assert_eq!(*stub.geometry_calls.lock().unwrap(), Vec::<String>::new());
        assert_eq!(*stub.summary_calls.lock().unwrap(), Vec::<String>::new());
    }
);

both_backends!(a_matterport_asset_in_live_mode_goes_to_the_provider, |db| {
    let t = TestApp::new(db, &[]);
    let stub = install(&t);
    let (tok, pid, property) = hollywood(&t).await;
    // Make the Hollywood scan a real Matterport asset (as the CoStar kit lists them).
    let id = Uuid::parse_str(&property).unwrap();
    let mut tx = t.app.store.begin().await.unwrap();
    let mut prop = PROPERTIES.get(&mut *tx, id).await.unwrap().unwrap();
    prop.interior_assets[0].provider = "matterport".into();
    prop.interior_assets[0].asset_id = stub.asset.clone();
    PROPERTIES.upsert(&mut *tx, &prop).await.unwrap();
    tx.commit().await.unwrap();

    let (_task, g) = layout_geometry(&t, &tok, &pid, &property).await;
    assert_eq!(g["source_id"], stub.asset.as_str());
    assert_eq!(g["data_origin"], "LIVE");
    assert_eq!(g["geometry_source"], "MATTERPORT_MEASURED");
    assert_eq!(
        *stub.geometry_calls.lock().unwrap(),
        vec![stub.asset.clone()]
    );
});

// ---- R1 of `live-2.md`: a model the API does not know is `None`, not a provider error ---------
//
// The canned answers below are reconstructed from the adapter's error message as observed on
// 2026-11-15 (`[matterport] BAD_RESPONSE: Error fetching data (path: /model, code: not.found):
// Not Found`), not captured raw: only the message text is known, and only it is matched.

use common::mock_http::{Canned, MockServer};
use ss_domain::integrations::base::RetryPolicy;
use ss_server::integrations::LiveMatterportProvider;
use ss_server::services::design::DesignService;

const NOT_FOUND_MSG: &str = "Error fetching data (path: /model, code: not.found): Not Found";
const UNKNOWN_MODEL_MSG: &str = "no measured geometry available for this property: Matterport \
                                 does not know the model, or this token cannot see it";

fn graphql(body: Value) -> Canned {
    Canned {
        status: 200,
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&body).unwrap(),
    }
}

fn not_found_answer() -> Canned {
    graphql(json!({
        "errors": [{"message": NOT_FOUND_MSG, "locations": [{"line": 3, "column": 3}]}],
        "data": null,
    }))
}

fn quick() -> RetryPolicy {
    RetryPolicy {
        attempts: 1,
        backoff_s: 0.0,
        timeout_s: 2.0,
    }
}

fn live(base: &str) -> LiveMatterportProvider {
    LiveMatterportProvider::with_policy(&format!("{base}/graph"), Some("tid"), Some("ts"), quick())
}

#[test]
fn an_unknown_model_is_none_and_cached() {
    let server = MockServer::start(vec![not_found_answer()]);
    let p = live(&server.base);
    assert!(!p.model_not_found(LIVE_MODEL));
    assert_eq!(p.get_floor_geometry(LIVE_MODEL).unwrap(), None);
    assert!(p.model_not_found(LIVE_MODEL));
    // Cached like any other `None`: no second request.
    assert_eq!(p.get_floor_geometry(LIVE_MODEL).unwrap(), None);
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn every_other_geometry_error_stays_an_error() {
    let cases = [
        // A `not.found` on another path.
        (
            json!({"errors": [{"message": "Error fetching data (path: /model/floors, code: not.found): Not Found"}], "data": null}),
            "BAD_RESPONSE",
        ),
        // A `not.found` alongside another error.
        (
            json!({"errors": [{"message": NOT_FOUND_MSG}, {"message": "Cannot query field 'bogus'"}], "data": null}),
            "BAD_RESPONSE",
        ),
        // Authentication (the message says so).
        (
            json!({"errors": [{"message": "Not Authorized for model"}]}),
            "AUTH",
        ),
        // An error without a message.
        (json!({"errors": [{"code": "not.found"}]}), "BAD_RESPONSE"),
    ];
    for (body, kind) in cases {
        let server = MockServer::start(vec![graphql(body.clone())]);
        let p = live(&server.base);
        let e = p.get_floor_geometry(LIVE_MODEL).unwrap_err();
        assert_eq!(e.kind.as_str(), kind, "{body}: {e}");
        assert!(!p.model_not_found(LIVE_MODEL), "{body}");
    }
    // HTTP status errors are not a `None`, even with the not-found text in the body.
    for (status, kind) in [(401u16, "AUTH"), (429, "RATE_LIMITED")] {
        let server = MockServer::start(vec![Canned {
            status,
            content_type: Some("application/json".into()),
            body: br#"{"errors":[{"message":"Error fetching data (path: /model, code: not.found): Not Found"}]}"#.to_vec(),
        }]);
        let e = live(&server.base)
            .get_floor_geometry(LIVE_MODEL)
            .unwrap_err();
        assert_eq!(e.kind.as_str(), kind, "{status}: {e}");
    }
    // A malformed answer.
    let server = MockServer::start(vec![graphql(json!({"data": {"model": 7}}))]);
    let e = live(&server.base)
        .get_floor_geometry(LIVE_MODEL)
        .unwrap_err();
    assert_eq!(e.kind.as_str(), "BAD_RESPONSE", "{e}");
}

/// The Hollywood property with one Matterport asset `LIVE_MODEL`, and the live adapter on `base`.
async fn hollywood_on(t: &TestApp, base: &str) -> (String, String, String) {
    let mut integrations = ss_server::integrations::Integrations::from_settings(&t.app.settings);
    integrations.matterport = Arc::new(live(base));
    t.app.extensions.insert(Arc::new(integrations));
    let (tok, pid, property) = hollywood(t).await;
    let id = Uuid::parse_str(&property).unwrap();
    let mut tx = t.app.store.begin().await.unwrap();
    let mut prop = PROPERTIES.get(&mut *tx, id).await.unwrap().unwrap();
    prop.interior_assets[0].provider = "matterport".into();
    prop.interior_assets[0].asset_id = LIVE_MODEL.into();
    PROPERTIES.upsert(&mut *tx, &prop).await.unwrap();
    tx.commit().await.unwrap();
    (tok, pid, property)
}

async fn generate_task(t: &TestApp, tok: &str, pid: &str, property: &str) -> Value {
    t.post(
        &format!("/api/v1/projects/{pid}/layouts/generate"),
        Some(tok),
        &json!({"property_id": property, "room_requirements": [{"room_type": "treatment", "count": 1, "min_area_sqm": 6, "min_width_m": 2.4}], "candidate_count": 1}),
    )
    .await
    .body["task"]
        .clone()
}

both_backends!(an_unknown_model_answers_geometry_unavailable, |db| {
    let t = TestApp::new(db, &[]);
    // The search's enrichment asks first (`model_summaries`, unchanged: here it gets the same
    // not-found and records the scans as not retrieved); then the layout's geometry fetch.
    let server = MockServer::start_with(|_| not_found_answer());
    let (tok, pid, property) = hollywood_on(&t, &server.base).await;
    let task = generate_task(&t, &tok, &pid, &property).await;
    assert_eq!(task["status"], "FAILED", "{task}");
    assert_eq!(task["error_code"], "NotFoundError", "{task}");
    assert_eq!(task["error_message"], UNKNOWN_MODEL_MSG, "{task}");

    // The service itself: 404 GEOMETRY_UNAVAILABLE with the plain-words message.
    let svc = DesignService::new(&t.app);
    let mut tx = t.app.store.begin().await.unwrap();
    let source = svc
        .site_source(
            &mut *tx,
            Uuid::parse_str(&pid).unwrap(),
            Uuid::parse_str(&property).unwrap(),
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let err = svc
        .prepare_site(Uuid::parse_str(&property).unwrap(), source)
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
    assert_eq!(err.error_code(), "GEOMETRY_UNAVAILABLE");
    assert_eq!(err.to_string(), UNKNOWN_MODEL_MSG);
});

both_backends!(a_provider_that_is_down_still_fails_the_task, |db| {
    let t = TestApp::new(db, &[]);
    // Nothing listens on this port.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (tok, pid, property) = hollywood_on(&t, &format!("http://127.0.0.1:{port}")).await;
    let task = generate_task(&t, &tok, &pid, &property).await;
    assert_eq!(task["status"], "FAILED", "{task}");
    assert_eq!(task["error_code"], "ProviderError", "{task}");
    assert!(
        task["error_message"]
            .as_str()
            .unwrap()
            .starts_with("[matterport] UNAVAILABLE"),
        "{task}"
    );
});
