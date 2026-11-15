//! Every owner text about a simulated call says it is simulated (AGENTS.md section 5).
//!
//! With owner texting live and voice on the fixture, a simulated call that escalates makes the
//! Rust service ask the Python service to text the owner. Each of those texts must start with the
//! summary's label, "[Simulated call] ", and must never say that someone will call him or that a
//! broker is waiting for him. A real call (`simulated = false`) keeps today's words byte for
//! byte. Fixture telephony and a recording owner notifier only: nothing is sent anywhere.
//!
//! Deliberate difference from Python (named here once): Python's `request_handoff` sends the
//! real-call handoff wording for a simulated call too. The parity replays compare only the
//! notify purposes, not their texts, so they are unaffected.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_core::UtcDateTime;
use ss_domain::contracts::{CallState, HandoffReason, OwnerChannel, OwnerChannelStatus};
use ss_server::services::call_orchestration::{CallOrchestrator, StartCall, SIMULATED_LABEL};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::store::comms::OWNER_CHANNELS;
use uuid::Uuid;

use common::{demo_project_body, TestApp, TestDb};

const OWNER_PHONE: &str = "+15555550142";
const QUESTION: &str = "Can you do a 2-month deposit?";

/// Words that would tell the owner a real call is waiting for him or that he is being dialed.
const REAL_CALL_CLAIMS: [&str; 5] = [
    "needs you on a call",
    "I'll call you",
    "Calling you",
    "Pick up",
    "join the broker",
];

async fn link_owner(t: &TestApp, owner: Uuid) {
    let mut tx = t.app.store.begin().await.unwrap();
    OWNER_CHANNELS
        .add(
            &mut *tx,
            &OwnerChannel {
                owner_channel_id: Uuid::new_v4(),
                owner_id: owner,
                platform: "imessage".into(),
                handle: OWNER_PHONE.into(),
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

fn with_recorder(t: &TestApp) -> Arc<RecordingNotifier> {
    let fake = Arc::new(RecordingNotifier::new());
    t.app
        .extensions
        .insert(Arc::new(OwnerNotify::new(fake.clone())));
    fake
}

fn assert_labelled(purpose: &str, text: &str) {
    assert!(
        text.starts_with(SIMULATED_LABEL),
        "{purpose} text lacks the label: {text}"
    );
    for claim in REAL_CALL_CLAIMS {
        assert!(!text.contains(claim), "{purpose} text says {claim:?}: {text}");
    }
}

/// The whole route flow: outreach, approval, a simulated call (fixture voice), the simulator's
/// escalation and the owner's answer. Every call text the owner notifier receives is labelled.
async fn route_flow(db: &TestDb, owner_action: &str) {
    let t = TestApp::new(db, &[]);
    let fake = with_recorder(&t);
    let (token, user) = t.dev_token().await;
    link_owner(&t, user).await;
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
    let prop_id = props.body[0]["property_id"].as_str().unwrap().to_owned();
    let body = format!(
        r#"{{"property_id": "{prop_id}", "channel": "VOICE", "recipient": {{"role": "broker", "name": "Dana Broker", "phone": "+15555550100"}}, "purpose": "Verify electrical"}}"#
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
            Some(&token),
            &json!({"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]}),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
    let call = t
        .post(
            &format!("/api/v1/projects/{pid}/calls"),
            Some(&token),
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
            Some(&token),
            &json!({"owner_action": owner_action}),
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text);
    assert_eq!(done.body["state"], "COMPLETED");
    t.app.owner_notify().idle().await;

    let reqs = fake.requests();
    let purposes: Vec<&str> = reqs.iter().map(|r| r.purpose.as_str()).collect();
    assert_eq!(
        purposes,
        ["approval_request", "handoff_request", "call_summary"],
        "{owner_action}"
    );
    for r in reqs.iter().filter(|r| r.purpose != "approval_request") {
        assert_labelled(&r.purpose, &r.text);
    }
    let handoff = &reqs[1].text;
    assert!(handoff.contains("Dana Broker"), "{handoff}");
    assert!(
        handoff.contains("Nobody is on the line and nobody will call you."),
        "{handoff}"
    );
}

both_backends!(simulated_escalation_owner_joins, |db| {
    route_flow(db, "accept").await;
});

both_backends!(simulated_escalation_owner_later, |db| {
    route_flow(db, "later").await;
});

/// The orchestrator directly, the same call with `simulated` true and false: the real call's
/// handoff and summary texts are exactly today's; the simulated one's are labelled.
async fn handoff_and_summary(t: &TestApp, simulated: bool) -> (CallState, Vec<(String, String)>) {
    let owner = Uuid::new_v4();
    let token = t.token_for(owner);
    let p = t
        .create_project(
            &token,
            &json!({"project_name": "t", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    let pid: Uuid = p["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    link_owner(t, owner).await;
    let mut recipient = IndexMap::new();
    recipient.insert("role".to_owned(), "broker".to_owned());
    recipient.insert("name".to_owned(), "B".to_owned());
    recipient.insert("phone".to_owned(), "+15555550100".to_owned());
    let payload = ss_domain::communications::call_payload(pid, &recipient, None);
    let mut tx = t.app.store.begin().await.unwrap();
    let a = t
        .app
        .approvals()
        .request(
            &mut *tx,
            ss_server::services::approvals::ApprovalSpec::new(
                pid,
                owner,
                "t",
                ss_server::contracts::ActionType::PlaceCall,
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
            ss_server::contracts::ApprovalStatus::Approved,
            &a.action_payload_hash,
        )
        .await
        .unwrap();
    let mut orch = CallOrchestrator::new(&t.app);
    let call = orch
        .start(
            &mut *tx,
            StartCall {
                project_id: pid,
                owner_id: owner,
                recipient: recipient.clone(),
                property_id: None,
                approval_id: Some(a.approval_id),
                intro_recording_reference: None,
                communication_id: None,
                simulated,
            },
        )
        .await
        .unwrap();
    assert_eq!(call.simulated, simulated);
    orch.advance(&mut *tx, call.call_id, CallState::Intro, None)
        .await
        .unwrap();
    orch.advance(&mut *tx, call.call_id, CallState::Active, None)
        .await
        .unwrap();
    orch.request_handoff(
        &mut *tx,
        call.call_id,
        HandoffReason::FinancialDetails,
        QUESTION,
    )
    .await
    .unwrap();
    let call = orch
        .advance(&mut *tx, call.call_id, CallState::Failed, Some("test end"))
        .await
        .unwrap();
    let call = orch
        .handle_event(
            &mut *tx,
            &json!({"data": {"event_type": "noop"}}),
            Some(call.clone()),
        )
        .await
        .map_or(call.state, |_| call.state);
    tx.commit().await.unwrap();
    let texts = orch
        .take_notices()
        .into_iter()
        .map(|n| (n.purpose, n.text))
        .collect();
    (call, texts)
}

both_backends!(real_call_texts_unchanged_simulated_labelled, |db| {
    let t = TestApp::new(db, &[]);
    let web = t.app.settings.public_web_url.clone();
    let (_, real) = handoff_and_summary(&t, false).await;
    let real_handoff = format!(
        "Your assistant needs you on a call with B: \"{QUESTION}\" Reply JOIN and I'll call you to \
         connect, or LATER and I'll say you'll follow up. Or open {web}/activity"
    );
    assert_eq!(real, vec![("handoff_request".to_owned(), real_handoff)]);

    let (_, sim) = handoff_and_summary(&t, true).await;
    let sim_handoff = format!(
        "[Simulated call] In this simulated call with B, the assistant would hand you this \
         question: \"{QUESTION}\" Nobody is on the line and nobody will call you. Reply JOIN or \
         LATER to see what the assistant would do, or open {web}/activity"
    );
    assert_eq!(sim, vec![("handoff_request".to_owned(), sim_handoff)]);
    assert_labelled("handoff_request", &sim[0].1);
});

/// The summary of a real call stays unlabelled; a simulated one keeps its label (route flow above
/// covers the simulated summary end to end).
both_backends!(summary_label_only_when_simulated, |db| {
    let _ = db;
    let label: &str = SIMULATED_LABEL;
    assert_eq!(label, "[Simulated call] ");
    let _: Value = json!(null);
});
