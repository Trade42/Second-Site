//! REVIEW (temporary, part 4 independent review): the Telnyx webhook. Signature before parsing
//! and storing, verified flag, dedupe under a race, foreign and malformed client_state, and a
//! fuzz run of malformed bodies (no panic, no change to another owner's call).

mod common;

use std::str::FromStr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_domain::contracts::CallSession;
use ss_domain::integrations::telnyx::{client_state, encode_client_state};
use ss_server::contracts::{ActionType, ApprovalStatus};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::call_orchestration::{CallOrchestrator, StartCall};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::store::comms::{webhooks, CALLS};
use tower::ServiceExt;
use uuid::Uuid;

use common::TestApp;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn pk() -> String {
    base64::engine::general_purpose::STANDARD.encode(key().verifying_key().to_bytes())
}

fn req(body: &[u8], ts: Option<i64>, signer: Option<&SigningKey>) -> Request<Body> {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri("/webhooks/telnyx")
        .header("content-type", "application/json");
    if let Some(ts) = ts {
        b = b.header("telnyx-timestamp", ts.to_string());
        if let Some(s) = signer {
            let mut msg = format!("{ts}|").into_bytes();
            msg.extend_from_slice(body);
            let sig = base64::engine::general_purpose::STANDARD.encode(s.sign(&msg).to_bytes());
            b = b.header("telnyx-signature-ed25519", sig);
        }
    }
    b.body(Body::from(body.to_vec())).unwrap()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn app(db: &common::TestDb, extra: &[(&str, &str)]) -> TestApp {
    let k = pk();
    let mut vars: Vec<(&str, &str)> = vec![("TELNYX_PUBLIC_KEY", k.as_str())];
    vars.extend_from_slice(extra);
    let t = TestApp::new(db, &vars);
    t.app
        .extensions
        .insert(Arc::new(OwnerNotify::new(Arc::new(RecordingNotifier::new()))));
    t
}

/// A DIALING call of a fresh owner.
async fn dialing(t: &TestApp) -> CallSession {
    let owner = Uuid::new_v4();
    let p = t
        .create_project(
            &t.token_for(owner),
            &json!({"project_name": "t", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
    let mut r = IndexMap::new();
    r.insert("name".to_owned(), "B".to_owned());
    r.insert("phone".to_owned(), "+15555550100".to_owned());
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
                ss_domain::communications::call_payload(pid, &r, None),
                "call",
                format!("w-{}", Uuid::new_v4()),
            ),
        )
        .await
        .unwrap();
    t.app
        .approvals()
        .decide(&mut *tx, a.approval_id, owner, ApprovalStatus::Approved, &a.action_payload_hash)
        .await
        .unwrap();
    let call = CallOrchestrator::new(&t.app)
        .start(
            &mut *tx,
            StartCall {
                project_id: pid,
                owner_id: owner,
                recipient: r,
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
    call
}

async fn load(t: &TestApp, id: Uuid) -> CallSession {
    let mut tx = t.app.store.begin().await.unwrap();
    let c = CALLS.require(&mut *tx, id).await.unwrap();
    tx.commit().await.unwrap();
    c
}

async fn rows(t: &TestApp) -> Vec<ss_server::store::Row> {
    let mut tx = t.app.store.begin().await.unwrap();
    let r = webhooks::rows(&mut *tx, "telnyx").await.unwrap();
    tx.commit().await.unwrap();
    r
}

fn ev(id: &str, et: &str, payload: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"data": {"record_type": "event", "id": id, "event_type": et, "payload": payload}}))
        .unwrap()
}

/// Comparable view of a call (no timestamps).
fn view(c: &CallSession) -> Value {
    let mut v = serde_json::to_value(c).unwrap();
    if let Some(m) = v.as_object_mut() {
        m.remove("updated_at");
    }
    v
}

both_backends!(signature_mode_and_verified_flag, |db| {
    // Fixture mode: accepted only when signed by the configured key; stored as not verified.
    let t = app(db, &[]);
    let c = dialing(&t).await;
    let leg = c.provider_call_id.clone().unwrap();
    let body = ev("e-fix-1", "call.answered", json!({"call_control_id": leg}));
    let bad = SigningKey::from_bytes(&[9u8; 32]);
    let mut seen = Vec::new();
    for (label, r) in [
        ("no headers", req(&body, None, None)),
        ("timestamp only", req(&body, Some(now()), None)),
        ("other key", req(&body, Some(now()), Some(&bad))),
        ("stale 301s", req(&body, Some(now() - 301), Some(&key()))),
        ("future 301s", req(&body, Some(now() + 301), Some(&key()))),
    ] {
        let s = t.send(r).await;
        seen.push(format!("{label}={}", s.status.as_u16()));
        assert_eq!(s.status.as_u16(), 400, "{label}: {}", s.text);
        assert_eq!(rows(&t).await.len(), 0, "{label}: stored");
        assert_eq!(load(&t, c.call_id).await.state.as_str(), "DIALING", "{label}: processed");
    }
    // A body tampered after signing.
    let mut r = req(&body, Some(now()), Some(&key()));
    *r.body_mut() = Body::from(ev("e-fix-1", "call.hangup", json!({"call_control_id": leg})));
    let s = t.send(r).await;
    seen.push(format!("tampered={}", s.status.as_u16()));
    assert_eq!(s.status.as_u16(), 400);
    assert_eq!(rows(&t).await.len(), 0);
    let s = t.send(req(&body, Some(now()), Some(&key()))).await;
    assert_eq!(s.status.as_u16(), 204);
    let r = rows(&t).await;
    println!("REVIEW fixture mode refusals {seen:?}; accepted row verified={} payload.mode={}", r[0].get("verified").map(|v| format!("{v:?}")).unwrap_or_default(), r[0].get("payload").map(|v| format!("{v:?}")).unwrap_or_default().chars().take(40).collect::<String>());
    assert_eq!(load(&t, c.call_id).await.state.as_str(), "INTRO");
    // Live mode (webhook label), same refusals; accepted rows are verified.
    let t = app(db, &[("VOICE_PROVIDER_MODE", "live")]);
    let c = dialing(&t).await;
    let leg = c.provider_call_id.clone().unwrap();
    let body = ev("e-live-1", "call.answered", json!({"call_control_id": leg}));
    let base = rows(&t).await.len();
    for r in [
        req(&body, None, None),
        req(&body, Some(now()), Some(&bad)),
        req(&body, Some(now() - 400), Some(&key())),
    ] {
        assert_eq!(t.send(r).await.status.as_u16(), 400);
    }
    assert_eq!(rows(&t).await.len(), base);
    assert_eq!(t.send(req(&body, Some(now()), Some(&key()))).await.status.as_u16(), 204);
    let r = rows(&t).await;
    println!("REVIEW live mode accepted row verified={:?}", r[base].get("verified"));
    assert_eq!(load(&t, c.call_id).await.state.as_str(), "INTRO");
    // No key configured: every delivery refused in both modes.
    for extra in [vec![("TELNYX_PUBLIC_KEY", "")], vec![("TELNYX_PUBLIC_KEY", ""), ("VOICE_PROVIDER_MODE", "live")]] {
        let t = TestApp::new(db, &extra);
        let s = t.send(req(&body, Some(now()), Some(&key()))).await;
        assert_eq!(s.status.as_u16(), 400, "{}", s.text);
    }
});

both_backends!(foreign_and_malformed_client_state_and_fuzz, |db| {
    let t = app(db, &[]);
    let a = dialing(&t).await;
    let b = dialing(&t).await;
    let a_leg = a.provider_call_id.clone().unwrap();
    let b_before = view(&load(&t, b.call_id).await);
    let b_id = b.call_id.to_string();
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    let client_states: Vec<Value> = vec![
        Value::Null,
        json!(""),
        json!(5),
        json!([1, 2]),
        json!({"call_id": b_id}),
        json!("!!!not base64!!!"),
        json!(b64("not json")),
        json!(b64("[1,2]")),
        json!(b64("{\"call_id\": 5}")),
        json!(b64("{\"call_id\": \"not-a-uuid\"}")),
        json!(b64(&format!("{{\"call_id\": \"{b_id}\", \"role\": \"owner\"}}"))),
        json!(b64(&format!("{{\"call_id\": \"{b_id}\", \"role\": 7}}"))),
        json!(encode_client_state(&client_state(&b_id, "counterparty"))),
        json!(encode_client_state(&client_state(&b_id, "owner"))),
        json!(format!("{}====garbage", encode_client_state(&client_state(&b_id, "counterparty")))),
    ];
    let event_types = [
        "call.answered",
        "call.speak.ended",
        "call.playback.ended",
        "call.transcription",
        "call.hangup",
        "call.machine.detection.ended",
        "conference.participant.joined",
        "call.initiated",
        "nonsense",
    ];
    let extras: Vec<Value> = vec![
        json!({}),
        json!({"transcription_data": {"transcript": "Can you commit to the deposit?", "is_final": true}}),
        json!({"transcription_data": "x"}),
        json!({"transcription_data": {"transcript": 5}}),
        json!({"transcription_data": {"transcript": "yes", "is_final": "no"}}),
        json!({"hangup_cause": 17}),
        json!({"hangup_cause": "call_rejected"}),
        json!({"result": "machine"}),
        json!({"result": ["machine"]}),
    ];
    let mut n = 0usize;
    let mut statuses: std::collections::BTreeMap<u16, usize> = Default::default();
    // Events for A's leg with B's id (or garbage) in client_state.
    for (i, cs) in client_states.iter().enumerate() {
        for (j, et) in event_types.iter().enumerate() {
            let extra = &extras[(i + j) % extras.len()];
            let mut p = json!({"call_control_id": a_leg, "client_state": cs});
            for (k, v) in extra.as_object().unwrap() {
                p[k] = v.clone();
            }
            let s = t.send(req(&ev(&format!("fz-{i}-{j}"), et, p), Some(now()), Some(&key()))).await;
            *statuses.entry(s.status.as_u16()).or_default() += 1;
            n += 1;
            assert_ne!(s.status.as_u16(), 500, "{et} {cs}: {}", s.text);
        }
    }
    let b_after = view(&load(&t, b.call_id).await);
    assert_eq!(b_before, b_after, "B's call moved by events of A's leg");
    // Structurally malformed bodies, all validly signed.
    let mut bodies: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"not json".to_vec(),
        b"[]".to_vec(),
        b"null".to_vec(),
        b"{}".to_vec(),
        b"{\"data\": null}".to_vec(),
        b"{\"data\": []}".to_vec(),
        b"{\"data\": \"x\"}".to_vec(),
        b"{\"data\": {}}".to_vec(),
        b"{\"data\": {\"id\": null}}".to_vec(),
        b"{\"data\": {\"id\": 5, \"payload\": []}}".to_vec(),
        b"{\"data\": {\"id\": {\"a\":1}, \"event_type\": 7, \"payload\": \"x\"}}".to_vec(),
        b"{\"data\": {\"id\": \"x1\", \"event_type\": \"call.hangup\", \"payload\": {\"call_control_id\": [1]}}}".to_vec(),
        b"{\"data\": {\"id\": \"x2\", \"event_type\": \"call.hangup\", \"payload\": {\"call_control_id\": {\"a\": 1}}}}".to_vec(),
        b"{\"data\": {\"id\": \"x3\", \"event_type\": \"call.hangup\", \"payload\": {\"call_control_id\": 12345678901234567890}}}".to_vec(),
        b"{\"data\": {\"id\": \"x4\", \"event_type\": \"call.hangup\", \"payload\": {\"call_control_id\": \"\"}}}".to_vec(),
        b"{\"data\": {\"id\": \"x5\"}, \"id\": \"y\"}".to_vec(),
        b"{\"data\": {\"id\": \"x6\", \"event_type\": \"call.answered\", \"payload\": {\"call_control_id\": \"v3:unknown\"}}}".to_vec(),
        "{\"data\": {\"id\": \"x7\u{0}\", \"event_type\": \"call.answered\"}}".as_bytes().to_vec(),
        b"{\"data\": {\"id\": \"x8\", \"event_type\": \"call.answered\", \"payload\": {\"call_control_id\": \"\\ud800\"}}}".to_vec(),
        b"\xff\xfe{\"data\": {\"id\": \"x9\"}}".to_vec(),
        b"{\"data\": {\"id\": 1e400}}".to_vec(),
        b"{\"data\": {\"id\": \"x10\"}} trailing".to_vec(),
        b"{\"data\": {\"id\": \"x11\", \"id\": \"x12\"}}".to_vec(),
    ];
    let deep = format!("{}{}", "[".repeat(5000), "]".repeat(5000));
    bodies.push(format!("{{\"data\": {{\"id\": \"deep\", \"payload\": {deep}}}}}").into_bytes());
    let long = "a".repeat(300);
    bodies.push(format!("{{\"data\": {{\"id\": \"{long}\"}}}}").into_bytes());
    // Deterministic pseudo-random byte soup.
    let mut seed: u64 = 0x5eed;
    for _ in 0..200 {
        let len = (seed % 120) as usize;
        let mut v = Vec::with_capacity(len);
        for _ in 0..len {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let alphabet = b"{}[]\":,0123456789abcdefnulltrue \\u";
            v.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
        }
        seed = seed.wrapping_add(7);
        bodies.push(v);
    }
    let mut fivehundreds = Vec::new();
    for body in &bodies {
        let s = t.send(req(body, Some(now()), Some(&key()))).await;
        *statuses.entry(s.status.as_u16()).or_default() += 1;
        n += 1;
        if s.status.as_u16() >= 500 {
            fivehundreds.push((String::from_utf8_lossy(&body[..body.len().min(60)]).into_owned(), s.body["error_code"].clone(), s.body["message"].clone()));
        }
    }
    println!("REVIEW webhook fuzz: {n} signed deliveries, statuses {statuses:?}");
    for f in &fivehundreds {
        println!("REVIEW webhook 5xx: {f:?}");
    }
    assert_eq!(b_before, view(&load(&t, b.call_id).await));
});

/// PostgreSQL, two deliveries of one event at the same moment: processed once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn dedupe_race_postgres() {
    let Some(url) = common::pg_url() else {
        eprintln!("SKIPPED");
        return;
    };
    let base = sqlx::postgres::PgConnectOptions::from_str(&ss_server::config::normalize_database_url(&url)).unwrap();
    let schema = format!("rt_{}", Uuid::new_v4().simple());
    let mut admin = sqlx::ConnectOptions::connect(&base).await.unwrap();
    sqlx::Executor::execute(&mut admin, format!("CREATE SCHEMA {schema}").as_str()).await.unwrap();
    drop(admin);
    let store = ss_server::store::postgres::PgStore::connect_with(
        base.options([("search_path", schema.as_str())]).application_name(&schema),
        16,
    )
    .await
    .unwrap();
    store.migrate().await.unwrap();
    let store: Arc<dyn ss_server::store::Store> = Arc::new(store);
    let k = pk();
    let app = ss_server::build_app(common::test_settings(&[("TELNYX_PUBLIC_KEY", k.as_str())]), store);
    app.extensions.insert(Arc::new(OwnerNotify::new(Arc::new(RecordingNotifier::new()))));
    let router = ss_server::http::router(app.clone());
    let t = TestApp { app, router, kind: common::Kind::Postgres };
    let mut totals = (0usize, 0usize);
    for round in 0..10 {
        let c = dialing(&t).await;
        let leg = c.provider_call_id.clone().unwrap();
        let cs = encode_client_state(&client_state(&c.call_id.to_string(), "counterparty"));
        // Move to ACTIVE.
        for (i, et) in ["call.answered", "call.speak.ended"].iter().enumerate() {
            let s = t.send(req(&ev(&format!("r{round}-{i}"), et, json!({"call_control_id": leg, "client_state": cs})), Some(now()), Some(&key()))).await;
            assert_eq!(s.status.as_u16(), 204);
        }
        let before = load(&t, c.call_id).await.transcript.len();
        let body = ev(
            &format!("r{round}-utter"),
            "call.transcription",
            json!({"call_control_id": leg, "transcription_data": {"transcript": "I'm not sure, I'd have to check.", "is_final": true}}),
        );
        let mut hs = Vec::new();
        for _ in 0..4 {
            let r = t.router.clone();
            let b = body.clone();
            hs.push(tokio::spawn(async move {
                r.oneshot(req(&b, Some(now()), Some(&key()))).await.unwrap().status().as_u16()
            }));
        }
        let mut st = Vec::new();
        for h in hs {
            st.push(h.await.unwrap());
        }
        let after = load(&t, c.call_id).await;
        let counterparty_turns = after.transcript.iter().filter(|x| x.speaker == "counterparty").count();
        assert!(st.iter().all(|s| *s == 204), "{st:?}");
        assert_eq!(counterparty_turns, 1, "round {round}: processed more than once");
        totals.0 += 1;
        totals.1 += after.transcript.len() - before;
    }
    println!("REVIEW dedupe race: 10 rounds of 4 concurrent deliveries, each processed once");
    common::drop_schema(&schema).await;
}
