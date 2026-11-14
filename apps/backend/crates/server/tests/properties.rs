//! Property routes and the property-analysis workflow, on both stores. Ports of
//! `apps/api/tests/test_api_smoke.py` (`test_end_to_end_property_slice` up to the design studio,
//! `test_unauthorized_project_access`, `test_missing_profile_is_explicit`) and both tests of
//! `test_events_workflow.py`, plus project and owner isolation for every route of this slice.

mod common;

use std::collections::BTreeSet;

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use ss_core::{DataOrigin, EvidenceClassification, Market, ProjectContext};
use ss_domain::contracts::{CostScenario, PropertySearchRequest, SourceEvidence};
use ss_server::app::AppState;
use ss_server::contracts::{ActorType, EventType};
use ss_server::services::events::Emit;
use ss_server::services::evidence::EvidenceService;
use ss_server::store::repo::{
    events, users, User, COST_SCENARIOS, PROFILES, PROJECTS, PROPERTIES, RANKINGS,
};
use ss_server::store::{schema, Query, Tx};
use ss_server::workflows::property_analysis::PropertyAnalysisWorkflow;
use uuid::Uuid;

use common::*;

const ALHAMBRA: &str = "5380 E ALHAMBRA AVE";
const HOLLYWOOD: &str = "6922 W HOLLYWOOD BLVD";

async fn project_with_demo_profile(t: &TestApp, token: &str) -> String {
    let body = t.create_project(token, &demo_project_body()).await;
    assert_eq!(body["business_profile"]["confirmation_status"], "CONFIRMED");
    body["project"]["project_id"].as_str().unwrap().to_owned()
}

async fn search(t: &TestApp, token: &str, pid: &str) -> Resp {
    t.post(
        &format!("/api/v1/projects/{pid}/properties/search"),
        Some(token),
        &json!({"city": "Los Angeles", "state": "CA"}),
    )
    .await
}

fn by_street(props: &Value) -> std::collections::HashMap<String, Value> {
    props
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["property_identity"]["street_address"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                p.clone(),
            )
        })
        .collect()
}

// ---- test_api_smoke.py ----------------------------------------------------------------------------

both_backends!(end_to_end_property_slice, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _user) = t.dev_token().await;
    assert_eq!(t.get("/health", None).await.body["status"], "ok");
    let ready = t.get("/ready", None).await.body;
    if t.kind == Kind::Postgres {
        assert_eq!(ready["checks"]["database"], "ok");
    }
    let statuses: std::collections::HashMap<&str, &str> = ready["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["provider"].as_str().unwrap(),
                p["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(statuses["costar"], "MOCK");

    let pid = project_with_demo_profile(&t, &tok).await;
    let r = search(&t, &tok, &pid).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_conforms("TaskAccepted", &r.body);
    let task = &r.body["task"];
    assert_eq!(task["status"], "COMPLETED", "{task}");
    let status_url = r.body["status_url"].as_str().unwrap();
    assert_eq!(
        t.get(status_url, Some(&tok)).await.body["status"],
        "COMPLETED"
    );

    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    assert_eq!(props.as_array().unwrap().len(), 2);
    for p in props.as_array().unwrap() {
        assert_conforms("PropertyDigitalProfile", p);
    }
    let by_addr = by_street(&props);
    let alh = &by_addr[ALHAMBRA];
    // Never presented as CoStar.
    assert_eq!(alh["commercial_listing"]["provider"], "fixture");
    assert_eq!(
        alh["commercial_listing"]["data_origin"],
        "FIXTURE_SYNTHETIC"
    );
    let alh_id = alh["property_id"].as_str().unwrap();

    let permits = t
        .get(
            &format!("/api/v1/projects/{pid}/properties/{alh_id}/permits"),
            Some(&tok),
        )
        .await
        .body;
    let p800 = permits
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["permit_number"] == "19041-10000-39945")
        .expect("the 800A permit");
    assert_eq!(p800["data_origin"], "FIXTURE_REAL");
    assert_eq!(p800["permit_status"], "Permit Finaled");
    assert_conforms("PermitRecord", p800);

    let feas = t
        .get(
            &format!("/api/v1/projects/{pid}/properties/{alh_id}/feasibility"),
            Some(&tok),
        )
        .await
        .body;
    assert!(feas
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["status"] == "UNKNOWN"));
    for a in feas.as_array().unwrap() {
        assert_conforms("InfrastructureAssessment", a);
    }

    let rankings = t
        .get(&format!("/api/v1/projects/{pid}/rankings"), Some(&tok))
        .await
        .body;
    assert_eq!(rankings.as_array().unwrap().len(), 1);
    assert_eq!(
        rankings[0]["ranked_properties"].as_array().unwrap().len(),
        2
    );
    assert_conforms("PropertyRankingResult", &rankings[0]);
    let scenarios = t
        .get(
            &format!("/api/v1/projects/{pid}/cost-scenarios"),
            Some(&tok),
        )
        .await
        .body;
    assert_eq!(scenarios.as_array().unwrap().len(), 2);
    assert_conforms("CostScenario", &scenarios[0]);

    let events = t
        .get(&format!("/api/v1/projects/{pid}/events"), Some(&tok))
        .await
        .body["events"]
        .clone();
    let types: Vec<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    for want in [
        "project.profile.confirmed",
        "property.search.completed",
        "property.identity.resolved",
        "property.permits.ingested",
        "property.analysis.completed",
        "infrastructure.unknown_detected",
        "financial.scenario.updated",
        "property.ranking.updated",
        "task.completed",
    ] {
        assert!(types.contains(&want), "{want} missing from {types:?}");
    }
    let permit_corr: BTreeSet<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            e["event_type"]
                .as_str()
                .unwrap()
                .starts_with("property.permits")
        })
        .map(|e| e["correlation_id"].as_str().unwrap())
        .collect();
    assert_eq!(permit_corr.len(), 1);
    // The project moved to COMPARING.
    let project = t
        .get(&format!("/api/v1/projects/{pid}"), Some(&tok))
        .await
        .body;
    assert_eq!(project["project"]["project_status"], "COMPARING");

    // A second search finds the same addresses: nothing is stored twice, a new ranking version.
    let r = search(&t, &tok, &pid).await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let props2 = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    assert_eq!(props2.as_array().unwrap().len(), 2);
    let versions: Vec<i64> = t
        .get(&format!("/api/v1/projects/{pid}/rankings"), Some(&tok))
        .await
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["ranking_version"].as_i64().unwrap())
        .collect();
    assert_eq!(versions, vec![1, 2]);
    assert_eq!(
        t.get(
            &format!("/api/v1/projects/{pid}/cost-scenarios"),
            Some(&tok)
        )
        .await
        .body
        .as_array()
        .unwrap()
        .len(),
        2,
        "earlier scenarios are superseded"
    );

    // Re-analysis of one property: 202, superseding its own outputs only.
    let r = t
        .call(
            Method::POST,
            &format!("/api/v1/projects/{pid}/properties/{alh_id}/analyze"),
            Some(&tok),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    assert_eq!(r.body["task"]["task_type"], "evaluate_infrastructure");
    let out = &r.body["task"]["output_reference"];
    assert!(out["assessment_ids"].as_array().unwrap().len() > 3);
    assert!(out["scenario_id"].is_string() && out["ranking_id"].is_string());
    let scen = t
        .get(
            &format!("/api/v1/projects/{pid}/cost-scenarios"),
            Some(&tok),
        )
        .await
        .body;
    assert_eq!(scen.as_array().unwrap().len(), 2);
    assert!(scen
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["scenario_id"] == out["scenario_id"]));
});

both_backends!(unauthorized_project_access, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let pid = project_with_demo_profile(&t, &tok).await;
    assert_eq!(
        t.get(&format!("/api/v1/projects/{pid}"), None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let other = t.token_for(Uuid::new_v4());
    let r = t
        .get(&format!("/api/v1/projects/{pid}"), Some(&other))
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(
        t.status_of(&format!("/api/v1/projects/{pid}/events"), Some(&other))
            .await,
        StatusCode::NOT_FOUND
    );
    let bad = t
        .get(&format!("/api/v1/projects/{pid}"), Some("junk"))
        .await;
    assert_problem(&bad, 401, "UNAUTHORIZED");
});

both_backends!(missing_profile_is_explicit, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let pid = t
        .create_project(
            &tok,
            &json!({"project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.body["task"]["status"], "FAILED");
    assert_eq!(r.body["task"]["error_code"], "NotFoundError");
    assert_eq!(
        r.body["task"]["error_message"],
        "business profile not found; create one first"
    );
    // Nothing of the failed slice is kept; the failed task and its events are.
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    assert_eq!(props, json!([]));
});

// ---- request checks, as Python answers them (probed with FastAPI's TestClient) ----------------------

both_backends!(request_checks_follow_fastapi_order, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let pid = project_with_demo_profile(&t, &tok).await;
    let other = t.token_for(Uuid::new_v4());
    let send_raw = |token: Option<String>, body: &'static [u8]| {
        let mut b = axum::http::Request::builder()
            .method(Method::POST)
            .uri(format!("/api/v1/projects/{pid}/properties/search"))
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(axum::body::Body::from(body)).unwrap()
    };
    // Invalid JSON is rejected before authentication and the project check.
    let r = t.send(send_raw(Some(other.clone()), b"{bad")).await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(r.body["details"]["errors"][0]["type"], "json_invalid");
    let r = t.send(send_raw(None, b"{bad")).await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    // A well-formed body with an extra field: the project check comes first.
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&other),
            &json!({"zzz": 1}),
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "project not found");
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({"limit": 0}),
        )
        .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(
        r.body["details"]["errors"][0]["loc"],
        json!(["body", "limit"])
    );
    // A malformed property id under another owner's project is the project's 404.
    let r = t
        .get(
            &format!("/api/v1/projects/{pid}/properties/not-a-uuid"),
            Some(&other),
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    let r = t
        .get(
            &format!("/api/v1/projects/{pid}/properties/not-a-uuid"),
            Some(&tok),
        )
        .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(
        r.body["details"]["errors"][0]["loc"],
        json!(["path", "property_id"])
    );
    let missing = Uuid::new_v4();
    for (m, path) in [
        (
            Method::GET,
            format!("/api/v1/projects/{pid}/properties/{missing}"),
        ),
        (
            Method::POST,
            format!("/api/v1/projects/{pid}/properties/{missing}/analyze"),
        ),
        (
            Method::GET,
            format!("/api/v1/projects/{pid}/properties/{missing}/permits"),
        ),
        (
            Method::GET,
            format!("/api/v1/projects/{pid}/properties/{missing}/feasibility"),
        ),
    ] {
        let r = t.call(m, &path, Some(&tok), None).await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "property not found", "{path}");
    }
});

// ---- isolation ------------------------------------------------------------------------------------

both_backends!(isolation_between_owners_and_projects, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let pid = project_with_demo_profile(&t, &tok).await;
    assert_eq!(
        search(&t, &tok, &pid).await.body["task"]["status"],
        "COMPLETED"
    );
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    let prop_id = props[0]["property_id"].as_str().unwrap().to_owned();

    // A second project of the same owner, and a second owner with a project of their own.
    let pid2 = project_with_demo_profile(&t, &tok).await;
    let other = t.token_for(Uuid::new_v4());
    let pid_other = project_with_demo_profile(&t, &other).await;

    let routes = |p: &str, prop: &str| {
        vec![
            (
                Method::POST,
                format!("/api/v1/projects/{p}/properties/search"),
                Some(json!({})),
            ),
            (
                Method::GET,
                format!("/api/v1/projects/{p}/properties"),
                None,
            ),
            (
                Method::GET,
                format!("/api/v1/projects/{p}/properties/{prop}"),
                None,
            ),
            (
                Method::POST,
                format!("/api/v1/projects/{p}/properties/{prop}/analyze"),
                None,
            ),
            (
                Method::GET,
                format!("/api/v1/projects/{p}/properties/{prop}/permits"),
                None,
            ),
            (
                Method::GET,
                format!("/api/v1/projects/{p}/properties/{prop}/feasibility"),
                None,
            ),
            (Method::GET, format!("/api/v1/projects/{p}/rankings"), None),
            (
                Method::GET,
                format!("/api/v1/projects/{p}/cost-scenarios"),
                None,
            ),
        ]
    };
    // The other owner gets 404 `project not found` on every route of my project.
    for (m, path, body) in routes(&pid, &prop_id) {
        let r = t.call(m, &path, Some(&other), body.as_ref()).await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "project not found", "{path}");
    }
    // My property under my other project: 404 `property not found`.
    for (m, path, body) in routes(&pid2, &prop_id)
        .into_iter()
        .filter(|(_, p, _)| p.contains(&prop_id))
    {
        let r = t.call(m, &path, Some(&tok), body.as_ref()).await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "property not found", "{path}");
    }
    // My property under the other owner's project, with their token: 404 too.
    let r = t
        .get(
            &format!("/api/v1/projects/{pid_other}/properties/{prop_id}"),
            Some(&other),
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    // The other projects see nothing of my search.
    for (p, token) in [(&pid2, &tok), (&pid_other, &other)] {
        for path in [
            format!("/api/v1/projects/{p}/properties"),
            format!("/api/v1/projects/{p}/rankings"),
            format!("/api/v1/projects/{p}/cost-scenarios"),
        ] {
            let r = t.get(&path, Some(token)).await;
            assert_eq!(r.status, StatusCode::OK);
            assert_eq!(r.body, json!([]), "{path}");
        }
    }
    // The events and tasks created by the search belong only to that project.
    let mut tx = t.app.store.begin().await.unwrap();
    let pid_u: Uuid = pid.parse().unwrap();
    let rows = events::select(&mut *tx, Query::new()).await.unwrap();
    let mine: Vec<_> = rows
        .iter()
        .map(|r| events::parse(r).unwrap())
        .filter(|e| e.event_type != EventType::ProjectProfileConfirmed)
        .collect();
    assert!(!mine.is_empty());
    assert!(
        mine.iter().all(|e| e.project_id == pid_u),
        "an event of the search landed elsewhere"
    );
    let tasks = tx.select(&schema::TASKS, &Query::new()).await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].uuid("project_id").unwrap(), pid_u);
    // The task status URL is not readable through another project.
    let task_id = tasks[0].uuid("id").unwrap();
    tx.rollback().await.unwrap();
    let r = t
        .get(
            &format!("/api/v1/projects/{pid2}/tasks/{task_id}"),
            Some(&tok),
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
});

// ---- test_events_workflow.py ------------------------------------------------------------------------

/// `_seed`: an owner, a project, the demo profile without opening capital, and the full slice.
async fn seed_slice(app: &AppState) -> ProjectContext {
    let mut txb = app.store.begin().await.unwrap();
    let tx: &mut dyn Tx = &mut *txb;
    let owner = Uuid::new_v4();
    users::insert(
        tx,
        &User {
            id: owner,
            display_name: "o".into(),
            email: None,
            phone: None,
        },
    )
    .await
    .unwrap();
    let project = ProjectContext::new(owner, "t", Market::new("Los Angeles", "CA"));
    PROJECTS.add(tx, &project).await.unwrap();
    let mut fx = read_json("fixtures/projects/facial_studio.json")["business_profile"].clone();
    fx["project_id"] = json!(project.project_id.to_string());
    fx["opening_capital"] = Value::Null;
    let profile =
        <ss_core::BusinessExpansionProfile as ss_core::Contract>::from_json_value(fx).unwrap();
    PROFILES.add(tx, &profile).await.unwrap();
    PropertyAnalysisWorkflow::new(app)
        .run(
            tx,
            project.project_id,
            PropertySearchRequest::default(),
            Uuid::new_v4(),
        )
        .await
        .unwrap();
    txb.commit().await.unwrap();
    project
}

both_backends!(evidence_update_recomputes_downstream, |db| {
    let app = ss_server::build_app(test_settings(&[]), db.store.clone());
    let project = seed_slice(&app).await;
    let mut txb = app.store.begin().await.unwrap();
    let tx: &mut dyn Tx = &mut *txb;
    let props = PROPERTIES
        .list(tx, Query::new().eq("project_id", project.project_id))
        .await
        .unwrap();
    let hw = props
        .iter()
        .find(|p| p.property_identity.street_address == HOLLYWOOD)
        .unwrap()
        .clone();
    let before: BTreeSet<i64> = RANKINGS
        .list(tx, Query::new().eq("project_id", project.project_id))
        .await
        .unwrap()
        .iter()
        .map(|r| r.ranking_version)
        .collect();
    assert_eq!(before, BTreeSet::from([1]));
    let ev = SourceEvidence {
        evidence_id: Uuid::new_v4(),
        project_id: Some(project.project_id),
        property_id: Some(hw.property_id),
        provider: "owner".into(),
        dataset: None,
        source_record_id: None,
        source_url: None,
        source_asset_reference: None,
        data_origin: DataOrigin::UserProvided,
        retrieved_at: ss_core::utcnow(),
        source_effective_at: None,
        observation_type: "electrical_service_amps".into(),
        extracted_value: json!(200),
        unit: Some("A".into()),
        evidence_classification: EvidenceClassification::OwnerReported,
        match_confidence: None,
        verification_status: ss_core::VerificationStatus::Unverified,
        limitations: vec!["Reported by property manager on a call; not inspected.".into()],
    };
    EvidenceService.add(tx, &ev).await.unwrap();
    let emit = || {
        Emit::new(
            EventType::InfrastructureEvidenceUpdated,
            project.project_id,
            ActorType::Agent,
            "voice",
            format!("evidence:{}", ev.evidence_id),
            json!({
                "property_id": hw.property_id.to_string(),
                "evidence_ids": [ev.evidence_id.to_string()],
                "reason": "property manager answered electrical question",
            }),
        )
    };
    let cause = app
        .events()
        .emit(tx, emit())
        .await
        .unwrap()
        .expect("new event");
    let rows = app
        .events()
        .by_correlation(tx, cause.correlation_id)
        .await
        .unwrap();
    let chain: Vec<&str> = rows.iter().map(|(_, e)| e.event_type.as_str()).collect();
    assert_eq!(chain[0], "infrastructure.evidence_updated");
    assert!(
        chain.contains(&"property.analysis.completed")
            && chain.contains(&"financial.scenario.updated")
    );
    assert_eq!(*chain.last().unwrap(), "property.ranking.updated");
    let ranking = &rows.last().unwrap().1;
    let scenario = &rows
        .iter()
        .find(|(_, e)| e.event_type == EventType::FinancialScenarioUpdated)
        .unwrap()
        .1;
    // Causation tracked hop by hop.
    assert_eq!(ranking.causation_id, Some(scenario.event_id));
    let versions: BTreeSet<i64> = RANKINGS
        .list(tx, Query::new().eq("project_id", project.project_id))
        .await
        .unwrap()
        .iter()
        .map(|r| r.ranking_version)
        .collect();
    assert_eq!(versions, BTreeSet::from([1, 2]));
    // Old outputs are superseded, not deleted.
    let all = tx
        .select(
            &schema::COST_SCENARIOS,
            &Query::new().eq("property_id", hw.property_id),
        )
        .await
        .unwrap();
    let live = all
        .iter()
        .filter(|r| !r.boolean("superseded").unwrap())
        .count();
    assert_eq!((live, all.len()), (1, 2));
    let live_scenarios: Vec<CostScenario> = COST_SCENARIOS
        .list(
            tx,
            Query::new()
                .eq("property_id", hw.property_id)
                .eq("superseded", false),
        )
        .await
        .unwrap();
    assert_eq!(live_scenarios.len(), 1);

    // Idempotency: re-emitting the same logical event is a no-op; redelivery doesn't re-run
    // the handlers.
    assert!(app.events().emit(tx, emit()).await.unwrap().is_none());
    app.events().dispatch(tx, &cause).await.unwrap();
    let versions: BTreeSet<i64> = RANKINGS
        .list(tx, Query::new().eq("project_id", project.project_id))
        .await
        .unwrap()
        .iter()
        .map(|r| r.ranking_version)
        .collect();
    assert_eq!(versions, BTreeSet::from([1, 2]));
    txb.commit().await.unwrap();
});

both_backends!(conflicting_evidence_coexists, |db| {
    let app = ss_server::build_app(test_settings(&[]), db.store.clone());
    let project = seed_slice(&app).await;
    let mut txb = app.store.begin().await.unwrap();
    let tx: &mut dyn Tx = &mut *txb;
    let alh = PROPERTIES
        .list(tx, Query::new().eq("project_id", project.project_id))
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.property_identity.street_address.starts_with("5380"))
        .unwrap();
    let ev = SourceEvidence {
        evidence_id: Uuid::new_v4(),
        project_id: Some(project.project_id),
        property_id: Some(alh.property_id),
        provider: "owner".into(),
        dataset: None,
        source_record_id: None,
        source_url: None,
        source_asset_reference: None,
        data_origin: DataOrigin::UserProvided,
        retrieved_at: ss_core::utcnow(),
        source_effective_at: None,
        observation_type: "electrical_service_amps".into(),
        extracted_value: json!(400),
        unit: Some("A".into()),
        evidence_classification: EvidenceClassification::OwnerReported,
        match_confidence: None,
        verification_status: ss_core::VerificationStatus::Unverified,
        limitations: vec!["Broker statement".into()],
    };
    EvidenceService.add(tx, &ev).await.unwrap();
    let all = EvidenceService
        .for_property(tx, alh.property_id)
        .await
        .unwrap();
    let conflicts = EvidenceService::conflicts(&all);
    let amps = conflicts
        .get("electrical_service_amps")
        .expect("a conflict");
    let values: BTreeSet<String> = amps
        .iter()
        .map(|e| ss_domain::py::value_str(&e.extracted_value))
        .collect();
    assert!(
        values.contains("800.0") && values.contains("400"),
        "{values:?}"
    );
    txb.rollback().await.unwrap();
});

// ---- live providers (synthetic kit, mocked Matterport and Vexcel) --------------------------------

both_backends!(missing_costar_kit_fails_the_task_like_python, |db| {
    let t = TestApp::new(
        db,
        &[
            ("PROPERTY_PROVIDER_MODE", "live"),
            ("COSTAR_KIT_DIR", "/nonexistent-kit"),
        ],
    );
    let (tok, _) = t.dev_token().await;
    let pid = project_with_demo_profile(&t, &tok).await;
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({}),
        )
        .await;
    // Python, same settings: 202, task FAILED, error_code ProviderError, str(exc) as the message.
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.body["task"]["status"], "FAILED");
    assert_eq!(r.body["task"]["error_code"], "ProviderError");
    assert_eq!(
        r.body["task"]["error_message"],
        "[costar] NOT_CONFIGURED: CoStar data kit not found at /nonexistent-kit (set COSTAR_KIT_DIR; see docs/PROVIDER_STATUS.md)."
    );
    let events = t
        .get(&format!("/api/v1/projects/{pid}/events"), Some(&tok))
        .await
        .body;
    let types: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        ["project.profile.confirmed", "task.started", "task.failed"]
    );
});

fn json_canned(v: Value) -> common::mock_http::Canned {
    common::mock_http::Canned {
        status: 200,
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&v).unwrap(),
    }
}

/// A Matterport GraphQL mock: answers each alias `mN: model(id: "X")` from a table.
fn graphql(rec: &common::mock_http::Recorded) -> common::mock_http::Canned {
    let body: Value = serde_json::from_slice(&rec.body).unwrap_or(Value::Null);
    let q = body["query"].as_str().unwrap_or("");
    let re = regex::Regex::new(r#"(m\d+): model\(id: "([^"]+)"\)"#).unwrap();
    let mut data = serde_json::Map::new();
    for c in re.captures_iter(q) {
        let entry = match &c[2] {
            "MODEL1" => {
                json!({"id": "MODEL1", "name": "Suite 101", "dimensions": {"areaFloor": 105.0}, "floors": [{"id": "f"}]})
            }
            "MODEL2" => {
                json!({"id": "MODEL2", "name": "Lobby", "dimensions": {"areaFloor": 40.0}, "floors": [{"id": "f"}]})
            }
            _ => Value::Null,
        };
        data.insert(c[1].to_owned(), entry);
    }
    json_canned(json!({"data": data}))
}

both_backends!(live_search_enriches_from_matterport_and_vexcel, |db| {
    let mp = common::mock_http::MockServer::start_with(graphql);
    let vx = common::mock_http::MockServer::start_with(|rec| {
        if rec.path.ends_with("/property/extract") {
            json_canned(json!({"features": [{"properties": {
                "structures": [{"primary-structure": true, "footprint.area": 812.4,
                                "roof-objects.ac-unit.count": 6}],
                "metadata.ortho-date-estimate": "2025-12-01T10:00:00",
                "metadata.collection": "us-ca-losangeles-2025"}}]}))
        } else {
            json_canned(json!({"features": []}))
        }
    });
    let kit = repo_root().join("apps/backend/tests-golden/costar_kit_synthetic");
    let kit_s = kit.to_string_lossy().into_owned();
    let graph = format!("{}/graph", mp.base);
    let vbase = format!("{}/v2", vx.base);
    let t = TestApp::new(
        db,
        &[
            ("PROPERTY_PROVIDER_MODE", "live"),
            ("COSTAR_KIT_DIR", kit_s.as_str()),
            ("MATTERPORT_PROVIDER_MODE", "live"),
            ("MATTERPORT_GRAPH_URL", graph.as_str()),
            ("MATTERPORT_API_KEY", "tid"),
            ("MATTERPORT_API_TOKEN", "tsecret"),
            ("VEXCEL_PROVIDER_MODE", "live"),
            ("VEXCEL_BASE_URL", vbase.as_str()),
            ("VEXCEL_API_KEY", "vk"),
            ("PUBLIC_BASE_URL", "http://api.test"),
        ],
    );
    let (tok, _) = t.dev_token().await;
    let pid = t
        .create_project(&tok, &json!({"project_name": "kit", "market": {"city": "Los Angeles", "state": "CA"},
                                      "business_profile": read_json("fixtures/projects/facial_studio.json")["business_profile"]}))
        .await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({"limit": 3}),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    let props = props.as_array().unwrap();
    assert!(!props.is_empty());
    for p in props {
        assert_conforms("PropertyDigitalProfile", p);
        // Kit records keep their label: CoStar data kit, a snapshot, never "live CoStar".
        assert_eq!(p["commercial_listing"]["provider"], "costar");
        assert_eq!(p["commercial_listing"]["data_origin"], "FIXTURE_REAL");
        let manifest: std::collections::HashMap<&str, &str> = p["source_manifest"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["provider"].as_str().unwrap(),
                    m["status"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(manifest["costar"], "SNAPSHOT");
        assert_eq!(manifest["vexcel"], "LIVE");
        // Aerial detections are visual evidence of the building.
        let obs = p["building_observations"].as_array().unwrap();
        assert!(obs
            .iter()
            .any(|o| o["observation_type"] == "rooftop_hvac_units" && o["value"] == json!(6.0)));
        assert!(obs
            .iter()
            .all(|o| o["classification"] == "VISUALLY_OBSERVED"));
        let ortho = p["exterior_assets"].as_array().unwrap().last().unwrap();
        assert_eq!(
            ortho["url"],
            format!(
                "http://api.test/api/v1/media/vexcel/ortho/{}",
                p["property_id"].as_str().unwrap()
            )
        );
        assert_eq!(ortho["captured_at"], "2025-12-01");
    }
    let summary: Vec<String> = props
        .iter()
        .map(|p| {
            format!(
                "{} suite={} assets={}",
                p["commercial_listing"]["provider_listing_id"],
                p["property_identity"]["unit_or_suite"],
                p["interior_assets"]
            )
        })
        .collect();
    // The listing whose suite has a scan comes first, its scan marked; the lobby scan is context.
    let first = &props[0];
    let scans: Vec<&Value> = first["interior_assets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["provider"] == "matterport")
        .collect();
    // With the demo profile's limits the kit offers 2:21 (suite A, no scans) and 1:11 (suite 101,
    // scans MODEL1 and MODEL2); the suite-matched candidate is preferred and stored first.
    assert_eq!(
        first["property_identity"]["unit_or_suite"], "101",
        "{summary:#?}"
    );
    {
        assert_eq!(scans[0]["asset_id"], "MODEL1");
        assert_eq!(scans[0]["matches_listing_unit"], true);
        assert_eq!(scans[0]["has_measured_geometry"], true);
        // The scan's area is not the listing's area.
        assert_ne!(
            scans[0]["area_sqm"],
            first["commercial_listing"]["available_area_sqm"]
        );
    }
    assert!(
        props.iter().any(|p| p["interior_assets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["matches_listing_unit"] == true)),
        "{summary:#?}"
    );
    // Matterport was called with HTTP Basic auth; the key never reaches a response.
    let reqs = mp.requests();
    assert!(!reqs.is_empty());
    assert!(reqs
        .iter()
        .all(|r| r.headers["authorization"] == "Basic dGlkOnRzZWNyZXQ="));
    assert!(vx.requests().iter().all(|r| r.headers["x-api-key"] == "vk"));
    let all = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .text;
    assert!(!all.contains("tsecret") && !all.contains("dGlkOnRzZWNyZXQ"));
});

both_backends!(missing_fixtures_are_reported_not_hidden, |db| {
    let t = TestApp::new(db, &[("FIXTURES_DIR", "/nonexistent-fixtures")]);
    let ready = t.get("/ready", None).await.body;
    let permits = ready["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "la_open_data")
        .unwrap()
        .clone();
    assert_eq!(permits["status"], "BLOCKED", "{permits}");
    let (tok, _) = t.dev_token().await;
    let pid = project_with_demo_profile(&t, &tok).await;
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({}),
        )
        .await;
    // A provider failure is a failed task naming the provider, never an empty result.
    assert_eq!(r.body["task"]["status"], "FAILED", "{}", r.text);
    assert_eq!(r.body["task"]["error_code"], "ProviderError");
    assert!(r.body["task"]["error_message"]
        .as_str()
        .unwrap()
        .starts_with("[costar] NOT_CONFIGURED: fixture listings /nonexistent-fixtures/properties/la_candidates.json"));
    assert_eq!(
        t.get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
            .await
            .body,
        json!([])
    );
});
