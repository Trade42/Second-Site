//! Server part 4: outreach, communications, calls, the call simulator and owner handoffs.
//!
//! Ports of `apps/api/tests/test_comms_flow.py` and `test_approvals_comms.py` (the parts that do
//! not need Photon's own code path), plus approval enforcement at the action boundary, concurrent
//! call starts, and isolation with a second owner on every route. Every test runs on the
//! in-memory store and, with `TEST_DATABASE_URL`, on PostgreSQL. Where Python checks that the
//! owner was texted, these tests check the request the owner notifier received (the Python
//! service texts the owner; see `services::notifier`).

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{Duration, Utc};
use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_core::UtcDateTime;
use ss_domain::contracts::{
    CallSession, CallState, HandoffReason, HandoffStatus, OwnerChannel, OwnerChannelStatus,
};
use ss_domain::integrations::telnyx::{client_state, encode_client_state, TelephonyCommand};
use ss_server::contracts::{ActionType, ApprovalStatus};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::call_orchestration::{CallOrchestrator, StartCall};
use ss_server::services::call_simulator::{event, CallSimulator};
use ss_server::services::notifier::{
    NotConfiguredNotifier, OwnerNotify, OwnerNotifier, RecordingNotifier,
};
use ss_server::store::comms::{CALLS, COMMUNICATIONS, HANDOFFS, OWNER_CHANNELS};
use ss_server::store::repo::{APPROVALS, EVIDENCE};
use ss_server::store::Query;
use tower::ServiceExt;
use uuid::Uuid;

use common::{assert_conforms, assert_conforms_openapi, demo_project_body, TestApp, TestDb};

const OWNER_PHONE: &str = "+15555550142";

struct Ctx {
    t: TestApp,
    fake: Arc<RecordingNotifier>,
    token: String,
    user: Uuid,
}

impl Ctx {
    async fn new(db: &TestDb) -> Ctx {
        let t = TestApp::new(db, &[]);
        let fake = Arc::new(RecordingNotifier::new());
        t.app
            .extensions
            .insert(Arc::new(OwnerNotify::new(fake.clone())));
        let (token, user) = t.dev_token().await;
        Ctx {
            t,
            fake,
            token,
            user,
        }
    }

    fn fixture(&self) -> Arc<ss_domain::integrations::telnyx::FixtureTelephonyProvider> {
        self.t.app.voice().fixture.clone().expect("fixture telephony")
    }

    async fn idle(&self) {
        self.t.app.owner_notify().idle().await;
    }

    fn texts(&self) -> Vec<String> {
        self.fake.requests().into_iter().map(|r| r.text).collect()
    }

    async fn post(&self, path: &str, body: &Value) -> common::Resp {
        self.t.post(path, Some(&self.token), body).await
    }

    async fn get(&self, path: &str) -> common::Resp {
        self.t.get(path, Some(&self.token)).await
    }

    /// `_project`: create the demo project and run the LA search.
    async fn project(&self) -> Uuid {
        let p = self.t.create_project(&self.token, &demo_project_body()).await;
        let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
        let r = self
            .post(
                &format!("/api/v1/projects/{pid}/properties/search"),
                &json!({"city": "Los Angeles", "state": "CA"}),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
        pid
    }

    async fn hollywood(&self, pid: Uuid) -> Value {
        let props = self.get(&format!("/api/v1/projects/{pid}/properties")).await;
        props
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|p| {
                p["property_identity"]["normalized_address"]
                    .as_str()
                    .unwrap_or("")
                    .contains("HOLLYWOOD")
            })
            .cloned()
            .expect("the Hollywood property")
    }

    async fn outreach(&self, pid: Uuid, property_id: &str, channel: &str) -> Value {
        let r = self
            .post(
                &format!("/api/v1/projects/{pid}/outreach"),
                &json!({
                    "property_id": property_id,
                    "channel": channel,
                    "recipient": {"role": "broker", "name": "Dana Broker", "phone": "+15555550100"},
                    "purpose": "Verify electrical, plumbing and HVAC",
                }),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
        r.body
    }

    async fn approve(&self, appr: &Value) -> common::Resp {
        self.post(
            &format!("/api/v1/approvals/{}/decision", appr["approval_id"].as_str().unwrap()),
            &json!({"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]}),
        )
        .await
    }

    async fn start(&self, pid: Uuid, out: &Value) -> common::Resp {
        self.post(
            &format!("/api/v1/projects/{pid}/calls"),
            &json!({"communication_id": out["communication"]["communication_id"]}),
        )
        .await
    }

    /// `_approved_simulated_call`.
    async fn approved_call(&self, pid: Uuid) -> (Value, Value, Value) {
        let prop = self.hollywood(pid).await;
        let out = self
            .outreach(pid, prop["property_id"].as_str().unwrap(), "VOICE")
            .await;
        let ok = self.approve(&out["approval"]).await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        let call = self.start(pid, &out).await;
        assert_eq!(call.status, StatusCode::ACCEPTED, "{}", call.text);
        assert_eq!(call.body["simulated"], true);
        assert_eq!(call.body["state"], "DIALING");
        (prop, out, call.body)
    }
}

/// What Python's Photon code writes when the owner texts the agent first (Rust only reads it).
async fn link_owner(t: &TestApp, owner: Uuid, phone: &str) {
    let mut tx = t.app.store.begin().await.unwrap();
    OWNER_CHANNELS
        .add(
            &mut *tx,
            &OwnerChannel {
                owner_channel_id: Uuid::new_v4(),
                owner_id: owner,
                platform: "imessage".into(),
                handle: phone.into(),
                status: OwnerChannelStatus::Linked,
                space_id: Some("spc_fixture_owner".into()),
                agent_user_id: Some("usr_fixture_agent".into()),
                last_inbound_message_id: None,
                linked_at: Some(UtcDateTime::now()),
                updated_at: UtcDateTime::now(),
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn call_of(t: &TestApp, id: &Value) -> CallSession {
    let id: Uuid = id.as_str().unwrap().parse().unwrap();
    let mut tx = t.app.store.begin().await.unwrap();
    let c = CALLS.require(&mut *tx, id).await.unwrap();
    tx.commit().await.unwrap();
    c
}

fn suffix_status(prop: &Value) -> IndexMap<String, Value> {
    prop["infrastructure_assessments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            let rid = a["requirement_id"].as_str().unwrap();
            (
                rid.split_once('.').map_or(rid, |(_, s)| s).to_owned(),
                a.clone(),
            )
        })
        .collect()
}

both_backends!(simulated_call_end_to_end_with_owner_join, |db| {
    let c = Ctx::new(db).await;
    let pid = c.project().await;
    link_owner(&c.t, c.user, OWNER_PHONE).await;
    let (prop, out, call) = c.approved_call(pid).await;
    assert_conforms("CallSession", &call);
    c.idle().await;
    // A link to the app, never "reply YES": approvals are not granted by text.
    assert!(c.texts().iter().any(|t| t.contains("approve in the app")));
    let claims: Vec<&str> = call["script"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["claim_type"].as_str().unwrap())
        .collect();
    assert!(claims.contains(&"tenant_subpanel_amps"));
    assert_eq!(claims.last(), Some(&"available_from"));
    let pid_s = prop["property_id"].as_str().unwrap();
    let before = c.get(&format!("/api/v1/projects/{pid}/properties/{pid_s}")).await;
    assert_eq!(
        suffix_status(&before.body)["electrical.tenant_service"]["status"],
        "UNKNOWN"
    );

    let r = c
        .post(
            &format!(
                "/api/v1/projects/{pid}/calls/{}/simulate",
                call["call_id"].as_str().unwrap()
            ),
            &json!({"owner_action": "accept"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let done = &r.body;
    assert_conforms("CallSession", done);
    assert_eq!(done["state"], "COMPLETED");
    assert_eq!(done["simulated"], true);
    let transcript = done["transcript"].as_array().unwrap();
    let speakers: Vec<&str> = transcript
        .iter()
        .map(|t| t["speaker"].as_str().unwrap())
        .collect();
    assert_eq!(speakers[0], "assistant");
    assert!(speakers.contains(&"owner"));
    assert!(transcript[0]["text"]
        .as_str()
        .unwrap()
        .contains("automated assistant")); // disclosure on answer
    assert!(transcript.iter().any(|t| t["speaker"] == "assistant"
        && t["text"].as_str().unwrap().contains("can't agree to anything")));
    assert!(transcript
        .iter()
        .any(|t| t["text"] == "[owner joined the call]"));
    let amps = done["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["claim_type"] == "tenant_subpanel_amps")
        .unwrap();
    assert_eq!(amps["value"], 200);
    assert_eq!(amps["unit"], "A");
    // Carrier commands (fixture): speak + transcription + hangup, and a conference with the owner.
    let cmds: Vec<&str> = c.fixture().commands().iter().map(|x| x.cmd()).collect();
    assert!(cmds.contains(&"transcription_start"));
    assert_eq!(cmds.last(), Some(&"hangup"));
    assert!(!c.fixture().conferences().is_empty());

    let after = c.get(&format!("/api/v1/projects/{pid}/properties/{pid_s}")).await;
    let by = suffix_status(&after.body);
    let elec = &by["electrical.tenant_service"];
    assert_eq!(elec["status"], "MEETS"); // 200 A reported >= 100 A required
    assert!(elec["observed_value"]
        .as_str()
        .unwrap()
        .contains("broker-reported"));
    assert!(elec["limitations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().contains("SIMULATED CALL")));
    let mut tx = c.t.app.store.begin().await.unwrap();
    let ev: Vec<_> = EVIDENCE
        .list(&mut *tx, Query::new().eq("property_id", pid_s.parse::<Uuid>().unwrap()))
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.provider == "broker_call")
        .collect();
    tx.commit().await.unwrap();
    assert!(!ev.is_empty());
    assert!(ev
        .iter()
        .all(|e| e.data_origin.as_str() == "FIXTURE_SYNTHETIC"
            && e.evidence_classification.as_str() == "OWNER_REPORTED"
            && e.limitations.iter().any(|l| l.contains("not independently verified"))));

    let comms = c.get(&format!("/api/v1/projects/{pid}/communications")).await;
    let outreach = comms
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["communication_id"] == out["communication"]["communication_id"])
        .unwrap();
    assert_eq!(outreach["state"], "COMPLETED");
    assert!(!outreach["structured_findings"].as_array().unwrap().is_empty());
    c.idle().await;
    let texts = c.texts();
    let summary = texts.last().unwrap();
    assert!(summary.starts_with("[Simulated call]"), "{summary}");
    assert!(summary.contains("tenant_subpanel_amps: 200 A"), "{summary}");
    let purposes: Vec<String> = c.fake.requests().into_iter().map(|r| r.purpose).collect();
    assert_eq!(
        purposes,
        ["approval_request", "handoff_request", "call_summary"]
    );
    let events = c
        .get(&format!("/api/v1/projects/{pid}/events?limit=500"))
        .await;
    let types: Vec<&str> = events.body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    for t in [
        "call.finding_recorded",
        "call.owner_handoff_requested",
        "call.owner_joined",
        "outreach.completed",
        "infrastructure.evidence_updated",
        "property.ranking.updated",
    ] {
        assert!(types.contains(&t), "{t}");
    }
    // Python's notify_owner writes communication.owner_notified; with the Python service
    // answering, Rust writes none itself.
    assert!(!types.contains(&"communication.owner_notified"));
});

both_backends!(owner_replies_later, |db| {
    let c = Ctx::new(db).await;
    let pid = c.project().await;
    link_owner(&c.t, c.user, OWNER_PHONE).await;
    let (_, _, call) = c.approved_call(pid).await;
    let id: Uuid = call["call_id"].as_str().unwrap().parse().unwrap();
    let mut tx = c.t.app.store.begin().await.unwrap();
    let mut sim = CallSimulator::new(&c.t.app).unwrap();
    let cs = CALLS.require(&mut *tx, id).await.unwrap();
    let cs = sim
        .leg_events(
            &mut *tx,
            cs,
            vec![
                ("call.answered", Default::default()),
                ("call.speak.ended", Default::default()),
            ],
        )
        .await
        .unwrap();
    let cs = sim
        .say_broker(
            &mut *tx,
            cs,
            "Before anything, can you commit to the deposit by Friday?",
        )
        .await
        .unwrap();
    assert_eq!(cs.state, CallState::HandoffPending);
    tx.commit().await.unwrap();
    c.t.app
        .owner_notify()
        .dispatch(c.t.app.clone(), sim.orch.take_notices());
    c.idle().await;
    let last = c.texts().last().cloned().unwrap();
    assert!(last.contains("Reply JOIN"), "{last}");
    assert!(c.texts().iter().all(|t| !t.contains("ref_"))); // no token material in texts
    // The owner answers LATER (Python: by text through Photon; here through the web route).
    let r = c
        .post(
            &format!(
                "/api/v1/projects/{pid}/handoffs/{}/response",
                cs.handoff_request_id.unwrap()
            ),
            &json!({"response": "DEFERRED"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let after = call_of(&c.t, &call["call_id"]).await;
    assert_eq!(after.state, CallState::AiResumed);
    assert_eq!(
        after.deferred_questions,
        ["Before anything, can you commit to the deposit by Friday?"]
    );
    assert!(after
        .transcript
        .iter()
        .any(|t| t.speaker == "assistant" && t.text.contains("follow up with you directly")));
    let handoffs = c.get(&format!("/api/v1/projects/{pid}/handoffs")).await;
    assert_eq!(handoffs.body.as_array().unwrap().len(), 1);
    assert_eq!(handoffs.body[0]["status"], "DEFERRED");
    assert_conforms("OwnerHandoffRequest", &handoffs.body[0]);
    // A second answer is refused.
    let again = c
        .post(
            &format!(
                "/api/v1/projects/{pid}/handoffs/{}/response",
                cs.handoff_request_id.unwrap()
            ),
            &json!({"response": "ACCEPTED"}),
        )
        .await;
    assert_eq!(again.status.as_u16(), 409, "{}", again.text);
    assert_eq!(again.body["error_code"], "INVALID_STATE_TRANSITION");
});

both_backends!(unanswered_and_voicemail_calls, |db| {
    let c = Ctx::new(db).await;
    let pid = c.project().await;
    let (_, out, call) = c.approved_call(pid).await;
    let id: Uuid = call["call_id"].as_str().unwrap().parse().unwrap();
    let mut tx = c.t.app.store.begin().await.unwrap();
    let mut sim = CallSimulator::new(&c.t.app).unwrap();
    let cs = CALLS.require(&mut *tx, id).await.unwrap();
    let mut machine = serde_json::Map::new();
    machine.insert("result".into(), json!("machine"));
    sim.leg_events(
        &mut *tx,
        cs.clone(),
        vec![("call.machine.detection.ended", machine)],
    )
    .await
    .unwrap();
    let cmds = c.fixture().commands();
    match &cmds[cmds.len() - 2] {
        TelephonyCommand::Speak { text, .. } => assert!(text.contains("follow up by email")),
        other => panic!("{other:?}"),
    }
    assert_eq!(cmds.last().unwrap().cmd(), "hangup");
    let mut hang = serde_json::Map::new();
    hang.insert("hangup_cause".into(), json!("timeout"));
    let cs = sim
        .leg_events(&mut *tx, cs, vec![("call.hangup", hang)])
        .await
        .unwrap();
    assert_eq!(cs.state, CallState::Failed);
    assert!(cs.failure_reason.as_deref().unwrap().contains("not answered"));
    let mut orch = CallOrchestrator::new(&c.t.app);
    let none = orch
        .handle_event(
            &mut *tx,
            &json!({"data": {"event_type": "call.answered", "payload": {"call_control_id": "nope"}}}),
        )
        .await
        .unwrap();
    assert!(none.is_none());
    tx.commit().await.unwrap();
    let comms = c.get(&format!("/api/v1/projects/{pid}/communications")).await;
    let comm = comms
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["communication_id"] == out["communication"]["communication_id"])
        .unwrap()
        .clone();
    assert_eq!(comm["state"], "FAILED");
});

both_backends!(approval_lifecycle_and_enforcement, |db| {
    let c = Ctx::new(db).await;
    let pid = c.project().await;
    let prop = c.hollywood(pid).await;
    let out = c
        .outreach(pid, prop["property_id"].as_str().unwrap(), "VOICE")
        .await;
    assert_conforms_openapi("OutreachAccepted", &out);
    let appr = &out["approval"];
    assert_eq!(appr["approval_status"], "PENDING");
    assert_eq!(appr["action_type"], "PLACE_CALL");
    assert_eq!(out["communication"]["state"], "AWAITING_APPROVAL");
    // Executing before approval is refused at the action boundary.
    let r = c.start(pid, &out).await;
    assert_eq!(r.status.as_u16(), 409, "{}", r.text);
    assert_eq!(r.body["error_code"], "APPROVAL_REQUIRED");
    // Hash mismatch (the owner saw a different payload) is refused.
    let r = c
        .post(
            &format!("/api/v1/approvals/{}/decision", appr["approval_id"].as_str().unwrap()),
            &json!({"decision": "APPROVED", "action_payload_hash": "0".repeat(64)}),
        )
        .await;
    assert_eq!(r.body["error_code"], "APPROVAL_PAYLOAD_MISMATCH");
    let ok = c.approve(appr).await;
    assert_eq!(ok.body["approval_status"], "APPROVED");
    let again = c
        .post(
            &format!("/api/v1/approvals/{}/decision", appr["approval_id"].as_str().unwrap()),
            &json!({"decision": "REJECTED", "action_payload_hash": appr["action_payload_hash"]}),
        )
        .await;
    assert_eq!(again.status.as_u16(), 409);
    assert_eq!(again.body["error_code"], "INVALID_STATE_TRANSITION");
    let call = c.start(pid, &out).await;
    assert_eq!(call.status, StatusCode::ACCEPTED, "{}", call.text);
    assert_eq!(call.body["state"], "DIALING");
    assert!(call.body["started_at"].is_string());
    assert!(call.body["ended_at"].is_null()); // accepted by the carrier, not answered
    // Single use: EXECUTED now, a second call is refused and nothing more is dialed.
    let r = c.start(pid, &out).await;
    assert_eq!(r.status.as_u16(), 409, "{}", r.text);
    assert_eq!(r.body["error_code"], "APPROVAL_REQUIRED");
    assert_eq!(c.fixture().calls().len(), 1);
    let mut tx = c.t.app.store.begin().await.unwrap();
    let stored = APPROVALS
        .require(&mut *tx, appr["approval_id"].as_str().unwrap().parse().unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stored.approval_status, ApprovalStatus::Executed);
    // The communication is in progress; the call list shows the call.
    let comms = c.get(&format!("/api/v1/projects/{pid}/communications")).await;
    assert_eq!(comms.body[0]["state"], "IN_PROGRESS");
    let calls = c.get(&format!("/api/v1/projects/{pid}/calls")).await;
    assert_eq!(calls.body.as_array().unwrap().len(), 1);
    assert_conforms("CallSession", &calls.body[0]);
});

both_backends!(outreach_records_and_notifies, |db| {
    let c = Ctx::new(db).await;
    let pid = c.project().await;
    let prop = c.hollywood(pid).await;
    let prop_id = prop["property_id"].as_str().unwrap();
    let out = c.outreach(pid, prop_id, "VOICE").await;
    let appr = &out["approval"];
    // The approved payload is exactly the call payload.
    assert_eq!(
        appr["action_payload"],
        json!({
            "project_id": pid.to_string(),
            "property_id": prop_id,
            "recipient": {"role": "broker", "name": "Dana Broker", "phone": "+15555550100"},
        })
    );
    assert_eq!(
        appr["summary"],
        "VOICE to Dana Broker: Verify electrical, plumbing and HVAC"
    );
    assert_eq!(
        appr["idempotency_key"],
        format!(
            "outreach:{pid}:{prop_id}:{{'role': 'broker', 'name': 'Dana Broker', 'phone': '+15555550100'}}"
        )
    );
    assert_eq!(appr["requested_by_agent"], "outreach");
    // A repeat returns the same approval and a new draft.
    let again = c.outreach(pid, prop_id, "VOICE").await;
    assert_eq!(again["approval"]["approval_id"], appr["approval_id"]);
    assert_ne!(
        again["communication"]["communication_id"],
        out["communication"]["communication_id"]
    );
    c.idle().await;
    let reqs = c.fake.requests();
    assert_eq!(reqs.len(), 2);
    for r in &reqs {
        assert_eq!(r.purpose, "approval_request");
        assert_eq!(
            r.idempotency_key,
            format!("approval-notify:{}", appr["approval_id"].as_str().unwrap())
        );
        assert_eq!(r.owner_id, c.user);
        assert_eq!(r.project_id, pid);
        assert_eq!(
            r.text,
            "Your assistant wants to call Dana Broker about a property (Verify electrical, \
             plumbing and HVAC). Review and approve in the app: http://localhost:3000/activity"
        );
    }
    let events = c
        .get(&format!("/api/v1/projects/{pid}/events?limit=500"))
        .await;
    let ev: Vec<&Value> = events.body["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(
                e["event_type"].as_str(),
                Some("approval.requested" | "outreach.approval_requested")
            )
        })
        .collect();
    assert_eq!(ev.len(), 3);
    assert_eq!(ev[0]["event_type"], "approval.requested");
    assert_eq!(ev[1]["event_type"], "outreach.approval_requested");
    assert_eq!(ev[1]["actor_id"], "outreach");
    assert_eq!(
        ev[1]["idempotency_key"],
        format!(
            "outreach:{}:requested",
            out["communication"]["communication_id"].as_str().unwrap()
        )
    );
    assert_eq!(
        ev[1]["payload"],
        json!({
            "communication_id": out["communication"]["communication_id"],
            "property_id": prop_id,
            "approval_id": appr["approval_id"],
        })
    );
    // Nothing was dialed.
    assert!(c.fixture().calls().is_empty());
    // Python accepted any property id; here an unknown one is 404 and nothing is stored.
    let r = c
        .post(
            &format!("/api/v1/projects/{pid}/outreach"),
            &json!({"property_id": Uuid::new_v4(), "channel": "VOICE",
                    "recipient": {"name": "X", "phone": "+15555550100"}, "purpose": "p"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 404, "{}", r.text);
    assert_eq!(r.body["message"], "property not found");
    // Body checks.
    let r = c
        .post(
            &format!("/api/v1/projects/{pid}/outreach"),
            &json!({"property_id": prop_id, "channel": "PIGEON",
                    "recipient": {}, "purpose": "p"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.text);
    let r = c
        .t
        .send(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/projects/{pid}/outreach"))
                .header("content-type", "application/json")
                .body(Body::from("{bad"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status.as_u16(), 422, "invalid JSON before authentication");
    let comms = c.get(&format!("/api/v1/projects/{pid}/communications")).await;
    assert_eq!(comms.body.as_array().unwrap().len(), 2);
});

both_backends!(non_voice_outreach_cannot_place_a_call, |db| {
    let c = Ctx::new(db).await;
    let pid = c.project().await;
    let prop = c.hollywood(pid).await;
    let out = c
        .outreach(pid, prop["property_id"].as_str().unwrap(), "EMAIL")
        .await;
    assert_eq!(out["approval"]["action_type"], "SEND_MESSAGE");
    assert_eq!(c.approve(&out["approval"]).await.status, StatusCode::OK);
    let r = c.start(pid, &out).await;
    assert_eq!(r.status.as_u16(), 409, "{}", r.text);
    assert_eq!(r.body["error_code"], "APPROVAL_REQUIRED");
    assert!(c.fixture().calls().is_empty());
});

/// An owner, a project and an APPROVED PLACE_CALL approval for a call without property
/// (`_approved_call` of the Python tests).
async fn approved_call(
    t: &TestApp,
    phone: Option<&str>,
) -> (Uuid, Uuid, IndexMap<String, String>, Uuid) {
    let owner = Uuid::new_v4();
    let token = t.token_for(owner);
    let p = t
        .create_project(
            &token,
            &json!({"project_name": "t", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
    if let Some(phone) = phone {
        link_owner(t, owner, phone).await;
    }
    let mut recipient = IndexMap::new();
    recipient.insert("role".to_owned(), "broker".to_owned());
    recipient.insert("name".to_owned(), "B".to_owned());
    recipient.insert("phone".to_owned(), "+15555550100".to_owned());
    let approval_id = approve_payload(t, pid, owner, &recipient, None, ActionType::PlaceCall).await;
    (owner, pid, recipient, approval_id)
}

async fn approve_payload(
    t: &TestApp,
    pid: Uuid,
    owner: Uuid,
    recipient: &IndexMap<String, String>,
    property_id: Option<Uuid>,
    action: ActionType,
) -> Uuid {
    let payload = ss_domain::communications::call_payload(pid, recipient, property_id);
    let mut tx = t.app.store.begin().await.unwrap();
    let a = t
        .app
        .approvals()
        .request(
            &mut *tx,
            ApprovalSpec::new(
                pid,
                owner,
                "t",
                action,
                payload,
                "call",
                format!("c-{}", Uuid::new_v4()),
            ),
        )
        .await
        .unwrap();
    t.app
        .approvals()
        .decide(
            &mut *tx,
            a.approval_id,
            owner,
            ApprovalStatus::Approved,
            &a.action_payload_hash,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    a.approval_id
}

fn start_args(
    pid: Uuid,
    owner: Uuid,
    recipient: &IndexMap<String, String>,
    approval_id: Option<Uuid>,
    intro: Option<&str>,
) -> StartCall {
    StartCall {
        project_id: pid,
        owner_id: owner,
        recipient: recipient.clone(),
        property_id: None,
        approval_id,
        intro_recording_reference: intro.map(str::to_owned),
        communication_id: None,
        simulated: false,
    }
}

both_backends!(call_handoff_accept_decline_and_timeout, |db| {
    let t = TestApp::new(db, &[]);
    let fixture = t.app.voice().fixture.clone().unwrap();
    for response in [
        Some(HandoffStatus::Accepted),
        Some(HandoffStatus::Declined),
        None,
    ] {
        let phone = format!("+1555555{:04}", Uuid::new_v4().as_u128() % 10000);
        let (owner, pid, recipient, a) = approved_call(&t, Some(&phone)).await;
        let mut tx = t.app.store.begin().await.unwrap();
        let mut orch = CallOrchestrator::new(&t.app);
        let call = orch
            .start(
                &mut *tx,
                start_args(pid, owner, &recipient, Some(a), Some("obj/owner-intro.m4a")),
            )
            .await
            .unwrap();
        assert_eq!(call.state, CallState::Dialing);
        assert!(call
            .provider_call_id
            .as_deref()
            .unwrap()
            .starts_with("fixture-call-"));
        orch.advance(&mut *tx, call.call_id, CallState::Intro, None)
            .await
            .unwrap();
        orch.advance(&mut *tx, call.call_id, CallState::Active, None)
            .await
            .unwrap();
        let h = orch
            .request_handoff(
                &mut *tx,
                call.call_id,
                HandoffReason::FinancialDetails,
                "Can you do a 2-month deposit?",
            )
            .await
            .unwrap();
        assert_eq!(h.status, HandoffStatus::Notified);
        let reference = h.join_token_reference.clone().unwrap();
        assert!(reference.starts_with("ref_"));
        let texts: Vec<String> = orch.notices.iter().map(|n| n.text.clone()).collect();
        assert!(texts.iter().any(|x| x.contains("needs you on a call")));
        assert!(texts.iter().all(|x| !x.contains("ref_"))); // no token material in texts
        let final_call = match response {
            None => {
                // The owner never responds: the handoff expires and the assistant defers.
                let mut stored = HANDOFFS
                    .require(&mut *tx, h.handoff_request_id)
                    .await
                    .unwrap();
                stored.expires_at =
                    UtcDateTime::from_datetime(Utc::now() - Duration::seconds(1));
                HANDOFFS.upsert(&mut *tx, &stored).await.unwrap();
                let f = orch
                    .expire_stale_handoffs(&mut *tx, call.call_id)
                    .await
                    .unwrap();
                assert_eq!(f.state, CallState::AiResumed);
                assert_eq!(
                    HANDOFFS
                        .require(&mut *tx, h.handoff_request_id)
                        .await
                        .unwrap()
                        .status,
                    HandoffStatus::Expired
                );
                assert_eq!(f.deferred_questions, ["Can you do a 2-month deposit?"]);
                f
            }
            Some(HandoffStatus::Accepted) => {
                let f = orch
                    .owner_response(&mut *tx, h.handoff_request_id, owner, HandoffStatus::Accepted)
                    .await
                    .unwrap();
                assert_eq!(f.state, CallState::OwnerJoining);
                let conf = f.conference_id.clone().unwrap();
                let owner_leg = f
                    .participant_states
                    .iter()
                    .find(|s| s.role == "owner")
                    .and_then(|s| s.leg_id.clone())
                    .unwrap();
                assert_eq!(
                    fixture
                        .calls()
                        .iter()
                        .find(|x| x.id == owner_leg)
                        .unwrap()
                        .to,
                    phone
                );
                let cs = encode_client_state(&client_state(&call.call_id.to_string(), "owner"));
                orch.handle_event(
                    &mut *tx,
                    &json!({"data": {"event_type": "call.answered",
                        "payload": {"call_control_id": owner_leg, "client_state": cs}}}),
                )
                .await
                .unwrap();
                assert_eq!(fixture.conferences()[&conf].last(), Some(&owner_leg));
                let joined = orch
                    .handle_event(
                        &mut *tx,
                        &json!({"data": {"event_type": "conference.participant.joined",
                            "payload": {"call_control_id": owner_leg, "client_state": cs}}}),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(joined.state, CallState::HumanJoined);
                joined
            }
            Some(r) => {
                let f = orch
                    .owner_response(&mut *tx, h.handoff_request_id, owner, r)
                    .await
                    .unwrap();
                assert_eq!(f.state, CallState::AiResumed);
                assert!(f.transcript.iter().any(|x| x.speaker == "assistant"
                    && x.text.contains("follow up with you directly")
                    && x.text.contains("can't agree to anything")));
                f
            }
        };
        let events = t.app.events().list(&mut *tx, pid, 0, 500).await.unwrap();
        for (_, e) in &events {
            let s = serde_json::to_string(&e.payload).unwrap();
            assert!(!s.contains("join_token"), "{s}");
            assert!(!s.contains(&reference), "{s}");
        }
        assert!(!final_call.transcript.is_empty());
        tx.commit().await.unwrap();
    }
});

both_backends!(owner_accept_without_phone_defers, |db| {
    let t = TestApp::new(db, &[]);
    // Nobody can be texted: the honest record says so.
    let none: Arc<dyn OwnerNotifier> = Arc::new(NotConfiguredNotifier);
    t.app.extensions.insert(Arc::new(OwnerNotify::new(none)));
    let (owner, pid, recipient, a) = approved_call(&t, None).await;
    let mut tx = t.app.store.begin().await.unwrap();
    let mut orch = CallOrchestrator::new(&t.app);
    let call = orch
        .start(&mut *tx, start_args(pid, owner, &recipient, Some(a), None))
        .await
        .unwrap();
    orch.advance(&mut *tx, call.call_id, CallState::Intro, None)
        .await
        .unwrap();
    orch.advance(&mut *tx, call.call_id, CallState::Active, None)
        .await
        .unwrap();
    let h = orch
        .request_handoff(
            &mut *tx,
            call.call_id,
            HandoffReason::BindingCommitment,
            "Sign today?",
        )
        .await
        .unwrap();
    let f = orch
        .owner_response(&mut *tx, h.handoff_request_id, owner, HandoffStatus::Accepted)
        .await
        .unwrap();
    assert_eq!(f.state, CallState::AiResumed);
    assert!(f.deferred_questions.contains(&"Sign today?".to_owned()));
    tx.commit().await.unwrap();
    t.app
        .owner_notify()
        .dispatch(t.app.clone(), orch.take_notices());
    t.app.owner_notify().idle().await;
    let mut tx = t.app.store.begin().await.unwrap();
    let comms = COMMUNICATIONS
        .list(&mut *tx, Query::new().eq("project_id", pid))
        .await
        .unwrap();
    // Not linked / not reachable: not sent, recorded as FAILED.
    assert!(comms
        .iter()
        .any(|c| c.purpose == "handoff_request" && c.state.as_str() == "FAILED"));
    let events = t.app.events().list(&mut *tx, pid, 0, 500).await.unwrap();
    let notified: Vec<_> = events
        .iter()
        .filter(|(_, e)| e.event_type.as_str() == "communication.owner_notified")
        .collect();
    assert_eq!(notified.len(), 1);
    assert_eq!(notified[0].1.payload["delivery"], "failed");
    assert!(notified[0]
        .1
        .idempotency_key
        .starts_with("notify-unreached:handoff-notify:"));
    tx.commit().await.unwrap();
});

both_backends!(call_provider_failure_and_disconnect, |db| {
    let t = TestApp::new(db, &[]);
    let fixture = t.app.voice().fixture.clone().unwrap();
    let (owner, pid, recipient, a) = approved_call(&t, Some("+15555550177")).await;
    fixture.set_fail_next_dial(true);
    let mut tx = t.app.store.begin().await.unwrap();
    let mut orch = CallOrchestrator::new(&t.app);
    let call = orch
        .start(&mut *tx, start_args(pid, owner, &recipient, Some(a), None))
        .await
        .unwrap();
    assert_eq!(call.state, CallState::Failed);
    assert!(call.failure_reason.as_deref().unwrap().contains("simulated"));
    assert!(call.ended_at.is_some());
    // A dial the carrier refused does not spend the approval (Python behaviour).
    assert_eq!(
        APPROVALS.require(&mut *tx, a).await.unwrap().approval_status,
        ApprovalStatus::Approved
    );
    tx.commit().await.unwrap();

    let (owner, pid, recipient, a) = approved_call(&t, Some("+15555550178")).await;
    let mut tx = t.app.store.begin().await.unwrap();
    let mut orch = CallOrchestrator::new(&t.app);
    let call = orch
        .start(&mut *tx, start_args(pid, owner, &recipient, Some(a), None))
        .await
        .unwrap();
    orch.advance(&mut *tx, call.call_id, CallState::Intro, None)
        .await
        .unwrap();
    orch.advance(&mut *tx, call.call_id, CallState::Active, None)
        .await
        .unwrap();
    let dropped = orch
        .advance(
            &mut *tx,
            call.call_id,
            CallState::Failed,
            Some("counterparty disconnected"),
        )
        .await
        .unwrap();
    assert_eq!(dropped.state, CallState::Failed);
    assert!(dropped.ended_at.is_some());
    let err = orch
        .request_handoff(
            &mut *tx,
            call.call_id,
            HandoffReason::BindingCommitment,
            "sign?",
        )
        .await
        .unwrap_err();
    assert_eq!(err.status().as_u16(), 409);
    tx.commit().await.unwrap();
});

both_backends!(start_refuses_without_the_exact_approval, |db| {
    let t = TestApp::new(db, &[]);
    let fixture = t.app.voice().fixture.clone().unwrap();
    let (owner, pid, recipient, approved) = approved_call(&t, None).await;
    let mut other = recipient.clone();
    other.insert("phone".into(), "+15555550999".into());
    // (label, approval, recipient, expected status, expected error code)
    let mut cases: Vec<(&str, Option<Uuid>, IndexMap<String, String>, u16, &str)> = vec![
        ("no approval", None, recipient.clone(), 409, "APPROVAL_REQUIRED"),
        (
            "approval for another recipient",
            Some(approved),
            other.clone(),
            409,
            "APPROVAL_PAYLOAD_MISMATCH",
        ),
        (
            "unknown approval",
            Some(Uuid::new_v4()),
            recipient.clone(),
            404,
            "NOT_FOUND",
        ),
    ];
    // Another project's approval (the same owner's second project): not found.
    let token = t.token_for(owner);
    let p2 = t
        .create_project(
            &token,
            &json!({"project_name": "t2", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    let pid2: Uuid = p2["project"]["project_id"].as_str().unwrap().parse().unwrap();
    let foreign = approve_payload(&t, pid2, owner, &recipient, None, ActionType::PlaceCall).await;
    cases.push((
        "another project's approval",
        Some(foreign),
        recipient.clone(),
        404,
        "NOT_FOUND",
    ));
    // A message approval for the same payload: another action type.
    let message = approve_payload(&t, pid, owner, &recipient, None, ActionType::SendMessage).await;
    cases.push((
        "approval for another action",
        Some(message),
        recipient.clone(),
        409,
        "APPROVAL_REQUIRED",
    ));
    // A pending approval.
    let mut tx = t.app.store.begin().await.unwrap();
    let pending = t
        .app
        .approvals()
        .request(
            &mut *tx,
            ApprovalSpec::new(
                pid,
                owner,
                "t",
                ActionType::PlaceCall,
                ss_domain::communications::call_payload(pid, &recipient, None),
                "call",
                format!("p-{}", Uuid::new_v4()),
            ),
        )
        .await
        .unwrap();
    // An approved approval that has expired since.
    let expired = t
        .app
        .approvals()
        .request(
            &mut *tx,
            ApprovalSpec::new(
                pid,
                owner,
                "t",
                ActionType::PlaceCall,
                ss_domain::communications::call_payload(pid, &recipient, None),
                "call",
                format!("e-{}", Uuid::new_v4()),
            ),
        )
        .await
        .unwrap();
    let mut decided = t
        .app
        .approvals()
        .decide(
            &mut *tx,
            expired.approval_id,
            owner,
            ApprovalStatus::Approved,
            &expired.action_payload_hash,
        )
        .await
        .unwrap();
    decided.expires_at = UtcDateTime::from_datetime(Utc::now() - Duration::seconds(1));
    APPROVALS.upsert(&mut *tx, &decided).await.unwrap();
    // A rejected one.
    let rejected = t
        .app
        .approvals()
        .request(
            &mut *tx,
            ApprovalSpec::new(
                pid,
                owner,
                "t",
                ActionType::PlaceCall,
                ss_domain::communications::call_payload(pid, &recipient, None),
                "call",
                format!("r-{}", Uuid::new_v4()),
            ),
        )
        .await
        .unwrap();
    t.app
        .approvals()
        .decide(
            &mut *tx,
            rejected.approval_id,
            owner,
            ApprovalStatus::Rejected,
            &rejected.action_payload_hash,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    cases.push(("pending", Some(pending.approval_id), recipient.clone(), 409, "APPROVAL_REQUIRED"));
    cases.push(("expired", Some(expired.approval_id), recipient.clone(), 409, "APPROVAL_REQUIRED"));
    cases.push(("rejected", Some(rejected.approval_id), recipient.clone(), 409, "APPROVAL_REQUIRED"));
    for (label, approval, rcpt, status, code) in cases {
        let mut tx = t.app.store.begin().await.unwrap();
        let mut orch = CallOrchestrator::new(&t.app);
        let err = orch
            .start(&mut *tx, start_args(pid, owner, &rcpt, approval, None))
            .await
            .expect_err(label);
        assert_eq!(err.status().as_u16(), status, "{label}: {err:?}");
        assert_eq!(err.error_code(), code, "{label}");
        drop(tx);
        assert!(fixture.calls().is_empty(), "{label}: something was dialed");
    }
    // Spent once, refused afterwards.
    let mut tx = t.app.store.begin().await.unwrap();
    let mut orch = CallOrchestrator::new(&t.app);
    orch.start(&mut *tx, start_args(pid, owner, &recipient, Some(approved), None))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = t.app.store.begin().await.unwrap();
    let err = orch
        .start(&mut *tx, start_args(pid, owner, &recipient, Some(approved), None))
        .await
        .unwrap_err();
    assert_eq!(err.error_code(), "APPROVAL_REQUIRED");
    drop(tx);
    assert_eq!(fixture.calls().len(), 1);
    // The expired approval was not persisted as EXPIRED by the refused request (rolled back), and
    // nothing else was stored by the refusals.
    let mut tx = t.app.store.begin().await.unwrap();
    let calls = CALLS
        .list(&mut *tx, Query::new().eq("project_id", pid))
        .await
        .unwrap();
    assert_eq!(calls.len(), 1);
    tx.commit().await.unwrap();
});

both_backends!(concurrent_call_starts_dial_once, |db| {
    let c = Ctx::new(db).await;
    let pid = c.project().await;
    let prop = c.hollywood(pid).await;
    let out = c
        .outreach(pid, prop["property_id"].as_str().unwrap(), "VOICE")
        .await;
    assert_eq!(c.approve(&out["approval"]).await.status, StatusCode::OK);
    let body = serde_json::to_vec(
        &json!({"communication_id": out["communication"]["communication_id"]}),
    )
    .unwrap();
    let mut handles = Vec::new();
    for _ in 0..2 {
        let router = c.t.router.clone();
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/v1/projects/{pid}/calls"))
            .header("authorization", format!("Bearer {}", c.token))
            .header("content-type", "application/json")
            .body(Body::from(body.clone()))
            .unwrap();
        handles.push(tokio::spawn(async move {
            router.oneshot(req).await.unwrap().status().as_u16()
        }));
    }
    let mut statuses = Vec::new();
    for h in handles {
        statuses.push(h.await.unwrap());
    }
    statuses.sort();
    assert_eq!(statuses, [202, 409]);
    assert_eq!(c.fixture().calls().len(), 1, "exactly one dial");
    let calls = c.get(&format!("/api/v1/projects/{pid}/calls")).await;
    assert_eq!(calls.body.as_array().unwrap().len(), 1);
});

both_backends!(simulate_refusals, |db| {
    let c = Ctx::new(db).await;
    let pid = c.project().await;
    let (_, _, call) = c.approved_call(pid).await;
    let cid = call["call_id"].as_str().unwrap();
    // Unknown call: Python's require message.
    let unknown = Uuid::new_v4();
    let r = c
        .post(
            &format!("/api/v1/projects/{pid}/calls/{unknown}/simulate"),
            &json!({}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 404);
    assert_eq!(r.body["message"], format!("CallSession {unknown} not found"));
    // A call that was not created as a simulation.
    let mut tx = c.t.app.store.begin().await.unwrap();
    let mut stored = CALLS
        .require(&mut *tx, cid.parse().unwrap())
        .await
        .unwrap();
    stored.simulated = false;
    CALLS.upsert(&mut *tx, &stored).await.unwrap();
    tx.commit().await.unwrap();
    let r = c
        .post(&format!("/api/v1/projects/{pid}/calls/{cid}/simulate"), &json!({}))
        .await;
    assert_eq!(r.status.as_u16(), 409);
    assert_eq!(r.body["error_code"], "NOT_SIMULATED");
    // Unknown body field.
    let r = c
        .post(
            &format!("/api/v1/projects/{pid}/calls/{cid}/simulate"),
            &json!({"owner_action": "accept", "extra": 1}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 422);
});

both_backends!(simulator_refuses_with_a_live_voice_provider, |db| {
    let t = TestApp::new(
        db,
        &[
            ("VOICE_PROVIDER_MODE", "live"),
            ("TELNYX_API_KEY", "KEY"),
            ("TELNYX_CONNECTION_ID", "conn1"),
        ],
    );
    assert_eq!(t.app.voice().mode(), "live");
    let err = CallSimulator::new(&t.app).err().unwrap();
    assert_eq!(err.error_code(), "SIMULATOR_DISABLED");
    let ready = t.get("/ready", None).await;
    let telnyx = ready.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "telnyx")
        .unwrap()
        .clone();
    assert_eq!(
        telnyx,
        json!({"provider": "telnyx", "mode": "live", "status": "PARTIAL", "credentials_present": true,
               "note": "Call flow (dial, intro, transcription turns, handoff conference) per API reference; never dialed from CI; live outbound disabled."})
    );
});

both_backends!(simulated_call_later_and_no_response, |db| {
    for (action, expect_deferred) in [("later", "DEFERRED"), ("none", "EXPIRED")] {
        let c = Ctx::new(db).await;
        let pid = c.project().await;
        link_owner(&c.t, c.user, OWNER_PHONE).await;
        let (_, _, call) = c.approved_call(pid).await;
        let r = c
            .post(
                &format!(
                    "/api/v1/projects/{pid}/calls/{}/simulate",
                    call["call_id"].as_str().unwrap()
                ),
                &json!({"owner_action": action}),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        assert_eq!(r.body["state"], "COMPLETED");
        assert_eq!(
            r.body["deferred_questions"],
            json!([ss_server::services::call_simulator::ESCALATION_LINE])
        );
        let summary = r.body["outcome_summary"].as_str().unwrap();
        assert!(summary.contains("Deferred to you:"), "{summary}");
        let handoffs = c.get(&format!("/api/v1/projects/{pid}/handoffs")).await;
        assert_eq!(handoffs.body[0]["status"], expect_deferred);
        // The assistant never agreed to anything and said the owner will follow up.
        let transcript = r.body["transcript"].as_array().unwrap();
        assert!(transcript.iter().any(|t| t["speaker"] == "assistant"
            && t["text"]
                .as_str()
                .unwrap()
                .contains("make sure they follow up with you directly")));
        assert!(!transcript.iter().any(|t| t["speaker"] == "owner"));
        assert!(c.fixture().conferences().is_empty());
    }
});

both_backends!(every_route_is_scoped_to_the_owner_and_project, |db| {
    let c = Ctx::new(db).await;
    let pid = c.project().await;
    link_owner(&c.t, c.user, OWNER_PHONE).await;
    let (prop, out, call) = c.approved_call(pid).await;
    let cid = call["call_id"].as_str().unwrap().to_owned();
    // Make a handoff exist.
    let mut tx = c.t.app.store.begin().await.unwrap();
    let mut sim = CallSimulator::new(&c.t.app).unwrap();
    let cs = CALLS.require(&mut *tx, cid.parse().unwrap()).await.unwrap();
    let cs = sim
        .leg_events(
            &mut *tx,
            cs,
            vec![
                ("call.answered", Default::default()),
                ("call.speak.ended", Default::default()),
            ],
        )
        .await
        .unwrap();
    let cs = sim
        .say_broker(&mut *tx, cs, "Can I talk to the owner?")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let hid = cs.handoff_request_id.unwrap();
    let prop_id = prop["property_id"].as_str().unwrap();
    let comm_id = out["communication"]["communication_id"].as_str().unwrap();

    // A second owner: every route of the first owner's project is 404, unauthenticated is 401.
    let other = Uuid::new_v4();
    let other_token = c.t.token_for(other);
    let routes: Vec<(Method, String, Option<Value>)> = vec![
        (
            Method::POST,
            format!("/api/v1/projects/{pid}/outreach"),
            Some(json!({"property_id": prop_id, "channel": "VOICE", "recipient": {}, "purpose": "x"})),
        ),
        (Method::GET, format!("/api/v1/projects/{pid}/communications"), None),
        (
            Method::POST,
            format!("/api/v1/projects/{pid}/calls"),
            Some(json!({"communication_id": comm_id})),
        ),
        (Method::GET, format!("/api/v1/projects/{pid}/calls"), None),
        (
            Method::POST,
            format!("/api/v1/projects/{pid}/calls/{cid}/simulate"),
            Some(json!({})),
        ),
        (Method::GET, format!("/api/v1/projects/{pid}/handoffs"), None),
        (
            Method::POST,
            format!("/api/v1/projects/{pid}/handoffs/{hid}/response"),
            Some(json!({"response": "DEFERRED"})),
        ),
    ];
    for (m, path, body) in &routes {
        let r = c
            .t
            .call(m.clone(), path, Some(&other_token), body.as_ref())
            .await;
        assert_eq!(r.status.as_u16(), 404, "{m} {path}: {}", r.text);
        assert_eq!(r.body["message"], "project not found", "{path}");
        let r = c.t.call(m.clone(), path, None, body.as_ref()).await;
        assert_eq!(r.status.as_u16(), 401, "{m} {path}");
    }

    // The second owner's own project with the first owner's record ids: 404, nothing changes.
    let p2 = c
        .t
        .create_project(
            &other_token,
            &json!({"project_name": "other", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    let pid2 = p2["project"]["project_id"].as_str().unwrap().to_owned();
    let post2 = |path: String, body: Value| {
        let t = &c.t;
        let tok = other_token.clone();
        async move { t.post(&path, Some(&tok), &body).await }
    };
    let r = post2(
        format!("/api/v1/projects/{pid2}/outreach"),
        json!({"property_id": prop_id, "channel": "VOICE",
               "recipient": {"name": "x", "phone": "+15555550100"}, "purpose": "x"}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 404, "{}", r.text);
    assert_eq!(r.body["message"], "property not found");
    let r = post2(
        format!("/api/v1/projects/{pid2}/calls"),
        json!({"communication_id": comm_id}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 404, "{}", r.text);
    assert_eq!(r.body["message"], format!("CommunicationSession {comm_id} not found"));
    let r = post2(
        format!("/api/v1/projects/{pid2}/calls/{cid}/simulate"),
        json!({}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 404, "{}", r.text);
    assert_eq!(r.body["message"], "call not found");
    let r = post2(
        format!("/api/v1/projects/{pid2}/handoffs/{hid}/response"),
        json!({"response": "DEFERRED"}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 404, "{}", r.text);
    assert_eq!(r.body["message"], format!("OwnerHandoffRequest {hid} not found"));
    for list in ["communications", "calls", "handoffs"] {
        let r = c
            .t
            .get(&format!("/api/v1/projects/{pid2}/{list}"), Some(&other_token))
            .await;
        assert_eq!(r.body, json!([]), "{list}");
    }
    // The first owner's handoff is untouched and its call still waits.
    let after = call_of(&c.t, &json!(cid)).await;
    assert_eq!(after.state, CallState::HandoffPending);
    assert_eq!(c.fixture().calls().len(), 1);
    // Malformed ids are 422 after the project check, as FastAPI orders them.
    let r = c
        .post(
            &format!("/api/v1/projects/{pid}/handoffs/not-a-uuid/response"),
            &json!({"response": "DEFERRED"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 422);
    let r = c
        .t
        .post(
            &format!("/api/v1/projects/{pid}/handoffs/not-a-uuid/response"),
            Some(&other_token),
            &json!({"response": "DEFERRED"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 404);
});

#[test]
fn call_script_and_escalation_policy() {
    use ss_domain::communications::{availability, build_script, detect_escalation, extract};
    use ss_domain::contracts::{AssessmentStatus, InfrastructureAssessment};
    let a = |rid: &str, st: AssessmentStatus| {
        InfrastructureAssessment::from_json_value(json!({
            "project_id": Uuid::from_u128(1), "property_id": Uuid::from_u128(2),
            "requirement_id": rid, "status": st.as_str(), "uncertainty": "x",
            "verification_questions": ["?"],
        }))
        .unwrap()
    };
    use ss_core::Contract;
    let script = build_script(&[
        a("fs.electrical.tenant_service", AssessmentStatus::Unknown),
        a("fs.space.min_area", AssessmentStatus::Fails),
    ]);
    let claims: Vec<&str> = script.iter().map(|q| q.claim_type.as_str()).collect();
    assert_eq!(claims, ["tenant_subpanel_amps", "available_from"]);
    assert!(script[0].requirement_id.is_some());
    let q = &script[0];
    assert_eq!(extract(q, "It's a 400 amp service, 3 phase").value, json!(400));
    assert!(extract(q, "Not sure, maybe 200 amps? I'd have to check")
        .value
        .is_null());
    let plumb = build_script(&[a(
        "fs.plumbing.treatment_sinks",
        AssessmentStatus::MayRequireWork,
    )])[0]
        .clone();
    assert_eq!(extract(&plumb, "Yes, there's a sink in back").value, json!(true));
    assert_eq!(extract(&plumb, "No, it's all open").value, json!(false));
    assert_eq!(extract(&availability(), "January 1st").value, json!("January 1st"));
    assert_eq!(
        detect_escalation("Will you need a guarantor?"),
        Some(HandoffReason::GuarantorRequested)
    );
    assert_eq!(
        detect_escalation("We need first month and deposit by Friday"),
        Some(HandoffReason::FinancialDetails)
    );
    assert_eq!(
        detect_escalation("Can I talk to the owner?"),
        Some(HandoffReason::CounterpartyRequestedOwner)
    );
    assert_eq!(detect_escalation("Is there a sink in the back room?"), None);
    // The simulator's escalation line escalates.
    assert!(detect_escalation(ss_server::services::call_simulator::ESCALATION_LINE).is_some());
    let _ = event("call.answered", serde_json::Map::new());
}
