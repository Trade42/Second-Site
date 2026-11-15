//! What a simulated contact "said" never changes a real listing's feasibility or costs.
//!
//! The owner's case (browser-flow-3, section 3, item 2): a simulated call for a real listing
//! answers plumbing and HVAC from its script; the answers are stored as `FIXTURE_SYNTHETIC`
//! evidence and `infrastructure.evidence_updated` re-evaluates the property
//! (`OnEvidenceUpdated` -> `PropertyAnalysisWorkflow::evaluate`). With the rule
//! (`ss_domain::feasibility::engine::counts_for`) the assessments, the cost scenario's lines, the
//! ranking and the Nessie plan of a `FIXTURE_REAL` listing are the same before and after, each
//! assessment the answers would have changed says so in one limitation line, and the call's own
//! findings, summary and evidence still show what was "said". The synthetic demo listing moves
//! as it always did. Fixture telephony only: nothing is sent anywhere.
//!
//! Deliberate difference from Python, which counts the evidence whatever its origin.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use ss_core::DataOrigin;
use ss_domain::feasibility::engine::SIMULATED_NOT_COUNTED;
use ss_server::store::repo::PROPERTIES;
use uuid::Uuid;

use common::{demo_project_body, TestApp, TestDb};

/// Removes what is new on every evaluation (ids, timestamps, versions), keeping every figure.
fn stable(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| {
                    !(k.ends_with("_id")
                        || k.ends_with("_ids")
                        || k.ends_with("_at")
                        || k.ends_with("_version")
                        || k.as_str() == "version")
                })
                .map(|(k, v)| (k.clone(), stable(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(stable).collect()),
        other => other.clone(),
    }
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    /// Per requirement: status, observed value, evidence ids, proposed work, and the limitations
    /// without the "not counted" line.
    assessments: Vec<Value>,
    /// The property's live cost scenario without ids: every line and total.
    scenario: Value,
    /// The newest ranking without ids.
    ranking: Value,
    /// The Nessie plan's payments without ids (status and body when there is no plan).
    plan: Value,
    /// The other routes that show a property's decisions (added by the reviewer): the Nessie
    /// affordability answer, the evidence validation's problem list and the property itself.
    other: Vec<Value>,
}

struct Flow<'a> {
    t: &'a TestApp,
    token: String,
    pid: String,
    prop: String,
}

impl Flow<'_> {
    async fn feasibility(&self) -> Vec<Value> {
        let r = self
            .t
            .get(
                &format!(
                    "/api/v1/projects/{}/properties/{}/feasibility",
                    self.pid, self.prop
                ),
                Some(&self.token),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        let mut list = r.body.as_array().cloned().unwrap();
        list.sort_by_key(|a| a["requirement_id"].as_str().unwrap().to_owned());
        list
    }

    async fn snapshot(&self) -> Snapshot {
        let assessments = self
            .feasibility()
            .await
            .iter()
            .map(|a| {
                let limits: Vec<&Value> = a["limitations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|l| l.as_str() != Some(SIMULATED_NOT_COUNTED))
                    .collect();
                json!({
                    "requirement_id": a["requirement_id"], "status": a["status"],
                    "observed_value": a["observed_value"], "evidence_ids": a["evidence_ids"],
                    "proposed_work": a["proposed_work"], "limitations": limits,
                    "uncertainty": a["uncertainty"],
                })
            })
            .collect();
        let r = self
            .t
            .get(
                &format!("/api/v1/projects/{}/cost-scenarios", self.pid),
                Some(&self.token),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        let mine: Vec<&Value> = r
            .body
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["property_id"] == json!(self.prop))
            .collect();
        assert_eq!(mine.len(), 1, "one live scenario for the property");
        let scenario = stable(mine[0]);
        let r = self
            .t
            .get(
                &format!("/api/v1/projects/{}/rankings", self.pid),
                Some(&self.token),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        let ranking = stable(r.body.as_array().unwrap().last().unwrap());
        let r = self
            .t
            .get(
                &format!(
                    "/api/v1/projects/{}/properties/{}/nessie/plan",
                    self.pid, self.prop
                ),
                Some(&self.token),
            )
            .await;
        let plan = if r.status == StatusCode::OK {
            assert!(r.body["plan"]["payments"].is_array(), "{}", r.text);
            stable(&r.body["plan"])
        } else {
            json!({"status": r.status.as_u16(), "body": stable(&r.body)})
        };
        let mut other = Vec::new();
        for path in ["/nessie/affordability", "/evidence/validation", ""] {
            let r = self
                .t
                .get(
                    &format!(
                        "/api/v1/projects/{}/properties/{}{path}",
                        self.pid, self.prop
                    ),
                    Some(&self.token),
                )
                .await;
            let mut body = stable(&r.body);
            if let Some(list) = body
                .get_mut("infrastructure_assessments")
                .and_then(Value::as_array_mut)
            {
                for a in list {
                    if let Some(l) = a.get_mut("limitations").and_then(Value::as_array_mut) {
                        l.retain(|x| x.as_str() != Some(SIMULATED_NOT_COUNTED));
                    }
                }
            }
            other.push(json!({"path": path, "status": r.status.as_u16(), "body": body}));
        }
        Snapshot {
            assessments,
            scenario,
            ranking,
            plan,
            other,
        }
    }

    /// Outreach, approval, a simulated call (fixture voice), the script to its end. Returns the
    /// finished call as the simulate route gives it.
    async fn simulated_call(&self) -> Value {
        let (t, pid, token) = (self.t, &self.pid, &self.token);
        let body = format!(
            r#"{{"property_id": "{}", "channel": "VOICE", "recipient": {{"role": "broker", "name": "Dana Broker", "phone": "+15555550100"}}, "purpose": "Verify electrical"}}"#,
            self.prop
        );
        let out = t
            .send(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/v1/projects/{pid}/outreach"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
        assert_eq!(out.status, StatusCode::ACCEPTED, "{}", out.text);
        let appr = &out.body["approval"];
        let ok = t
            .post(
                &format!(
                    "/api/v1/approvals/{}/decision",
                    appr["approval_id"].as_str().unwrap()
                ),
                Some(token),
                &json!({"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]}),
            )
            .await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        let call = t
            .post(
                &format!("/api/v1/projects/{pid}/calls"),
                Some(token),
                &json!({"communication_id": out.body["communication"]["communication_id"]}),
            )
            .await;
        assert_eq!(call.status, StatusCode::ACCEPTED, "{}", call.text);
        assert_eq!(call.body["simulated"], true);
        let done = t
            .post(
                &format!(
                    "/api/v1/projects/{pid}/calls/{}/simulate",
                    call.body["call_id"].as_str().unwrap()
                ),
                Some(token),
                &json!({"owner_action": "accept"}),
            )
            .await;
        assert_eq!(done.status, StatusCode::OK, "{}", done.text);
        assert_eq!(done.body["state"], "COMPLETED");
        done.body
    }
}

/// The whole flow for one listing origin. Returns (before, after, the two "not counted"
/// requirement ids after the call).
async fn flow(db: &TestDb, origin: DataOrigin) -> (Snapshot, Snapshot, Vec<String>) {
    let t = TestApp::new(db, &[]);
    let (token, _user) = t.dev_token().await;
    let p = t.create_project(&token, &demo_project_body()).await;
    let pid = p["project"]["project_id"].as_str().unwrap().to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&token),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&token))
        .await;
    let prop = props.body[0]["property_id"].as_str().unwrap().to_owned();
    assert_eq!(
        props.body[0]["commercial_listing"]["data_origin"],
        "FIXTURE_SYNTHETIC"
    );
    let f = Flow {
        t: &t,
        token,
        pid,
        prop,
    };
    if origin != DataOrigin::FixtureSynthetic {
        // The same listing as a genuine record (what the data kit's listings are).
        let id: Uuid = f.prop.parse().unwrap();
        let mut tx = t.app.store.begin().await.unwrap();
        let mut stored = PROPERTIES.require(&mut *tx, id).await.unwrap();
        stored.commercial_listing.as_mut().unwrap().data_origin = origin;
        PROPERTIES.upsert(&mut *tx, &stored).await.unwrap();
        tx.commit().await.unwrap();
        let r = t
            .post(
                &format!("/api/v1/projects/{}/properties/{}/analyze", f.pid, f.prop),
                Some(&f.token),
                &json!({}),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    }
    let before = f.snapshot().await;
    assert!(
        f.feasibility().await.iter().all(|a| !a["limitations"]
            .as_array()
            .unwrap()
            .contains(&json!(SIMULATED_NOT_COUNTED))),
        "no simulated answer exists before the call"
    );

    let call = f.simulated_call().await;
    // The call itself still shows what the simulated contact "said".
    let findings = call["findings"].as_array().unwrap();
    let claim = |c: &str| findings.iter().find(|x| x["claim_type"] == c);
    assert_eq!(
        claim("in_unit_plumbing").map(|x| &x["value"]),
        Some(&json!(true)),
        "{findings:?}"
    );
    assert_eq!(
        claim("hvac_zonable").map(|x| &x["value"]),
        Some(&json!(true)),
        "{findings:?}"
    );
    let summary = call["outcome_summary"].as_str().unwrap_or_default();
    assert!(!summary.is_empty(), "{call}");
    // And the evidence list still holds both answers, labelled as what they are.
    let ev = t
        .get(
            &format!("/api/v1/projects/{}/properties/{}/evidence", f.pid, f.prop),
            Some(&f.token),
        )
        .await;
    assert_eq!(ev.status, StatusCode::OK, "{}", ev.text);
    for c in ["in_unit_plumbing", "hvac_zonable"] {
        let e = ev
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["observation_type"] == c && e["provider"] == "broker_call")
            .unwrap_or_else(|| panic!("{c} evidence listed: {}", ev.text));
        assert_eq!(e["data_origin"], "FIXTURE_SYNTHETIC");
        assert_eq!(
            e["limitations"][0],
            "SIMULATED CALL: scripted demo answer, not a real conversation."
        );
    }

    let after = f.snapshot().await;
    let said: Vec<String> = f
        .feasibility()
        .await
        .iter()
        .filter(|a| {
            a["limitations"]
                .as_array()
                .unwrap()
                .contains(&json!(SIMULATED_NOT_COUNTED))
        })
        .map(|a| a["requirement_id"].as_str().unwrap().to_owned())
        .collect();
    (before, after, said)
}

/// The figures the report quotes: the scenario's opening cost and the plan's cash at move-in.
fn figures(s: &Snapshot) -> String {
    format!(
        "opening cost {} / {} / {} minor; plan cash at move-in {} expected, {} high",
        s.scenario["opening_cost_low"],
        s.scenario["opening_cost_expected"],
        s.scenario["opening_cost_high"],
        s.plan["move_in_cash_expected_minor"],
        s.plan["move_in_cash_high_minor"]
    )
}

both_backends!(a_real_listing_is_not_moved_by_a_simulated_call, |db| {
    let (before, after, said) = flow(db, DataOrigin::FixtureReal).await;
    println!("FIXTURE_REAL before: {}", figures(&before));
    println!("FIXTURE_REAL after:  {}", figures(&after));
    assert_eq!(before.assessments, after.assessments);
    assert_eq!(before.scenario, after.scenario);
    assert_eq!(before.ranking, after.ranking);
    assert_eq!(before.plan, after.plan);
    assert_eq!(before.other, after.other);
    println!("REVIEW other: {}", json!(before.other));
    assert!(before.plan["payments"].is_array(), "{}", before.plan);
    // The script answers the panel, the plumbing and the HVAC: the three rules that read those
    // answers say that a simulated answer exists and was not counted; no other assessment does.
    assert_eq!(
        said,
        [
            "facial_studio.electrical.tenant_service",
            "facial_studio.hvac.ventilation",
            "facial_studio.plumbing.treatment_sinks"
        ]
    );
});

both_backends!(the_synthetic_listing_moves_as_before, |db| {
    let (before, after, said) = flow(db, DataOrigin::FixtureSynthetic).await;
    println!("FIXTURE_SYNTHETIC before: {}", figures(&before));
    println!("FIXTURE_SYNTHETIC after:  {}", figures(&after));
    assert_ne!(before.assessments, after.assessments);
    assert_ne!(before.scenario, after.scenario);
    assert_ne!(before.plan, after.plan);
    assert!(
        after.plan["move_in_cash_expected_minor"].as_i64()
            < before.plan["move_in_cash_expected_minor"].as_i64()
    );
    assert!(said.is_empty(), "{said:?}");
});
