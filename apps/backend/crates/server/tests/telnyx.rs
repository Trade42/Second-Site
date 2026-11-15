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
    let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
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
    let body =
        serde_json::to_vec(&json!({"data": {"id": "tx-evt-1", "event_type": "call.answered", "payload": {}}}))
            .unwrap();
    let ts = now();
    assert_eq!(
        t.send(signed_request(&body, ts, &key())).await.status.as_u16(),
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
            .send(signed_request(&body, now(), &SigningKey::from_bytes(&[9u8; 32])))
            .await;
        assert_eq!(r.status.as_u16(), 400, "{mode}");
        assert_eq!(r.body["message"], "telnyx signature mismatch");
        // Garbage signature.
        let mut req = signed_request(&body, now(), &key());
        req.headers_mut()
            .insert("telnyx-signature-ed25519", "AAAA".parse().unwrap());
        assert_eq!(t.send(req).await.status.as_u16(), 400, "{mode}");
        // Stale timestamp (correctly signed).
        let r = t
            .send(signed_request(&body, now() - 3600, &key()))
            .await;
        assert_eq!(r.status.as_u16(), 400, "{mode}");
        assert_eq!(r.body["message"], "webhook timestamp outside tolerance");
        assert_eq!(processed(&t).await.len(), base, "{mode}: nothing stored");
        // A good one is stored with the mode's label.
        assert_eq!(
            t.send(signed_request(&body, now(), &key())).await.status.as_u16(),
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
        deliver(&t, "e1", "call.speak.ended", json!({"call_control_id": leg, "client_state": mine})).await,
        204
    );
    assert_eq!(load(&t, id).await.state, CallState::Dialing);
    // Answered (the carrier's own event): INTRO, intro spoken.
    assert_eq!(
        deliver(&t, "e2", "call.answered", json!({"call_control_id": leg, "client_state": mine})).await,
        204
    );
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::Intro);
    let transcript = c.transcript.len();
    let commands = fixture.commands().len();
    // Replay of the same event id: nothing happens.
    assert_eq!(
        deliver(&t, "e2", "call.answered", json!({"call_control_id": leg, "client_state": mine})).await,
        204
    );
    // A second answer with a new id (carrier duplicate): ignored outside DIALING.
    assert_eq!(
        deliver(&t, "e3", "call.answered", json!({"call_control_id": leg, "client_state": mine})).await,
        204
    );
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::Intro);
    assert_eq!(c.transcript.len(), transcript);
    assert_eq!(fixture.commands().len(), commands);
    // Intro done: questions start; transcription is started on the broker leg.
    deliver(&t, "e4", "call.speak.ended", json!({"call_control_id": leg, "client_state": mine})).await;
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
        deliver(&t, "e6", "call.transcription", json!({"call_control_id": leg, "client_state": "!!!",
            "transcription_data": {"transcript": "  ", "is_final": true}})).await,
        204
    );
    assert_eq!(
        deliver(&t, "e7", "call.transcription", json!({"call_control_id": leg, "client_state": 5,
            "transcription_data": {"transcript": "x", "is_final": false}})).await,
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
    deliver(&t, "e9", "call.hangup", json!({"call_control_id": leg, "client_state": mine,
        "hangup_cause": "normal_clearing"})).await;
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::Completed);
    assert!(c.ended_at.is_some());
    assert!(c.outcome_summary.as_deref().unwrap().contains("available_from"));
    t.app.owner_notify().idle().await;
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].purpose, "call_summary");
    assert_eq!(reqs[0].idempotency_key, format!("call-summary:{id}"));
    assert!(!reqs[0].text.starts_with("[Simulated call]"));
    // After the end, later events change nothing.
    deliver(&t, "e10", "call.answered", json!({"call_control_id": leg, "client_state": mine})).await;
    assert_eq!(load(&t, id).await, c);
    // An event of an unknown call is recorded and ignored.
    assert_eq!(
        deliver(&t, "e11", "call.answered", json!({"call_control_id": "nope"})).await,
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
    deliver(&t, "h1", "call.answered", json!({"call_control_id": leg, "client_state": broker})).await;
    deliver(&t, "h2", "call.speak.ended", json!({"call_control_id": leg, "client_state": broker})).await;
    // Escalation on the broker's utterance.
    deliver(&t, "h3", "call.transcription", json!({"call_control_id": leg,
        "transcription_data": {"transcript": "Will you need a guarantor?", "is_final": true}})).await;
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::HandoffPending);
    // While the handoff waits, the assistant stays quiet and records nothing.
    let said = c.transcript.iter().filter(|x| x.speaker == "assistant").count();
    deliver(&t, "h4", "call.transcription", json!({"call_control_id": leg,
        "transcription_data": {"transcript": "We also need first month and deposit", "is_final": true}})).await;
    let c = load(&t, id).await;
    assert_eq!(
        c.transcript.iter().filter(|x| x.speaker == "assistant").count(),
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
    let dial = fixture.calls().into_iter().find(|x| x.id == owner_leg).unwrap();
    assert_eq!(dial.to, "+15555550160");
    assert_eq!(dial.idempotency_key, format!("owner-leg:{}", c.handoff_request_id.unwrap()));
    let owner_cs = cs(id, "owner");
    // Owner leg answered: joined to the conference; then the conference join: HUMAN_JOINED.
    deliver(&t, "h5", "call.answered", json!({"call_control_id": owner_leg, "client_state": owner_cs})).await;
    let conf = c.conference_id.clone().unwrap();
    assert_eq!(fixture.conferences()[&conf], vec![leg.clone(), owner_leg.clone()]);
    deliver(&t, "h6", "conference.participant.joined", json!({"call_control_id": owner_leg,
        "client_state": owner_cs, "conference_id": conf})).await;
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::HumanJoined);
    assert!(c.transcript.iter().any(|x| x.text == "[owner joined the call]"));
    // An owner-role event from a leg that is not one of the call's legs: ignored.
    deliver(&t, "h7", "call.hangup", json!({"call_control_id": "someone-else", "client_state": owner_cs})).await;
    assert_eq!(load(&t, id).await.state, CallState::HumanJoined);
    // The owner leaves: the assistant resumes and (script done) closes the call.
    deliver(&t, "h8", "call.hangup", json!({"call_control_id": owner_leg, "client_state": owner_cs})).await;
    let c = load(&t, id).await;
    assert_eq!(c.state, CallState::AiResumed);
    assert!(c.transcript.iter().any(|x| x.text == "[owner left the call]"));
    deliver(&t, "h9", "call.hangup", json!({"call_control_id": leg, "client_state": broker,
        "hangup_cause": "normal_clearing"})).await;
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
    deliver(&t, "d1", "call.hangup", json!({"call_control_id": leg,
        "client_state": cs(rejected.call_id, "counterparty"), "hangup_cause": "call_rejected"})).await;
    let c = load(&t, rejected.call_id).await;
    assert_eq!(c.state, CallState::Declined);
    assert!(c.ended_at.is_some());
    assert!(c.failure_reason.is_none());
    let (_, busy) = dialing_call(&t, None).await;
    let leg = busy.provider_call_id.clone().unwrap();
    deliver(&t, "d2", "call.hangup", json!({"call_control_id": leg, "hangup_cause": "user_busy"})).await;
    let c = load(&t, busy.call_id).await;
    assert_eq!(c.state, CallState::Failed);
    assert_eq!(c.failure_reason.as_deref(), Some("not answered (user_busy)"));
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
    assert_eq!(reqs[0].headers.get("idempotency-key").map(String::as_str), Some("idem"));
    assert!(reqs[1..].iter().all(|r| !r.headers.contains_key("idempotency-key")));
    assert_eq!(body(1), json!({"payload": "hello", "voice": "female", "language": "en-US"}));
    assert_eq!(body(2), json!({"audio_url": "https://x/a.mp3"}));
    assert_eq!(body(3), json!({"transcription_engine": "Telnyx"}));
    assert_eq!(body(4), json!({"call_control_id": "v3:abc", "name": "call-1"}));
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
    assert!(st["note"].as_str().unwrap().ends_with("; live outbound disabled."));
    let st = serde_json::to_value(LiveTelnyxProvider::with_base_url("KEY", "", true, &base).status())
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
    let tx = LiveTelnyxProvider::with_base_url("KEY", "conn1", true, &format!("{}/v2", server.base));
    match tx.dial("+1", "+1", "u", "k", None) {
        Err(TelephonyError::Provider(e)) => {
            assert_eq!(e.kind, ProviderErrorKind::Unavailable);
            assert_eq!(e.message, "HTTP 503");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(server.requests().len(), 1, "one attempt, as Python's RetryPolicy(attempts=1)");
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
    let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
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
        .decide(&mut *tx, a.approval_id, owner, ApprovalStatus::Approved, &a.action_payload_hash)
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
