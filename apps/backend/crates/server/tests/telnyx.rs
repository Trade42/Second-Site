//! Server part 4: `POST /webhooks/telnyx` (signature on the raw body, dedupe, routing to the call
//! orchestrator) and the live Telnyx client, which runs here only against a local mock server.
//! No real call is placed by any test.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_domain::contracts::{CallSession, CallState, HandoffReason, HandoffStatus};
use ss_domain::integrations::base::ProviderErrorKind;
use ss_domain::integrations::telnyx::{
    client_state, decode_client_state, encode_client_state, TelephonyError, TelephonyProvider,
};
use ss_server::contracts::{ActionType, ApprovalStatus};
use ss_server::integrations::telnyx::{LiveTelnyxProvider, Voice};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::call_orchestration::{CallOrchestrator, StartCall};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::store::comms::{webhooks, CALLS};
use ss_server::store::repo::APPROVALS;
use ss_server::store::Query;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer, Recorded};
use common::{TestApp, TestDb};

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn public_key_b64() -> String {
    base64::engine::general_purpose::STANDARD.encode(key().verifying_key().to_bytes())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn signed_request(body: &[u8], ts: i64, signer: &SigningKey) -> Request<Body> {
    let mut msg = format!("{ts}|").into_bytes();
    msg.extend_from_slice(body);
    let sig = base64::engine::general_purpose::STANDARD.encode(signer.sign(&msg).to_bytes());
    Request::builder()
        .method(Method::POST)
        .uri("/webhooks/telnyx")
        .header("telnyx-signature-ed25519", sig)
        .header("telnyx-timestamp", ts.to_string())
        .header("content-type", "application/json")
        .body(Body::from(body.to_vec()))
        .unwrap()
}

fn app_with_key(db: &TestDb, extra: &[(&str, &str)]) -> (TestApp, Arc<RecordingNotifier>) {
    let pk = public_key_b64();
    let mut vars: Vec<(&str, &str)> = vec![("TELNYX_PUBLIC_KEY", pk.as_str())];
    vars.extend_from_slice(extra);
    let t = TestApp::new(db, &vars);
    let fake = Arc::new(RecordingNotifier::new());
    t.app
        .extensions
        .insert(Arc::new(OwnerNotify::new(fake.clone())));
    (t, fake)
}

async fn deliver(t: &TestApp, id: &str, et: &str, payload: Value) -> u16 {
    let body = serde_json::to_vec(&json!({
        "data": {"record_type": "event", "id": id, "event_type": et, "payload": payload}
    }))
    .unwrap();
    t.send(signed_request(&body, now(), &key()))
        .await
        .status
        .as_u16()
}

async fn processed(t: &TestApp) -> Vec<ss_server::store::Row> {
    let mut tx = t.app.store.begin().await.unwrap();
    let rows = webhooks::rows(&mut *tx, "telnyx").await.unwrap();
    tx.commit().await.unwrap();
    rows
}

async fn load(t: &TestApp, id: Uuid) -> CallSession {
    let mut tx = t.app.store.begin().await.unwrap();
    let c = CALLS.require(&mut *tx, id).await.unwrap();
    tx.commit().await.unwrap();
    c
}

/// A project of a fresh owner (with or without a linked phone) and a started call, DIALING.
async fn dialing_call(t: &TestApp, owner_phone: Option<&str>) -> (Uuid, CallSession) {
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
    if let Some(phone) = owner_phone {
        let mut tx = t.app.store.begin().await.unwrap();
        ss_server::store::comms::OWNER_CHANNELS
            .add(
                &mut *tx,
                &ss_domain::contracts::OwnerChannel {
                    owner_channel_id: Uuid::new_v4(),
                    owner_id: owner,
                    platform: "imessage".into(),
                    handle: phone.into(),
                    status: ss_domain::contracts::OwnerChannelStatus::Linked,
                    space_id: Some("spc".into()),
                    agent_user_id: Some("usr".into()),
                    last_inbound_message_id: None,
                    linked_at: None,
                    updated_at: ss_core::UtcDateTime::now(),
                },
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
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
            ApprovalSpec::new(
                pid,
                owner,
                "t",
                ActionType::PlaceCall,
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
    let mut orch = CallOrchestrator::new(&t.app);
    let call = orch
        .start(
            &mut *tx,
            StartCall {
                project_id: pid,
                owner_id: owner,
                recipient,
                property_id: None,
                approval_id: Some(a.approval_id),
                intro_recording_reference: None,
                communication_id: None,
                simulated: false,
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (owner, call)
}

fn cs(call_id: Uuid, role: &str) -> String {
    encode_client_state(&client_state(&call_id.to_string(), role))
}

both_backends!(telnyx_webhook_ed25519, |db| {
    let (t, _) = app_with_key(db, &[]);
    let body = serde_json::to_vec(
        &json!({"data": {"id": "tx-evt-1", "event_type": "call.answered", "payload": {}}}),
    )
    .unwrap();
    let ts = now();
    assert_eq!(
        t.send(signed_request(&body, ts, &key()))
            .await
            .status
            .as_u16(),
        204
    );
    let mut tampered = body.clone();
    tampered.push(b' ');
    let mut req = signed_request(&body, ts, &key());
    *req.body_mut() = Body::from(tampered);
    let r = t.send(req).await;
    assert_eq!(r.status.as_u16(), 400);
    assert_eq!(r.body["error_code"], "INVALID_SIGNATURE");
    // The verified event was recorded once (fixture mode: not a live verification).
    let rows = processed(&t).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text("provider_event_id").unwrap(), "tx-evt-1");
    assert!(!rows[0].boolean("verified").unwrap());
    assert_eq!(rows[0].json("payload").unwrap()["mode"], "fixture");
});

both_backends!(bad_and_missing_signatures_are_refused_in_both_modes, |db| {
    // Live mode for the webhook label (VOICE_PROVIDER_MODE=live), but no TELNYX_API_KEY, so the
    // telephony provider stays the fixture and nothing can reach a carrier.
    for mode in ["live", "fixture"] {
        let (t, _) = app_with_key(db, &[("VOICE_PROVIDER_MODE", mode)]);
        assert_eq!(t.app.voice().mode(), "fixture");
        let id = format!("evt-{mode}-{}", Uuid::new_v4());
        let body = serde_json::to_vec(
            &json!({"data": {"id": id, "event_type": "call.hangup", "payload": {}}}),
        )
        .unwrap();
        let base = processed(&t).await.len();
        // Missing headers.
        let r = t
            .send(
                Request::builder()
                    .method(Method::POST)
                    .uri("/webhooks/telnyx")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await;
        assert_eq!(r.status.as_u16(), 400, "{mode}");
        assert_eq!(
            r.body["message"],
            "missing telnyx-signature-ed25519 / telnyx-timestamp"
        );
        // Signed by another key.
        let r = t
            .send(signed_request(
                &body,
                now(),
                &SigningKey::from_bytes(&[9u8; 32]),
            ))
            .await;
        assert_eq!(r.status.as_u16(), 400, "{mode}");
        assert_eq!(r.body["message"], "telnyx signature mismatch");
        // Garbage signature.
        let mut req = signed_request(&body, now(), &key());
        req.headers_mut()
            .insert("telnyx-signature-ed25519", "AAAA".parse().unwrap());
        assert_eq!(t.send(req).await.status.as_u16(), 400, "{mode}");
        // Stale timestamp (correctly signed).
        let r = t.send(signed_request(&body, now() - 3600, &key())).await;
        assert_eq!(r.status.as_u16(), 400, "{mode}");
        assert_eq!(r.body["message"], "webhook timestamp outside tolerance");
        assert_eq!(processed(&t).await.len(), base, "{mode}: nothing stored");
        // A good one is stored with the mode's label.
        assert_eq!(
            t.send(signed_request(&body, now(), &key()))
                .await
                .status
                .as_u16(),
            204
        );
        let rows = processed(&t).await;
        let row = rows
            .iter()
            .find(|r| r.text("provider_event_id").unwrap() == id)
            .unwrap();
        assert_eq!(row.boolean("verified").unwrap(), mode == "live");
        assert_eq!(row.json("payload").unwrap()["mode"], mode);
        assert_eq!(
            row.json("payload").unwrap()["event"]["data"]["id"],
            Value::from(id.clone())
        );
    }
    // Without TELNYX_PUBLIC_KEY every delivery is refused, in both modes.
    for mode in ["live", "fixture"] {
        let t = TestApp::new(db, &[("VOICE_PROVIDER_MODE", mode)]);
        let body = br#"{"data":{"id":"x","event_type":"call.hangup","payload":{}}}"#;
        let r = t.send(signed_request(body, now(), &key())).await;
        assert_eq!(r.status.as_u16(), 400);
        assert_eq!(r.body["message"], "TELNYX_PUBLIC_KEY not configured");
    }
});

both_backends!(webhooks_drive_a_call_deduplicated_and_in_any_order, |db| {
    let (t, fake) = app_with_key(db, &[]);
    let fixture = t.app.voice().fixture.clone().unwrap();
    let (_, call) = dialing_call(&t, None).await;
    let (_, other) = dialing_call(&t, None).await; // a second owner's call
    let leg = call.provider_call_id.clone().unwrap();
    let id = call.call_id;
    let mine = cs(id, "counterparty");
    // Out of order: the end of the intro arrives before the answer, and is ignored.
    assert_eq!(
        deliver(
            &t,
            "e1",
            "call.speak.ended",
            json!({"call_control_id": leg, "client_state": mine})
        )
        .await,
        204
    );
    assert_eq!(load(&t, id).await.state, CallState::Dialing);
    // Answered (the carrier's own event): INTRO, intro spoken.
    assert_eq!(
        deliver(
            &t,
            "e2",
            "call.answered",
            json!({"call_control_id": leg, "client_state": mine})
        )
        .await,
        204
    );
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::Intro);
    let transcript = c.transcript.len();
    let commands = fixture.commands().len();
    // Replay of the same event id: nothing happens.
    assert_eq!(
        deliver(
            &t,
            "e2",
            "call.answered",
            json!({"call_control_id": leg, "client_state": mine})
        )
        .await,
        204
    );
    // A second answer with a new id (carrier duplicate): ignored outside DIALING.
    assert_eq!(
        deliver(
            &t,
            "e3",
            "call.answered",
            json!({"call_control_id": leg, "client_state": mine})
        )
        .await,
        204
    );
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::Intro);
    assert_eq!(c.transcript.len(), transcript);
    assert_eq!(fixture.commands().len(), commands);
    // Intro done: questions start; transcription is started on the broker leg.
    deliver(
        &t,
        "e4",
        "call.speak.ended",
        json!({"call_control_id": leg, "client_state": mine}),
    )
    .await;
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::Active);
    assert!(fixture
        .commands()
        .iter()
        .any(|x| x.cmd() == "transcription_start" && x.leg() == leg));
    // A second owner's call id in client_state on this leg: matched by the leg, the other
    // owner's call is untouched.
    let other_before = load(&t, other.call_id).await;
    deliver(
        &t,
        "e5",
        "call.transcription",
        json!({"call_control_id": leg, "client_state": cs(other.call_id, "counterparty"),
               "transcription_data": {"transcript": "Hello?", "is_final": false}}),
    )
    .await;
    assert_eq!(load(&t, other.call_id).await, other_before);
    // Malformed client_state (not base64 JSON; not even a string): matched by the leg, no 500.
    assert_eq!(
        deliver(
            &t,
            "e6",
            "call.transcription",
            json!({"call_control_id": leg, "client_state": "!!!",
            "transcription_data": {"transcript": "  ", "is_final": true}})
        )
        .await,
        204
    );
    assert_eq!(
        deliver(
            &t,
            "e7",
            "call.transcription",
            json!({"call_control_id": leg, "client_state": 5,
            "transcription_data": {"transcript": "x", "is_final": false}})
        )
        .await,
        204
    );
    // Transcription events carry no client_state: matched by leg. The answer is a finding.
    deliver(
        &t,
        "e8",
        "call.transcription",
        json!({"call_control_id": leg,
               "transcription_data": {"transcript": "It's available December first.", "is_final": true}}),
    )
    .await;
    let c = load(&t, id).await;
    assert_eq!(c.findings.len(), 1);
    assert_eq!(c.findings[0].claim_type.as_deref(), Some("available_from"));
    assert_eq!(c.findings[0].classification, "OWNER_REPORTED");
    assert_eq!(c.question_index, 1);
    // The script is done: the assistant thanked them and hung up its leg.
    assert_eq!(fixture.commands().last().unwrap().cmd(), "hangup");
    // Hangup: COMPLETED on the carrier's event, summary text queued after the commit.
    deliver(
        &t,
        "e9",
        "call.hangup",
        json!({"call_control_id": leg, "client_state": mine,
        "hangup_cause": "normal_clearing"}),
    )
    .await;
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::Completed);
    assert!(c.ended_at.is_some());
    assert!(c
        .outcome_summary
        .as_deref()
        .unwrap()
        .contains("available_from"));
    t.app.owner_notify().idle().await;
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].purpose, "call_summary");
    assert_eq!(reqs[0].idempotency_key, format!("call-summary:{id}"));
    assert!(!reqs[0].text.starts_with("[Simulated call]"));
    // After the end, later events change nothing.
    deliver(
        &t,
        "e10",
        "call.answered",
        json!({"call_control_id": leg, "client_state": mine}),
    )
    .await;
    assert_eq!(load(&t, id).await, c);
    // An event of an unknown call is recorded and ignored.
    assert_eq!(
        deliver(
            &t,
            "e11",
            "call.answered",
            json!({"call_control_id": "nope"})
        )
        .await,
        204
    );
    let rows = processed(&t).await;
    assert_eq!(rows.len(), 11, "one row per distinct event id");
    assert_eq!(load(&t, other.call_id).await, other_before);
});

both_backends!(webhooks_for_both_legs_of_a_handoff, |db| {
    let (t, _) = app_with_key(db, &[]);
    let fixture = t.app.voice().fixture.clone().unwrap();
    let (owner, call) = dialing_call(&t, Some("+15555550160")).await;
    let id = call.call_id;
    let leg = call.provider_call_id.clone().unwrap();
    let broker = cs(id, "counterparty");
    deliver(
        &t,
        "h1",
        "call.answered",
        json!({"call_control_id": leg, "client_state": broker}),
    )
    .await;
    deliver(
        &t,
        "h2",
        "call.speak.ended",
        json!({"call_control_id": leg, "client_state": broker}),
    )
    .await;
    // Escalation on the broker's utterance.
    deliver(
        &t,
        "h3",
        "call.transcription",
        json!({"call_control_id": leg,
        "transcription_data": {"transcript": "Will you need a guarantor?", "is_final": true}}),
    )
    .await;
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::HandoffPending);
    // While the handoff waits, the assistant stays quiet and records nothing.
    let said = c
        .transcript
        .iter()
        .filter(|x| x.speaker == "assistant")
        .count();
    deliver(&t, "h4", "call.transcription", json!({"call_control_id": leg,
        "transcription_data": {"transcript": "We also need first month and deposit", "is_final": true}})).await;
    let c = load(&t, id).await;
    assert_eq!(
        c.transcript
            .iter()
            .filter(|x| x.speaker == "assistant")
            .count(),
        said
    );
    assert!(c.findings.is_empty());
    // The owner accepts (web route): the owner's phone is dialed and conferenced.
    let r = t
        .post(
            &format!(
                "/api/v1/projects/{}/handoffs/{}/response",
                c.project_id,
                c.handoff_request_id.unwrap()
            ),
            Some(&t.token_for(owner)),
            &json!({"response": "ACCEPTED"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.text);
    assert_eq!(r.body["state"], "OWNER_JOINING");
    let c = load(&t, id).await;
    let owner_leg = c
        .participant_states
        .iter()
        .find(|s| s.role == "owner")
        .and_then(|s| s.leg_id.clone())
        .unwrap();
    let dial = fixture
        .calls()
        .into_iter()
        .find(|x| x.id == owner_leg)
        .unwrap();
    assert_eq!(dial.to, "+15555550160");
    assert_eq!(
        dial.idempotency_key,
        format!("owner-leg:{}", c.handoff_request_id.unwrap())
    );
    let owner_cs = cs(id, "owner");
    // Owner leg answered: joined to the conference; then the conference join: HUMAN_JOINED.
    deliver(
        &t,
        "h5",
        "call.answered",
        json!({"call_control_id": owner_leg, "client_state": owner_cs}),
    )
    .await;
    let conf = c.conference_id.clone().unwrap();
    assert_eq!(
        fixture.conferences()[&conf],
        vec![leg.clone(), owner_leg.clone()]
    );
    deliver(
        &t,
        "h6",
        "conference.participant.joined",
        json!({"call_control_id": owner_leg,
        "client_state": owner_cs, "conference_id": conf}),
    )
    .await;
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::HumanJoined);
    assert!(c
        .transcript
        .iter()
        .any(|x| x.text == "[owner joined the call]"));
    // An owner-role event from a leg that is not one of the call's legs: ignored.
    deliver(
        &t,
        "h7",
        "call.hangup",
        json!({"call_control_id": "someone-else", "client_state": owner_cs}),
    )
    .await;
    assert_eq!(load(&t, id).await.state, CallState::HumanJoined);
    // The owner leaves: the assistant resumes and (script done) closes the call.
    deliver(
        &t,
        "h8",
        "call.hangup",
        json!({"call_control_id": owner_leg, "client_state": owner_cs}),
    )
    .await;
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::AiResumed);
    assert!(c
        .transcript
        .iter()
        .any(|x| x.text == "[owner left the call]"));
    deliver(
        &t,
        "h9",
        "call.hangup",
        json!({"call_control_id": leg, "client_state": broker,
        "hangup_cause": "normal_clearing"}),
    )
    .await;
    assert_eq!(load(&t, id).await.state, CallState::Completed);
    let mut tx = t.app.store.begin().await.unwrap();
    let h = ss_server::store::comms::HANDOFFS
        .require(&mut *tx, c.handoff_request_id.unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(h.status, HandoffStatus::Accepted);
    let _ = HandoffReason::GuarantorRequested;
});

both_backends!(a_hangup_while_dialing_is_not_answered, |db| {
    let (t, _) = app_with_key(db, &[]);
    let (_, rejected) = dialing_call(&t, None).await;
    let leg = rejected.provider_call_id.clone().unwrap();
    deliver(
        &t,
        "d1",
        "call.hangup",
        json!({"call_control_id": leg,
        "client_state": cs(rejected.call_id, "counterparty"), "hangup_cause": "call_rejected"}),
    )
    .await;
    let c = load(&t, rejected.call_id).await;
    assert_eq!(c.state, CallState::Declined);
    assert!(c.ended_at.is_some());
    assert!(c.failure_reason.is_none());
    let (_, busy) = dialing_call(&t, None).await;
    let leg = busy.provider_call_id.clone().unwrap();
    deliver(
        &t,
        "d2",
        "call.hangup",
        json!({"call_control_id": leg, "hangup_cause": "user_busy"}),
    )
    .await;
    let c = load(&t, busy.call_id).await;
    assert_eq!(c.state, CallState::Failed);
    assert_eq!(
        c.failure_reason.as_deref(),
        Some("not answered (user_busy)")
    );
});

// ---- the live client, against a mock server only -------------------------------------------

fn telnyx_mock() -> MockServer {
    MockServer::start_with(|req: &Recorded| {
        let data = match req.path.as_str() {
            "/v2/calls" => json!({"data": {"call_control_id": "v3:abc"}}),
            "/v2/conferences" => json!({"data": {"id": "conf1"}}),
            _ => json!({"data": {"result": "ok"}}),
        };
        Canned {
            status: 200,
            content_type: Some("application/json".into()),
            body: serde_json::to_vec(&data).unwrap(),
        }
    })
}

#[test]
fn live_telnyx_request_shapes() {
    let server = telnyx_mock();
    let base = format!("{}/v2", server.base);
    let tx = LiveTelnyxProvider::with_base_url("KEY", "conn1", true, &base);
    let res = tx
        .dial(
            "+15555550100",
            "+15555550111",
            "https://x/webhooks/telnyx",
            "idem",
            Some(&client_state("c1", "counterparty")),
        )
        .unwrap();
    assert_eq!(res.call_control_id, "v3:abc");
    assert_eq!(res.state, "initiated");
    tx.speak("v3:abc", "hello").unwrap();
    tx.play("v3:abc", "https://x/a.mp3").unwrap();
    tx.start_transcription("v3:abc").unwrap();
    assert_eq!(tx.create_conference("v3:abc", "call-1").unwrap(), "conf1");
    tx.join_conference("conf1", "v3:owner").unwrap();
    tx.hangup("v3:abc").unwrap();
    let reqs = server.requests();
    let paths: Vec<&str> = reqs.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/v2/calls",
            "/v2/calls/v3:abc/actions/speak",
            "/v2/calls/v3:abc/actions/playback_start",
            "/v2/calls/v3:abc/actions/transcription_start",
            "/v2/conferences",
            "/v2/conferences/conf1/actions/join",
            "/v2/calls/v3:abc/actions/hangup",
        ]
    );
    assert!(reqs.iter().all(|r| r.method == "POST"
        && r.headers.get("authorization").map(String::as_str) == Some("Bearer KEY")));
    let body = |i: usize| serde_json::from_slice::<Value>(&reqs[i].body).unwrap();
    let dial = body(0);
    assert_eq!(dial["connection_id"], "conn1");
    assert_eq!(dial["answering_machine_detection"], "detect");
    assert_eq!(dial["timeout_secs"], 40);
    assert_eq!(dial["to"], "+15555550100");
    assert_eq!(dial["from"], "+15555550111");
    assert_eq!(dial["webhook_url"], "https://x/webhooks/telnyx");
    let decoded = decode_client_state(dial["client_state"].as_str());
    assert_eq!(
        serde_json::to_value(&decoded).unwrap(),
        json!({"call_id": "c1", "role": "counterparty"})
    );
    assert_eq!(
        reqs[0].headers.get("idempotency-key").map(String::as_str),
        Some("idem")
    );
    assert!(reqs[1..]
        .iter()
        .all(|r| !r.headers.contains_key("idempotency-key")));
    assert_eq!(
        body(1),
        json!({"payload": "hello", "voice": "female", "language": "en-US"})
    );
    assert_eq!(body(2), json!({"audio_url": "https://x/a.mp3"}));
    assert_eq!(body(3), json!({"transcription_engine": "Telnyx"}));
    assert_eq!(
        body(4),
        json!({"call_control_id": "v3:abc", "name": "call-1"})
    );
    assert_eq!(body(5), json!({"call_control_id": "v3:owner"}));
    assert_eq!(body(6), json!({}));
    // Outbound disabled: refused before any request.
    let blocked = LiveTelnyxProvider::with_base_url("KEY", "conn1", false, &base);
    let n = server.requests().len();
    match blocked.dial("+1", "+1", "u", "k", None) {
        Err(TelephonyError::Forbidden(m)) => assert!(m.contains("disabled")),
        other => panic!("{other:?}"),
    }
    assert_eq!(server.requests().len(), n);
    // Status entries.
    let st = serde_json::to_value(tx.status()).unwrap();
    assert_eq!(st["status"], "PARTIAL");
    assert_eq!(st["credentials_present"], true);
    let st = serde_json::to_value(blocked.status()).unwrap();
    assert!(st["note"]
        .as_str()
        .unwrap()
        .ends_with("; live outbound disabled."));
    let st =
        serde_json::to_value(LiveTelnyxProvider::with_base_url("KEY", "", true, &base).status())
            .unwrap();
    assert_eq!(st["credentials_present"], false);
}

#[test]
fn live_telnyx_errors_are_provider_errors_without_retry() {
    let server = MockServer::start_with(|req: &Recorded| match req.path.as_str() {
        "/v2/calls" => Canned {
            status: 503,
            content_type: Some("application/json".into()),
            body: b"{}".to_vec(),
        },
        "/v2/conferences" => Canned {
            status: 200,
            content_type: Some("application/json".into()),
            body: br#"{"data": {}}"#.to_vec(),
        },
        _ => Canned {
            status: 401,
            content_type: None,
            body: Vec::new(),
        },
    });
    let tx =
        LiveTelnyxProvider::with_base_url("KEY", "conn1", true, &format!("{}/v2", server.base));
    match tx.dial("+1", "+1", "u", "k", None) {
        Err(TelephonyError::Provider(e)) => {
            assert_eq!(e.kind, ProviderErrorKind::Unavailable);
            assert_eq!(e.message, "HTTP 503");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        server.requests().len(),
        1,
        "one attempt, as Python's RetryPolicy(attempts=1)"
    );
    match tx.create_conference("v3:abc", "x") {
        Err(TelephonyError::Provider(e)) => assert_eq!(e.kind, ProviderErrorKind::BadResponse),
        other => panic!("{other:?}"),
    }
    match tx.speak("v3/../x?y", "hi") {
        Err(TelephonyError::Provider(e)) => assert_eq!(e.kind, ProviderErrorKind::Auth),
        other => panic!("{other:?}"),
    }
    // The carrier-supplied id cannot change the path.
    assert_eq!(
        server.requests().last().unwrap().path,
        "/v2/calls/v3%2F..%2Fx%3Fy/actions/speak"
    );
    // No key in any error.
    assert!(!format!("{:?}", tx).contains("KEY"));
}

// The orchestrator with the live client (mock carrier): the approval gate holds, a carrier
// "accepted" answer is only DIALING, and outbound disabled is a 403 with nothing stored.
both_backends!(live_client_through_the_orchestrator, |db| {
    let server = telnyx_mock();
    let base = format!("{}/v2", server.base);
    let t = TestApp::new(db, &[("TELNYX_FROM_NUMBER", "+15555550111")]);
    t.app.extensions.insert(Arc::new(Voice::live(Arc::new(
        LiveTelnyxProvider::with_base_url("KEY", "conn1", true, &base),
    ))));
    // dialing_call uses the orchestrator with whatever provider is installed.
    let (_, call) = dialing_call(&t, None).await;
    assert_eq!(call.state, CallState::Dialing);
    assert_eq!(call.provider_call_id.as_deref(), Some("v3:abc"));
    assert!(!call.simulated);
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    let dial: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(dial["webhook_url"], "http://localhost:8000/webhooks/telnyx");
    assert_eq!(dial["from"], "+15555550111");
    assert_eq!(
        reqs[0].headers.get("idempotency-key").cloned(),
        Some(format!("call:{}", call.call_id))
    );
    let decoded = decode_client_state(dial["client_state"].as_str());
    assert_eq!(decoded["call_id"], call.call_id.to_string());
    assert_eq!(decoded["role"], "counterparty");
    let mut tx = t.app.store.begin().await.unwrap();
    assert_eq!(
        APPROVALS
            .require(&mut *tx, call.approval_id)
            .await
            .unwrap()
            .approval_status,
        ApprovalStatus::Executed
    );
    tx.commit().await.unwrap();

    // Outbound calls disabled: 403, nothing stored, the approval stays APPROVED.
    let t2 = TestApp::new(db, &[]);
    t2.app.extensions.insert(Arc::new(Voice::live(Arc::new(
        LiveTelnyxProvider::with_base_url("KEY", "conn1", false, &base),
    ))));
    let before = server.requests().len();
    let owner = Uuid::new_v4();
    let p = t2
        .create_project(
            &t2.token_for(owner),
            &json!({"project_name": "t", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    let pid: Uuid = p["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut recipient = IndexMap::new();
    recipient.insert("phone".to_owned(), "+15555550100".to_owned());
    let mut tx = t2.app.store.begin().await.unwrap();
    let a = t2
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
                format!("c-{}", Uuid::new_v4()),
            ),
        )
        .await
        .unwrap();
    t2.app
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
    let mut tx = t2.app.store.begin().await.unwrap();
    let mut orch = CallOrchestrator::new(&t2.app);
    let err = orch
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
                simulated: false,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.status().as_u16(), 403);
    drop(tx);
    assert_eq!(server.requests().len(), before);
    let mut tx = t2.app.store.begin().await.unwrap();
    assert!(CALLS
        .list(&mut *tx, Query::new().eq("project_id", pid))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        APPROVALS
            .require(&mut *tx, a.approval_id)
            .await
            .unwrap()
            .approval_status,
        ApprovalStatus::Approved
    );
    tx.commit().await.unwrap();
});

// ---- added by the independent review (rust-server-4-review F1, F2, F4) ----------------------

/// A carrier whose dial fails with a fixed provider error, counting the dial attempts.
struct FailingDial {
    dials: std::sync::atomic::AtomicUsize,
    kind: ProviderErrorKind,
    message: &'static str,
}

impl TelephonyProvider for FailingDial {
    fn mode(&self) -> &str {
        "live"
    }
    fn dial(
        &self,
        _to: &str,
        _from: &str,
        _webhook_url: &str,
        _key: &str,
        _cs: Option<&IndexMap<String, Value>>,
    ) -> Result<ss_domain::integrations::telnyx::DialResult, TelephonyError> {
        self.dials.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(TelephonyError::Provider(
            ss_domain::integrations::base::ProviderError::new("telnyx", self.kind, self.message),
        ))
    }
    fn speak(&self, _: &str, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn play(&self, _: &str, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn start_transcription(&self, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn hangup(&self, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn create_conference(&self, _: &str, _: &str) -> Result<String, TelephonyError> {
        Ok("conf".into())
    }
    fn join_conference(&self, _: &str, _: &str) -> Result<(), TelephonyError> {
        Ok(())
    }
    fn status(&self) -> ss_domain::contracts::ProviderStatusEntry {
        ss_domain::integrations::base::status_entry(
            "telnyx",
            "live",
            ss_core::IntegrationStatus::Partial,
            true,
            "test carrier",
        )
    }
}

// A dial the carrier did not confirm (no answer, a 5xx, an unreadable 2xx) may have placed the
// call: the approval is spent and the record says the outcome is unknown, so a second start for
// the same approval dials nothing. A refusal the carrier stated leaves the approval APPROVED.
both_backends!(
    a_dial_the_carrier_did_not_confirm_spends_the_approval,
    |db| {
        for (kind, message, unknown) in [
            (ProviderErrorKind::Timeout, "timed out", true),
            (ProviderErrorKind::Unavailable, "HTTP 503", true),
            (
                ProviderErrorKind::Unavailable,
                "Network Error: connection reset",
                true,
            ),
            (
                ProviderErrorKind::BadResponse,
                "response without data.call_control_id",
                true,
            ),
            (ProviderErrorKind::BadResponse, "HTTP 422", false),
            (
                ProviderErrorKind::Unavailable,
                "Connection Failed: refused",
                false,
            ),
            (ProviderErrorKind::RateLimited, "HTTP 429", false),
        ] {
            let t = TestApp::new(db, &[]);
            let carrier = Arc::new(FailingDial {
                dials: std::sync::atomic::AtomicUsize::new(0),
                kind,
                message,
            });
            t.app
                .extensions
                .insert(Arc::new(Voice::live(carrier.clone())));
            let (owner, call) = dialing_call(&t, None).await;
            assert_eq!(call.state, CallState::Failed, "{message}");
            let reason = call.failure_reason.clone().unwrap();
            assert_eq!(
                reason.contains("outcome unknown"),
                unknown,
                "{message}: {reason}"
            );
            assert!(reason.contains(message), "{reason}");
            let mut tx = t.app.store.begin().await.unwrap();
            let status = APPROVALS
                .require(&mut *tx, call.approval_id)
                .await
                .unwrap()
                .approval_status;
            tx.commit().await.unwrap();
            let want = if unknown {
                ApprovalStatus::Executed
            } else {
                ApprovalStatus::Approved
            };
            assert_eq!(status, want, "{message}");
            let mut tx = t.app.store.begin().await.unwrap();
            let again = CallOrchestrator::new(&t.app)
                .start(
                    &mut *tx,
                    StartCall {
                        project_id: call.project_id,
                        owner_id: owner,
                        recipient: call.recipient.clone(),
                        property_id: None,
                        approval_id: Some(call.approval_id),
                        intro_recording_reference: None,
                        communication_id: None,
                        simulated: false,
                    },
                )
                .await;
            drop(tx);
            if unknown {
                assert_eq!(
                    again.unwrap_err().error_code(),
                    "APPROVAL_REQUIRED",
                    "{message}"
                );
                assert_eq!(
                    carrier.dials.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "{message}"
                );
            } else {
                assert!(again.is_ok(), "{message}");
                assert_eq!(
                    carrier.dials.load(std::sync::atomic::Ordering::SeqCst),
                    2,
                    "{message}"
                );
            }
        }
    }
);

// Live outbound calls disabled: answering JOIN is refused before any carrier command (no
// conference is created on the carrier), and nothing is stored.
both_backends!(owner_leg_with_outbound_disabled_sends_nothing, |db| {
    let (t, _) = app_with_key(db, &[]);
    let first = telnyx_mock();
    t.app.extensions.insert(Arc::new(Voice::live(Arc::new(
        LiveTelnyxProvider::with_base_url("KEY", "conn1", true, &format!("{}/v2", first.base)),
    ))));
    let (owner, call) = dialing_call(&t, Some("+15555550142")).await;
    let leg = call.provider_call_id.clone().unwrap();
    let mine = cs(call.call_id, "counterparty");
    for (i, (et, extra)) in [
        ("call.answered", json!({})),
        ("call.speak.ended", json!({})),
        (
            "call.transcription",
            json!({"transcription_data": {"transcript": "Will you need a guarantor?", "is_final": true}}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut p = json!({"call_control_id": leg, "client_state": mine});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        assert_eq!(deliver(&t, &format!("od{i}"), et, p).await, 204);
    }
    let pending = load(&t, call.call_id).await;
    assert_eq!(pending.state, CallState::HandoffPending);
    let second = telnyx_mock();
    t.app.extensions.insert(Arc::new(Voice::live(Arc::new(
        LiveTelnyxProvider::with_base_url("KEY", "conn1", false, &format!("{}/v2", second.base)),
    ))));
    let token = t.token_for(owner);
    let hid = pending.handoff_request_id.unwrap();
    let r = t
        .post(
            &format!(
                "/api/v1/projects/{}/handoffs/{hid}/response",
                call.project_id
            ),
            Some(&token),
            &json!({"response": "ACCEPTED"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 403, "{}", r.text);
    assert_eq!(r.body["error_code"], "FORBIDDEN");
    assert!(second.requests().is_empty(), "{:?}", second.requests());
    let after = load(&t, call.call_id).await;
    assert_eq!(after.state, CallState::HandoffPending);
    assert_eq!(after.conference_id, None);
});

// A replayed event id whose handling has a visible effect (a broker answer) is processed once.
both_backends!(a_replayed_transcription_is_processed_once, |db| {
    let (t, _) = app_with_key(db, &[]);
    let (_, call) = dialing_call(&t, None).await;
    let leg = call.provider_call_id.clone().unwrap();
    let mine = cs(call.call_id, "counterparty");
    deliver(
        &t,
        "rp1",
        "call.answered",
        json!({"call_control_id": leg, "client_state": mine}),
    )
    .await;
    deliver(
        &t,
        "rp2",
        "call.speak.ended",
        json!({"call_control_id": leg, "client_state": mine}),
    )
    .await;
    let utter = json!({"call_control_id": leg,
                       "transcription_data": {"transcript": "It's available December first.", "is_final": true}});
    for _ in 0..3 {
        assert_eq!(
            deliver(&t, "rp3", "call.transcription", utter.clone()).await,
            204
        );
    }
    let c = load(&t, call.call_id).await;
    let heard = c
        .transcript
        .iter()
        .filter(|x| x.speaker == "counterparty")
        .count();
    assert_eq!(heard, 1);
    assert_eq!(c.findings.len(), 1);
    let rows = processed(&t).await;
    assert_eq!(
        rows.iter()
            .filter(|r| matches!(r.get("provider_event_id"), Some(ss_server::store::SqlValue::Text(s)) if s == "rp3"))
            .count(),
        1
    );
});

// ---- media-stream commands (telnyx-stream-1): mock server and fixture only -------------------

mod stream {
    use super::*;
    use ss_domain::integrations::telnyx::{
        Bidirectional, FixtureTelephonyProvider, StreamCodec, StreamEvent, StreamOptions,
        StreamTargetLegs, StreamTrack, TelephonyCommand,
    };

    fn opts() -> StreamOptions {
        StreamOptions::voice_agent("wss://x/voice/stream")
    }

    /// The exact bytes of every request body, for byte-level comparisons.
    fn raw(server: &MockServer) -> Vec<(String, String, String)> {
        server
            .requests()
            .iter()
            .map(|r| {
                (
                    r.method.clone(),
                    r.path.clone(),
                    String::from_utf8(r.body.clone()).unwrap(),
                )
            })
            .collect()
    }

    /// The bodies the commands that existed before this change send, byte for byte (the same
    /// bytes as at HEAD: `serde_json` without `preserve_order` writes keys sorted).
    #[test]
    fn existing_command_bodies_are_byte_identical() {
        let server = telnyx_mock();
        let tx =
            LiveTelnyxProvider::with_base_url("KEY", "conn1", true, &format!("{}/v2", server.base));
        tx.dial(
            "+15555550100",
            "+15555550111",
            "https://x/w",
            "idem",
            Some(&client_state("c1", "counterparty")),
        )
        .unwrap();
        tx.dial("+15555550100", "+15555550111", "https://x/w", "idem2", None)
            .unwrap();
        tx.speak("v3:abc", "hello").unwrap();
        tx.play("v3:abc", "https://x/a.mp3").unwrap();
        tx.start_transcription("v3:abc").unwrap();
        tx.hangup("v3:abc").unwrap();
        let cs = encode_client_state(&client_state("c1", "counterparty"));
        let bodies: Vec<String> = raw(&server).into_iter().map(|(_, _, b)| b).collect();
        assert_eq!(
            bodies,
            [
                format!(
                    r#"{{"answering_machine_detection":"detect","client_state":"{cs}","connection_id":"conn1","from":"+15555550111","timeout_secs":40,"to":"+15555550100","webhook_url":"https://x/w"}}"#
                ),
                r#"{"answering_machine_detection":"detect","connection_id":"conn1","from":"+15555550111","timeout_secs":40,"to":"+15555550100","webhook_url":"https://x/w"}"#.to_owned(),
                r#"{"language":"en-US","payload":"hello","voice":"female"}"#.to_owned(),
                r#"{"audio_url":"https://x/a.mp3"}"#.to_owned(),
                r#"{"transcription_engine":"Telnyx"}"#.to_owned(),
                "{}".to_owned(),
            ]
        );
    }

    /// Each new command's method, path, headers and JSON body.
    /// Dial: https://developers.telnyx.com/api-reference/call-commands/dial
    /// Streaming start: https://developers.telnyx.com/api-reference/call-commands/streaming-start
    /// Streaming stop: https://developers.telnyx.com/api-reference/call-commands/streaming-stop
    /// Transcription stop: https://developers.telnyx.com/api-reference/call-commands/transcription-stop
    /// Stop audio playback: https://developers.telnyx.com/api-reference/call-commands/stop-audio-playback
    /// Bidirectional RTP: https://developers.telnyx.com/docs/voice/programmable-voice/media-streaming
    #[test]
    fn stream_command_request_shapes() {
        let server = telnyx_mock();
        let tx =
            LiveTelnyxProvider::with_base_url("KEY", "conn1", true, &format!("{}/v2", server.base));
        let res = tx
            .dial_streaming(
                "+15555550100",
                "+15555550111",
                "https://x/w",
                "idem",
                Some(&client_state("c1", "counterparty")),
                &opts(),
            )
            .unwrap();
        assert_eq!(res.call_control_id, "v3:abc");
        assert_eq!(res.state, "initiated");
        tx.start_streaming("v3:abc", &opts()).unwrap();
        let full = StreamOptions {
            stream_url: "wss://x/s".into(),
            track: StreamTrack::Both,
            stream_codec: None,
            bidirectional: Some(Bidirectional {
                codec: StreamCodec::L16,
                target_legs: Some(StreamTargetLegs::Own),
                sampling_rate: Some(16000),
            }),
            auth_token: Some("tok".into()),
        };
        tx.start_streaming("v3:a/b", &full).unwrap();
        let one_way = StreamOptions {
            stream_url: "wss://x/s".into(),
            track: StreamTrack::Outbound,
            stream_codec: Some(StreamCodec::AmrWb),
            bidirectional: None,
            auth_token: None,
        };
        tx.start_streaming("v3:abc", &one_way).unwrap();
        tx.stop_streaming("v3:abc", None).unwrap();
        tx.stop_streaming("v3:abc", Some("32de0dea-53cb-4b21-89a4-9e1819c043bc"))
            .unwrap();
        tx.stop_transcription("v3:abc").unwrap();
        tx.stop_playback("v3:abc").unwrap();
        let reqs = server.requests();
        let got: Vec<(&str, &str)> = reqs
            .iter()
            .map(|r| (r.method.as_str(), r.path.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("POST", "/v2/calls"),
                ("POST", "/v2/calls/v3:abc/actions/streaming_start"),
                ("POST", "/v2/calls/v3:a%2Fb/actions/streaming_start"),
                ("POST", "/v2/calls/v3:abc/actions/streaming_start"),
                ("POST", "/v2/calls/v3:abc/actions/streaming_stop"),
                ("POST", "/v2/calls/v3:abc/actions/streaming_stop"),
                ("POST", "/v2/calls/v3:abc/actions/transcription_stop"),
                ("POST", "/v2/calls/v3:abc/actions/playback_stop"),
            ]
        );
        assert!(reqs
            .iter()
            .all(|r| r.headers.get("authorization").map(String::as_str) == Some("Bearer KEY")));
        assert_eq!(
            reqs[0].headers.get("idempotency-key").map(String::as_str),
            Some("idem")
        );
        assert!(reqs[1..]
            .iter()
            .all(|r| !r.headers.contains_key("idempotency-key")));
        let body = |i: usize| serde_json::from_slice::<Value>(&reqs[i].body).unwrap();
        assert_eq!(
            body(0),
            json!({
                "connection_id": "conn1", "to": "+15555550100", "from": "+15555550111",
                "webhook_url": "https://x/w", "answering_machine_detection": "detect",
                "timeout_secs": 40,
                "client_state": encode_client_state(&client_state("c1", "counterparty")),
                "stream_url": "wss://x/voice/stream", "stream_track": "inbound_track",
                "stream_codec": "PCMU", "stream_bidirectional_mode": "rtp",
                "stream_bidirectional_codec": "PCMU",
            })
        );
        let agent = json!({
            "stream_url": "wss://x/voice/stream", "stream_track": "inbound_track",
            "stream_codec": "PCMU", "stream_bidirectional_mode": "rtp",
            "stream_bidirectional_codec": "PCMU",
        });
        assert_eq!(body(1), agent);
        assert_eq!(
            body(2),
            json!({
                "stream_url": "wss://x/s", "stream_track": "both_tracks",
                "stream_bidirectional_mode": "rtp", "stream_bidirectional_codec": "L16",
                "stream_bidirectional_target_legs": "self",
                "stream_bidirectional_sampling_rate": 16000, "stream_auth_token": "tok",
            })
        );
        assert_eq!(
            body(3),
            json!({"stream_url": "wss://x/s", "stream_track": "outbound_track", "stream_codec": "AMR-WB"})
        );
        assert_eq!(body(4), json!({}));
        assert_eq!(
            body(5),
            json!({"stream_id": "32de0dea-53cb-4b21-89a4-9e1819c043bc"})
        );
        assert_eq!(body(6), json!({}));
        assert_eq!(body(7), json!({"stop": "all"}));
        // The token never shows in Debug output.
        assert!(!format!("{full:?}").contains("tok\""));
        // dial_streaming is refused like dial when live outbound calls are disabled.
        let blocked = LiveTelnyxProvider::with_base_url(
            "KEY",
            "conn1",
            false,
            &format!("{}/v2", server.base),
        );
        let n = server.requests().len();
        match blocked.dial_streaming("+1", "+1", "u", "k", None, &opts()) {
            Err(TelephonyError::Forbidden(m)) => assert!(m.contains("disabled")),
            other => panic!("{other:?}"),
        }
        assert_eq!(server.requests().len(), n);
    }

    #[test]
    fn stream_command_errors() {
        // A refusal: Telnyx's 422 (for example "call has already ended").
        let server = MockServer::start_with(|_req: &Recorded| Canned {
            status: 422,
            content_type: Some("application/json".into()),
            body: br#"{"errors":[{"code":"90018","title":"Call has already ended"}]}"#.to_vec(),
        });
        let tx =
            LiveTelnyxProvider::with_base_url("KEY", "conn1", true, &format!("{}/v2", server.base));
        for r in [
            tx.start_streaming("v3:abc", &opts()),
            tx.stop_streaming("v3:abc", None),
            tx.stop_transcription("v3:abc"),
            tx.stop_playback("v3:abc"),
        ] {
            match r {
                Err(TelephonyError::Provider(e)) => {
                    assert_eq!(e.kind, ProviderErrorKind::BadResponse);
                    assert_eq!(e.message, "HTTP 422");
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(server.requests().len(), 4, "one attempt each, no retry");
        // A transport failure: nothing listens on the port.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let tx = LiveTelnyxProvider::with_base_url(
            "KEY",
            "conn1",
            true,
            &format!("http://127.0.0.1:{closed}/v2"),
        );
        match tx.start_streaming("v3:abc", &opts()) {
            Err(TelephonyError::Provider(e)) => {
                assert_eq!(e.provider, "telnyx");
                assert!(
                    matches!(
                        e.kind,
                        ProviderErrorKind::Unavailable | ProviderErrorKind::Timeout
                    ),
                    "{e:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn fixture_records_stream_commands() {
        let f = FixtureTelephonyProvider::new();
        let d = f
            .dial_streaming("+15555550100", "+1", "u", "k1", None, &opts())
            .unwrap();
        assert_eq!(
            f.dial_streaming("+15555550100", "+1", "u", "k1", None, &opts())
                .unwrap(),
            d
        );
        f.dial("+15555550101", "+1", "u", "k2", None).unwrap();
        let calls = f.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].stream, Some(opts()));
        assert_eq!(calls[1].stream, None);
        let leg = d.call_control_id.as_str();
        f.start_streaming(leg, &opts()).unwrap();
        f.stop_streaming(leg, Some("s1")).unwrap();
        f.stop_streaming(leg, None).unwrap();
        f.stop_transcription(leg).unwrap();
        f.stop_playback(leg).unwrap();
        let cmds = f.commands();
        assert_eq!(
            cmds,
            [
                TelephonyCommand::StreamingStart {
                    leg: leg.into(),
                    stream: opts()
                },
                TelephonyCommand::StreamingStop {
                    leg: leg.into(),
                    stream_id: Some("s1".into())
                },
                TelephonyCommand::StreamingStop {
                    leg: leg.into(),
                    stream_id: None
                },
                TelephonyCommand::TranscriptionStop { leg: leg.into() },
                TelephonyCommand::PlaybackStop { leg: leg.into() },
            ]
        );
        let json: Vec<Value> = cmds.iter().map(TelephonyCommand::to_json).collect();
        assert_eq!(
            json,
            [
                json!({"cmd": "streaming_start", "leg": leg, "stream_url": "wss://x/voice/stream", "stream_track": "inbound_track"}),
                json!({"cmd": "streaming_stop", "leg": leg, "stream_id": "s1"}),
                json!({"cmd": "streaming_stop", "leg": leg}),
                json!({"cmd": "transcription_stop", "leg": leg}),
                json!({"cmd": "playback_stop", "leg": leg}),
            ]
        );
        // A provider written before these commands (here the test double `FailingDial`) refuses
        // them without doing anything.
        let old = FailingDial {
            dials: Default::default(),
            kind: ProviderErrorKind::Unavailable,
            message: "x",
        };
        match old.start_streaming("l", &opts()) {
            Err(TelephonyError::Provider(e)) => {
                assert_eq!(e.kind, ProviderErrorKind::NotConfigured)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stream_event_names() {
        for (s, e) in [
            ("streaming.started", StreamEvent::Started),
            ("streaming.stopped", StreamEvent::Stopped),
            ("streaming.failed", StreamEvent::Failed),
        ] {
            assert_eq!(StreamEvent::parse(s), Some(e));
            assert_eq!(e.as_str(), s);
        }
        assert_eq!(StreamEvent::parse("call.answered"), None);
    }

    // The three stream webhooks change nothing today: no state, no transcript, no command, no
    // owner text, 204, in INTRO and in ACTIVE.
    both_backends!(stream_webhooks_are_ignored_by_the_call_flow, |db| {
        let (t, fake) = app_with_key(db, &[]);
        let fixture = t.app.voice().fixture.clone().unwrap();
        let (_, call) = dialing_call(&t, None).await;
        let leg = call.provider_call_id.clone().unwrap();
        let id = call.call_id;
        let mine = cs(id, "counterparty");
        let p = json!({"call_control_id": leg, "client_state": mine, "stream_url": "wss://x/s"});
        for (et, eid) in [("call.answered", "a1"), ("call.speak.ended", "a2")] {
            let before = serde_json::to_value(load(&t, id).await).unwrap();
            let commands = fixture.commands();
            let notices = fake.requests().len();
            for s in ["streaming.started", "streaming.stopped", "streaming.failed"] {
                assert_eq!(
                    deliver(&t, &format!("{eid}-{s}"), s, p.clone()).await,
                    204,
                    "{s}"
                );
            }
            assert_eq!(
                serde_json::to_value(load(&t, id).await).unwrap(),
                before,
                "{et}"
            );
            assert_eq!(fixture.commands(), commands);
            assert_eq!(fake.requests().len(), notices);
            assert_eq!(
                deliver(
                    &t,
                    eid,
                    et,
                    json!({"call_control_id": leg, "client_state": mine})
                )
                .await,
                204
            );
        }
        assert_eq!(load(&t, id).await.state, CallState::Active);
    });
}
