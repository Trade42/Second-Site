//! Server part 6: five hardening items the independent reviews of parts 4 and 5 left open.
//!
//! - O4: with the live voice provider, `"simulate": true` on `POST .../calls` is refused before
//!   any approval is spent or any carrier request is made (a real call must never be labelled
//!   simulated); the fixture provider is unchanged.
//! - O6: a signed Telnyx event whose `data.id` does not fit `provider_event_id` (VARCHAR(255)) is
//!   a 400 on both stores, not a 500 on one and a stored row on the other.
//! - R1: a request body that stops arriving is answered within the idle bound, on every route,
//!   native or forwarded; a slow but steady upload still works; responses (the event stream) are
//!   not bounded.
//! - R2: a `PYTHON_SERVICE_URL` that leads back to this service is not reported `LIVE` and is not
//!   followed round in circles.
//!
//! (R3, the `.env` reader, is tested next to the parser in `src/config.rs`.) No real call is
//! placed and no real provider is contacted: the carrier and the Python service are loopback
//! mocks.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use ss_domain::integrations::webhook_signatures::{VerifiedWebhook, WebhookMode};
use ss_server::contracts::ApprovalStatus;
use ss_server::integrations::telnyx::{LiveTelnyxProvider, Voice};
use ss_server::services::forward::{install, parse_target, PythonLink, Target, Timeouts};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::store::comms::{webhooks, CALLS};
use ss_server::store::repo::APPROVALS;
use ss_server::store::{Query, SqlValue};
use uuid::Uuid;

use common::mock_http::{Canned, MockServer, Recorded};
use common::{assert_problem, demo_project_body, TestApp, TestDb};

// ---- shared helpers ------------------------------------------------------------------------

fn json_reply(v: Value) -> Canned {
    Canned {
        status: 200,
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&v).unwrap(),
    }
}

/// A loopback carrier that accepts every command (a dial gets a call control id).
fn carrier() -> MockServer {
    MockServer::start_with(|req: &Recorded| {
        json_reply(match req.path.as_str() {
            "/v2/calls" => json!({"data": {"call_control_id": "v3:part6"}}),
            _ => json!({"data": {"result": "ok"}}),
        })
    })
}

/// Serve `router` on an ephemeral loopback port.
async fn serve(router: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

/// Send `head` (and `body`) over a fresh socket, keep the socket open without sending more, and
/// return the first bytes of the answer with the time it took (or `None` after `wait`).
fn stalled(addr: SocketAddr, raw: Vec<u8>, wait: Duration) -> (Option<String>, Duration) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(wait)).unwrap();
    s.write_all(&raw).unwrap();
    let t0 = Instant::now();
    let mut buf = vec![0u8; 4096];
    let mut out = Vec::new();
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                // The whole answer is small; stop once the body has arrived.
                let text = String::from_utf8_lossy(&out);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let took = t0.elapsed();
    if out.is_empty() {
        (None, took)
    } else {
        (Some(String::from_utf8_lossy(&out).into_owned()), took)
    }
}

/// The JSON body of a raw HTTP/1.1 answer.
fn raw_body(answer: &str) -> Value {
    answer
        .split_once("\r\n\r\n")
        .and_then(|(_, b)| serde_json::from_str(b).ok())
        .unwrap_or(Value::Null)
}

// ---- O4: `simulate: true` with the live voice provider ---------------------------------------

struct CallCtx {
    t: TestApp,
    token: String,
}

impl CallCtx {
    fn new(db: &TestDb) -> CallCtx {
        let t = TestApp::new(db, &[("TELNYX_FROM_NUMBER", "+15555550111")]);
        let fake = Arc::new(RecordingNotifier::new());
        t.app.extensions.insert(Arc::new(OwnerNotify::new(fake)));
        let token = t.token_for(Uuid::new_v4());
        CallCtx { t, token }
    }

    fn live(&self, carrier: &MockServer) {
        self.t.app.extensions.insert(Arc::new(Voice::live(Arc::new(
            LiveTelnyxProvider::with_base_url(
                "KEY",
                "conn1",
                true,
                &format!("{}/v2", carrier.base),
            ),
        ))));
        assert_eq!(self.t.app.voice().mode(), "live");
    }

    /// The demo project after the LA search, and an outreach call on its Hollywood property
    /// whose PLACE_CALL approval is APPROVED: (project id, communication id, approval id).
    async fn approved_outreach(&self) -> (Uuid, String, Uuid) {
        let p = self
            .t
            .create_project(&self.token, &demo_project_body())
            .await;
        let pid: Uuid = p["project"]["project_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let r = self
            .t
            .post(
                &format!("/api/v1/projects/{pid}/properties/search"),
                Some(&self.token),
                &json!({"city": "Los Angeles", "state": "CA"}),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
        let props = self
            .t
            .get(
                &format!("/api/v1/projects/{pid}/properties"),
                Some(&self.token),
            )
            .await;
        let prop = props
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
            .expect("the Hollywood property");
        let out = self
            .t
            .post(
                &format!("/api/v1/projects/{pid}/outreach"),
                Some(&self.token),
                &json!({
                    "property_id": prop["property_id"],
                    "channel": "VOICE",
                    "recipient": {"role": "broker", "name": "Dana Broker", "phone": "+15555550100"},
                    "purpose": "Verify electrical, plumbing and HVAC",
                }),
            )
            .await;
        assert_eq!(out.status, StatusCode::ACCEPTED, "{}", out.text);
        let appr = &out.body["approval"];
        let ok = self
            .t
            .post(
                &format!(
                    "/api/v1/approvals/{}/decision",
                    appr["approval_id"].as_str().unwrap()
                ),
                Some(&self.token),
                &json!({"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]}),
            )
            .await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        (
            pid,
            out.body["communication"]["communication_id"]
                .as_str()
                .unwrap()
                .to_owned(),
            appr["approval_id"].as_str().unwrap().parse().unwrap(),
        )
    }

    async fn start(&self, pid: Uuid, body: Value) -> common::Resp {
        self.t
            .post(
                &format!("/api/v1/projects/{pid}/calls"),
                Some(&self.token),
                &body,
            )
            .await
    }

    async fn approval_status(&self, id: Uuid) -> ApprovalStatus {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let a = APPROVALS.require(&mut *tx, id).await.unwrap();
        tx.commit().await.unwrap();
        a.approval_status
    }

    async fn calls(&self, pid: Uuid) -> usize {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let n = CALLS
            .list(&mut *tx, Query::new().eq("project_id", pid))
            .await
            .unwrap()
            .len();
        tx.commit().await.unwrap();
        n
    }
}

both_backends!(live_provider_refuses_a_simulated_call, |db| {
    let up = carrier();
    let c = CallCtx::new(db);
    c.live(&up);
    let (pid, comm, approval) = c.approved_outreach().await;
    let r = c
        .start(pid, json!({"communication_id": comm, "simulate": true}))
        .await;
    // The simulator's own answer with the live provider (`SIMULATOR_DISABLED`, 409).
    assert_problem(&r, 409, "SIMULATOR_DISABLED");
    assert!(
        r.body["message"].as_str().unwrap().contains("real call"),
        "{}",
        r.text
    );
    assert!(up.requests().is_empty(), "{:?}", up.requests());
    assert_eq!(c.approval_status(approval).await, ApprovalStatus::Approved);
    assert_eq!(c.calls(pid).await, 0);

    // The same approval still places the real call when `simulate` is false or absent: one dial,
    // not labelled simulated.
    let r = c
        .start(pid, json!({"communication_id": comm, "simulate": false}))
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.body["simulated"], false);
    assert_eq!(r.body["state"], "DIALING");
    let dials: Vec<String> = up.requests().into_iter().map(|q| q.path).collect();
    assert_eq!(dials, ["/v2/calls"]);
    assert_eq!(c.approval_status(approval).await, ApprovalStatus::Executed);
    assert_eq!(c.calls(pid).await, 1);
});

both_backends!(fixture_provider_simulates_as_before, |db| {
    for body_extra in [json!(true), json!(false), Value::Null] {
        let c = CallCtx::new(db);
        assert_eq!(c.t.app.voice().mode(), "fixture");
        let (pid, comm, approval) = c.approved_outreach().await;
        let mut body = json!({"communication_id": comm});
        if !body_extra.is_null() {
            body["simulate"] = body_extra.clone();
        }
        let r = c.start(pid, body).await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{body_extra}: {}", r.text);
        // With the fixture provider every call is simulated, whatever the client asked.
        assert_eq!(r.body["simulated"], true);
        assert_eq!(r.body["state"], "DIALING");
        assert_eq!(c.approval_status(approval).await, ApprovalStatus::Executed);
        assert_eq!(c.calls(pid).await, 1);
    }
});

// ---- O6: a Telnyx event id longer than the column ---------------------------------------------

fn telnyx_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn telnyx_app(db: &TestDb) -> TestApp {
    let pk =
        base64::engine::general_purpose::STANDARD.encode(telnyx_key().verifying_key().to_bytes());
    let t = TestApp::new(db, &[("TELNYX_PUBLIC_KEY", pk.as_str())]);
    let fake = Arc::new(RecordingNotifier::new());
    t.app.extensions.insert(Arc::new(OwnerNotify::new(fake)));
    t
}

fn signed_telnyx(body: &[u8]) -> Request<Body> {
    let ts = chrono::Utc::now().timestamp();
    let mut msg = format!("{ts}|").into_bytes();
    msg.extend_from_slice(body);
    let sig = base64::engine::general_purpose::STANDARD.encode(telnyx_key().sign(&msg).to_bytes());
    Request::builder()
        .method(Method::POST)
        .uri("/webhooks/telnyx")
        .header("telnyx-signature-ed25519", sig)
        .header("telnyx-timestamp", ts.to_string())
        .header("content-type", "application/json")
        .body(Body::from(body.to_vec()))
        .unwrap()
}

async fn deliver_id(t: &TestApp, id: &Value) -> common::Resp {
    let body = serde_json::to_vec(&json!({
        "data": {"record_type": "event", "id": id, "event_type": "call.answered",
                 "payload": {"call_control_id": "v3:unknown-leg"}}
    }))
    .unwrap();
    t.send(signed_telnyx(&body)).await
}

async fn stored_ids(t: &TestApp, provider: &str) -> Vec<String> {
    let mut tx = t.app.store.begin().await.unwrap();
    let rows = webhooks::rows(&mut *tx, provider).await.unwrap();
    tx.commit().await.unwrap();
    rows.iter()
        .map(|r| match r.get("provider_event_id") {
            Some(SqlValue::Text(s)) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

both_backends!(an_event_id_longer_than_the_column_is_a_400, |db| {
    let t = telnyx_app(db);
    // 255 characters fit (twice: a replay is still a 204 that does nothing), and so do 255
    // two-byte characters (510 bytes): the column counts characters.
    let fits = "e".repeat(255);
    let wide = "é".repeat(255);
    for id in [&fits, &fits, &wide] {
        let r = deliver_id(&t, &json!(id)).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}: {}", id.len(), r.text);
    }
    assert_eq!(stored_ids(&t, "telnyx").await, [fits.clone(), wide.clone()]);
    // One character more is refused with the invalid-payload answer, and nothing is stored.
    for id in ["e".repeat(256), "é".repeat(256), "x".repeat(10_000)] {
        let r = deliver_id(&t, &json!(id)).await;
        assert_problem(&r, 400, "INVALID_SIGNATURE");
        assert!(
            r.body["message"].as_str().unwrap().contains("data.id"),
            "{}",
            r.text
        );
    }
    assert_eq!(stored_ids(&t, "telnyx").await, [fits.clone(), wide.clone()]);
    // A number is `str()`-ed, as before.
    let r = deliver_id(&t, &json!(12345)).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    assert_eq!(stored_ids(&t, "telnyx").await.last().unwrap(), "12345");
});

// The same function keeps Photon's shape of event (`webhook-id`) unchanged for valid ids.
both_backends!(record_once_is_unchanged_for_valid_ids, |db| {
    let t = telnyx_app(db);
    for (i, len) in [1usize, 64, 255].into_iter().enumerate() {
        let v = VerifiedWebhook {
            provider: "photon".into(),
            event_id: format!("msg_{i}_{}", "a".repeat(len.saturating_sub(6))),
            mode: WebhookMode::Fixture,
        };
        assert!(v.event_id.chars().count() <= 255);
        let payload = json!({"type": "message.received", "n": i});
        let mut tx = t.app.store.begin().await.unwrap();
        assert!(webhooks::record_once(&mut *tx, &v, &payload).await.unwrap());
        assert!(!webhooks::record_once(&mut *tx, &v, &payload).await.unwrap());
        tx.commit().await.unwrap();
    }
    let rows = {
        let mut tx = t.app.store.begin().await.unwrap();
        let rows = webhooks::rows(&mut *tx, "photon").await.unwrap();
        tx.commit().await.unwrap();
        rows
    };
    assert_eq!(rows.len(), 3);
    for r in &rows {
        assert!(matches!(r.get("verified"), Some(SqlValue::Bool(false))));
        match r.get("payload") {
            Some(SqlValue::Json(p)) => assert_eq!(p["mode"], "fixture"),
            other => panic!("{other:?}"),
        }
    }
});

// ---- R1: a request body that stops arriving -------------------------------------------------

/// The bound itself (30 s) over real sockets, through the production router: a stalled body on a
/// native route, on the native Telnyx webhook and on a forwarded route is answered within the
/// bound plus a few seconds, and the stalled forwarded request never reaches the Python service.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_uploads_are_answered_within_the_bound() {
    let db = TestDb::new(common::Kind::Memory).await.unwrap();
    let python = MockServer::start_with(|_req: &Recorded| json_reply(json!({"status": "ok"})));
    let t = TestApp::new(&db, &[("PYTHON_SERVICE_URL", python.base.as_str())]);
    let token = t.token_for(Uuid::new_v4());
    let addr = serve(t.router.clone()).await;
    let pid = Uuid::new_v4();
    let cases: Vec<(&str, String)> = vec![
        (
            "native POST /api/v1/projects",
            format!("POST /api/v1/projects HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {token}\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{{\"a\""),
        ),
        (
            "native POST /webhooks/telnyx",
            "POST /webhooks/telnyx HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{\"a\"".to_owned(),
        ),
        (
            "forwarded POST .../chat",
            format!("POST /api/v1/projects/{pid}/chat HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {token}\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{{\"a\""),
        ),
    ];
    let mut jobs = Vec::new();
    for (name, raw) in cases {
        jobs.push(tokio::task::spawn_blocking(move || {
            (
                name,
                stalled(addr, raw.into_bytes(), Duration::from_secs(40)),
            )
        }));
    }
    for job in jobs {
        let (name, (answer, took)) = job.await.unwrap();
        let answer = answer.unwrap_or_else(|| panic!("{name}: no answer in {took:?}"));
        assert!(
            answer.starts_with("HTTP/1.1 408"),
            "{name}: {}",
            answer.lines().next().unwrap_or("")
        );
        assert!(
            took >= Duration::from_secs(29) && took <= Duration::from_secs(32),
            "{name}: {took:?}"
        );
        let body = raw_body(&answer);
        assert_eq!(body["error_code"], "REQUEST_TIMEOUT", "{name}: {answer}");
        assert_eq!(body["retryable"], true, "{name}: {answer}");
    }
    let forwarded: Vec<String> = python
        .requests()
        .into_iter()
        .map(|r| r.path)
        .filter(|p| p != "/health")
        .collect();
    assert!(forwarded.is_empty(), "{forwarded:?}");
}

// ---- R2: a PYTHON_SERVICE_URL that leads back to this service ---------------------------------

fn settings(vars: &[(&str, &str)]) -> ss_server::config::Settings {
    common::test_settings(vars)
}

#[test]
fn a_target_naming_this_services_own_listener_is_not_usable() {
    let own = |host: &str, port: &str, url: &str| -> bool {
        let s = settings(&[("HOST", host), ("PORT", port), ("PYTHON_SERVICE_URL", url)]);
        let link = PythonLink::from_settings(&s, Timeouts::default());
        match link.target() {
            Target::Invalid(why) => {
                assert!(why.contains("this service"), "{why}");
                assert!(!why.contains(url), "the value is never repeated: {why}");
                true
            }
            Target::Base(_) => false,
            Target::NotConfigured => panic!("{url}"),
        }
    };
    // A wildcard listener (the default HOST) is reached through every loopback spelling.
    for host in ["0.0.0.0", "::", "[::]"] {
        for url in [
            "http://127.0.0.1:8123",
            "http://localhost:8123",
            "http://LOCALHOST:8123/",
            "http://[::1]:8123",
            "http://0.0.0.0:8123",
            "http://[::]:8123",
            "http://127.0.0.1:8123/prefix",
            "https://localhost:8123",
            "http://[::ffff:127.0.0.1]:8123",
            "http://127.1.2.3:8123",
            // The resolver's `inet_aton` spellings of 127.0.0.1.
            "http://127.1:8123",
            "http://2130706433:8123",
            "http://0x7f.0.0.1:8123",
            "http://0177.0.0.1:8123",
        ] {
            assert!(own(host, "8123", url), "HOST={host} {url}");
        }
    }
    // A loopback listener: its own address, `localhost` and the wildcard address.
    for (host, url) in [
        ("127.0.0.1", "http://127.0.0.1:8123"),
        ("127.0.0.1", "http://localhost:8123"),
        ("127.0.0.1", "http://0.0.0.0:8123"),
        ("localhost", "http://127.0.0.1:8123"),
        ("localhost", "http://[::1]:8123"),
        ("localhost", "http://localhost:8123"),
        ("::1", "http://[::1]:8123"),
        ("::1", "http://localhost:8123"),
        ("[::1]", "http://[0:0:0:0:0:0:0:1]:8123"),
        ("10.1.2.3", "http://10.1.2.3:8123"),
    ] {
        assert!(own(host, "8123", url), "HOST={host} {url}");
    }
    // The scheme's default port.
    assert!(own("0.0.0.0", "80", "http://localhost"));
    assert!(own("0.0.0.0", "443", "https://127.0.0.1/"));
    // Not this listener: another port, another host (the compose shape: both services on 8000),
    // another loopback address than the one bound, a loopback target of a non-loopback listener.
    for (host, port, url) in [
        ("0.0.0.0", "8123", "http://127.0.0.1:8124"),
        ("0.0.0.0", "8000", "http://api:8000"),
        ("0.0.0.0", "8000", "http://python.internal:8000"),
        ("127.0.0.1", "8123", "http://127.0.0.2:8123"),
        ("10.1.2.3", "8123", "http://127.0.0.1:8123"),
        ("10.1.2.3", "8123", "http://10.1.2.4:8123"),
        ("0.0.0.0", "80", "https://localhost"),
        ("0.0.0.0", "443", "http://localhost"),
        ("0.0.0.0", "8123", "http://localhost:80"),
    ] {
        assert!(!own(host, port, url), "HOST={host} PORT={port} {url}");
    }
}

both_backends!(a_self_target_answers_503_at_once_and_is_blocked, |db| {
    // Something does listen on that port (here a mock), but nothing is sent to it: no forward
    // and no health check.
    let listener = MockServer::start_with(|_req: &Recorded| json_reply(json!({"status": "ok"})));
    let port: u16 = listener.base.rsplit(':').next().unwrap().parse().unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let p = port.to_string();
    let t = TestApp::new(
        db,
        &[
            ("HOST", "127.0.0.1"),
            ("PORT", p.as_str()),
            ("PYTHON_SERVICE_URL", url.as_str()),
        ],
    );
    let t0 = Instant::now();
    let r = t.get("/api/v1/owner/channel", None).await;
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    assert_problem(&r, 503, "PYTHON_SERVICE_NOT_CONFIGURED");
    let msg = r.body["message"].as_str().unwrap();
    assert!(
        msg.contains("not usable") && msg.contains("this service"),
        "{msg}"
    );
    assert_eq!(r.body["retryable"], false);
    let ready = t.get("/ready", None).await;
    let entry = ready.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "python_service")
        .unwrap()
        .clone();
    assert_eq!(entry["status"], "BLOCKED", "{entry}");
    assert!(
        entry["note"].as_str().unwrap().contains("this service"),
        "{entry}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(listener.requests().is_empty(), "{:?}", listener.requests());
});

fn quick() -> Timeouts {
    Timeouts {
        connect: Duration::from_millis(1000),
        chat: Duration::from_millis(3000),
        default: Duration::from_millis(3000),
        health: Duration::from_millis(1000),
        health_interval: Duration::from_millis(100),
    }
}

async fn python_entry(t: &TestApp) -> Value {
    let ready = t.get("/ready", None).await;
    ready.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "python_service")
        .unwrap()
        .clone()
}

/// The link reaches this same app through a spelling the start-up check cannot see (the app's
/// own settings name another port): its `/health` answer is recognised, `/ready` says BLOCKED,
/// and forwarded routes answer 503 without sending anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn this_services_own_health_answer_is_recognised() {
    let db = TestDb::new(common::Kind::Memory).await.unwrap();
    let t = TestApp::new(&db, &[("HOST", "127.0.0.1"), ("PORT", "1")]);
    let addr = serve(t.router.clone()).await;
    let url = format!("http://{addr}");
    install(
        &t.app,
        Arc::new(PythonLink::new(parse_target(Some(&url)), quick())),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let entry = loop {
        let e = python_entry(&t).await;
        if e["status"] != "NOT_STARTED" || Instant::now() > deadline {
            break e;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(entry["status"], "BLOCKED", "{entry}");
    assert!(
        entry["note"].as_str().unwrap().contains("this service"),
        "{entry}"
    );
    let t0 = Instant::now();
    let r = t.get("/api/v1/owner/channel", None).await;
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
    assert_problem(&r, 503, "PYTHON_SERVICE_NOT_CONFIGURED");
    assert!(
        r.body["message"].as_str().unwrap().contains("this service"),
        "{}",
        r.text
    );
}

// ---- R1 with a short bound (the same layer, `http::with_body_idle_timeout`) -------------------

const SHORT: Duration = Duration::from_secs(1);

fn short_router(t: &TestApp) -> axum::Router {
    ss_server::http::with_body_idle_timeout(
        ss_server::http::routes::api_routes(),
        t.app.clone(),
        SHORT,
    )
}

fn media_dir() -> String {
    let d = std::env::temp_dir()
        .join(format!("ss-part6-{}", Uuid::new_v4().simple()))
        .join("objects");
    std::fs::create_dir_all(&d).unwrap();
    d.to_string_lossy().into_owned()
}

/// Every route answers a stalled body with the same honest 408, closes the connection, and the
/// forwarded ones say that nothing was forwarded (and nothing was).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_body_is_an_honest_408_on_every_kind_of_route() {
    let db = TestDb::new(common::Kind::Memory).await.unwrap();
    let python = MockServer::start_with(|_req: &Recorded| json_reply(json!({"status": "ok"})));
    let dir = media_dir();
    let t = TestApp::new(
        &db,
        &[
            ("PYTHON_SERVICE_URL", python.base.as_str()),
            ("OBJECT_STORAGE_LOCAL_DIR", dir.as_str()),
        ],
    );
    let (token, _) = t.dev_token().await;
    let p = t.create_project(&token, &demo_project_body()).await;
    let pid = p["project"]["project_id"].as_str().unwrap().to_owned();
    let addr = serve(short_router(&t)).await;
    let auth = format!("authorization: Bearer {token}\r\n");
    let cases: Vec<(&str, bool, String)> = vec![
        ("native JSON", false, format!("POST /api/v1/projects HTTP/1.1\r\nhost: x\r\n{auth}content-type: application/json\r\ncontent-length: 1000\r\n\r\n{{\"a\"")),
        ("native chunked", false, format!("POST /api/v1/projects HTTP/1.1\r\nhost: x\r\n{auth}content-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n4\r\n{{\"a\"\r\n")),
        ("native webhook", false, "POST /webhooks/telnyx HTTP/1.1\r\nhost: x\r\ncontent-length: 1000\r\n\r\n{\"a\"".to_owned()),
        ("native multipart", false, format!("POST /api/v1/projects/{pid}/media HTTP/1.1\r\nhost: x\r\n{auth}content-type: multipart/form-data; boundary=BND\r\ncontent-length: 5000\r\n\r\n--BND\r\ncontent-disposition: form-data; name=\"file\"; filename=\"a.jpg\"\r\ncontent-type: image/jpeg\r\n\r\nabc")),
        ("forwarded chat", true, format!("POST /api/v1/projects/{pid}/chat HTTP/1.1\r\nhost: x\r\n{auth}content-type: application/json\r\ncontent-length: 1000\r\n\r\n{{\"a\"")),
        ("forwarded webhook", true, "POST /webhooks/photon HTTP/1.1\r\nhost: x\r\ncontent-length: 1000\r\n\r\n{\"a\"".to_owned()),
        ("forwarded chunked", true, "POST /webhooks/photon HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n2\r\n{}\r\n".to_owned()),
    ];
    let mut jobs = Vec::new();
    for (name, forwarded, raw) in cases {
        jobs.push(tokio::task::spawn_blocking(move || {
            (
                name,
                forwarded,
                stalled(addr, raw.into_bytes(), Duration::from_secs(10)),
            )
        }));
    }
    for job in jobs {
        let (name, forwarded, (answer, took)) = job.await.unwrap();
        let answer = answer.unwrap_or_else(|| panic!("{name}: no answer in {took:?}"));
        let (head, _) = answer.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 408 "), "{name}: {head}");
        assert!(
            took >= SHORT && took < SHORT + Duration::from_secs(2),
            "{name}: {took:?}"
        );
        assert!(
            head.to_ascii_lowercase().contains("\r\nconnection: close"),
            "{name}: {head}"
        );
        assert!(
            head.to_ascii_lowercase().contains("\r\nx-request-id: "),
            "{name}: {head}"
        );
        let body = raw_body(&answer);
        common::assert_conforms("ProblemDetail", &body);
        assert_eq!(body["error_code"], "REQUEST_TIMEOUT", "{name}");
        assert_eq!(body["retryable"], true, "{name}");
        assert!(body["request_id"].is_string(), "{name}");
        let msg = body["message"].as_str().unwrap();
        assert!(msg.contains("stopped arriving"), "{name}: {msg}");
        if forwarded {
            assert!(msg.contains("nothing was forwarded"), "{name}: {msg}");
        } else {
            assert!(msg.contains("not processed"), "{name}: {msg}");
        }
    }
    let forwarded: Vec<String> = python
        .requests()
        .into_iter()
        .map(|r| r.path)
        .filter(|p| p != "/health")
        .collect();
    assert!(forwarded.is_empty(), "{forwarded:?}");
    // Nothing was created by the stalled project creation or upload.
    let projects = t.get("/api/v1/projects", Some(&token)).await;
    assert_eq!(
        projects.body.as_array().unwrap().len(),
        1,
        "{}",
        projects.text
    );
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
}

/// Send `head`, then `body` in `chunk`-byte pieces with `gap` between them; the full answer.
fn trickle(addr: SocketAddr, head: String, body: &[u8], chunk: usize, gap: Duration) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    for piece in body.chunks(chunk) {
        std::thread::sleep(gap);
        s.write_all(piece).unwrap();
    }
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// A slow but steady upload (every piece within the bound, the whole far longer than it) is not
/// cut off: a media file is stored whole, and a forwarded body arrives byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_steady_upload_is_not_cut_off() {
    let db = TestDb::new(common::Kind::Memory).await.unwrap();
    let python = MockServer::start_with(|_req: &Recorded| Canned {
        status: 204,
        content_type: None,
        body: Vec::new(),
    });
    let dir = media_dir();
    let t = TestApp::new(
        &db,
        &[
            ("PYTHON_SERVICE_URL", python.base.as_str()),
            ("OBJECT_STORAGE_LOCAL_DIR", dir.as_str()),
        ],
    );
    let (token, _) = t.dev_token().await;
    let p = t.create_project(&token, &demo_project_body()).await;
    let pid = p["project"]["project_id"].as_str().unwrap().to_owned();
    let addr = serve(short_router(&t)).await;
    let file: Vec<u8> = (0..6000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut form = b"--BND\r\ncontent-disposition: form-data; name=\"file\"; filename=\"slow.jpg\"\r\ncontent-type: image/jpeg\r\n\r\n".to_vec();
    form.extend_from_slice(&file);
    form.extend_from_slice(b"\r\n--BND--\r\n");
    let head = format!(
        "POST /api/v1/projects/{pid}/media HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {token}\r\ncontent-type: multipart/form-data; boundary=BND\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        form.len()
    );
    let t0 = Instant::now();
    // 25 pieces, 300 ms apart: about 7.5 s in all, under a 1 s idle bound.
    let answer = tokio::task::spawn_blocking(move || {
        trickle(addr, head, &form, 250, Duration::from_millis(300))
    })
    .await
    .unwrap();
    assert!(t0.elapsed() > SHORT * 5, "{:?}", t0.elapsed());
    assert!(answer.starts_with("HTTP/1.1 201 "), "{answer}");
    let body = raw_body(&answer);
    assert_eq!(body["size_bytes"], 6000, "{answer}");
    let key = body["object_key"].as_str().unwrap();
    assert_eq!(
        std::fs::read(std::path::Path::new(&dir).join(key)).unwrap(),
        file
    );

    // The same through the forwarder.
    let payload: Vec<u8> = (0..3000u32).map(|i| (i % 256) as u8).collect();
    let head = format!(
        "POST /webhooks/photon HTTP/1.1\r\nhost: x\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len()
    );
    let sent = payload.clone();
    let t0 = Instant::now();
    let answer = tokio::task::spawn_blocking(move || {
        trickle(addr, head, &sent, 300, Duration::from_millis(300))
    })
    .await
    .unwrap();
    assert!(t0.elapsed() > SHORT * 2, "{:?}", t0.elapsed());
    assert!(answer.starts_with("HTTP/1.1 204 "), "{answer}");
    let got: Vec<Vec<u8>> = python
        .requests()
        .into_iter()
        .filter(|r| r.path == "/webhooks/photon")
        .map(|r| r.body)
        .collect();
    assert_eq!(got, [payload]);
}

/// The bound is on request bodies only: the event stream stays open, and keeps sending, for far
/// longer than the bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_event_stream_outlives_the_bound() {
    let db = TestDb::new(common::Kind::Memory).await.unwrap();
    let t = TestApp::new(&db, &[]);
    let (token, _) = t.dev_token().await;
    let p = t.create_project(&token, &demo_project_body()).await;
    let pid = p["project"]["project_id"].as_str().unwrap().to_owned();
    let addr = serve(short_router(&t)).await;
    let answer = tokio::task::spawn_blocking(move || {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(
            format!("GET /api/v1/projects/{pid}/events/stream HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {token}\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
        let t0 = Instant::now();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        // Read for 4 s (4 bounds): keepalives keep arriving after the bound.
        while t0.elapsed() < Duration::from_secs(4) {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        (String::from_utf8_lossy(&out).into_owned(), t0.elapsed())
    })
    .await
    .unwrap();
    let (text, took) = answer;
    assert!(text.starts_with("HTTP/1.1 200 "), "{text}");
    assert!(
        took >= Duration::from_secs(4),
        "the stream closed after {took:?}: {text}"
    );
    assert!(text.matches(": keepalive\n\n").count() >= 3, "{text}");
}

// ---- R2 (b): the health answer and the one-round loop guard -----------------------------------

#[tokio::test]
async fn only_this_services_health_answer_carries_the_marker() {
    let db = TestDb::new(common::Kind::Memory).await.unwrap();
    let t = TestApp::new(&db, &[]);
    let h = t.get("/health", None).await;
    assert_eq!(h.status, StatusCode::OK);
    assert_eq!(h.headers["x-second-site-backend"], "rust");
    for path in ["/ready", "/api/v1/projects", "/nope"] {
        let r = t.get(path, None).await;
        assert!(!r.headers.contains_key("x-second-site-backend"), "{path}");
    }
}

/// Before any health check has run (the link is installed without its checks), a request that
/// goes round through this service is refused when it comes back: one round, a 508, at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_that_comes_back_is_not_forwarded_again() {
    let db = TestDb::new(common::Kind::Memory).await.unwrap();
    let t = TestApp::new(&db, &[("HOST", "127.0.0.1"), ("PORT", "1")]);
    let addr = serve(t.router.clone()).await;
    let url = format!("http://{addr}");
    // No `install`: no health check, so only the loop guard can stop it.
    t.app
        .extensions
        .insert(Arc::new(PythonLink::new(parse_target(Some(&url)), quick())));
    for (method, path, body) in [
        (Method::GET, "/api/v1/owner/channel?x=1", None),
        (
            Method::POST,
            "/webhooks/photon",
            Some(json!({"type": "message.received"})),
        ),
    ] {
        let t0 = Instant::now();
        let r = t.call(method.clone(), path, None, body.as_ref()).await;
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "{path}: {:?}",
            t0.elapsed()
        );
        assert_problem(&r, 508, "PYTHON_SERVICE_LOOP");
        assert!(
            r.body["message"]
                .as_str()
                .unwrap()
                .contains("not forwarded again"),
            "{}",
            r.text
        );
    }
}

/// The guard holds a request only while it is being forwarded: the same request with the same
/// `x-request-id`, sent again after the first one was answered, is forwarded again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeated_request_is_forwarded_once_the_first_is_answered() {
    let db = TestDb::new(common::Kind::Memory).await.unwrap();
    let python = MockServer::start_with(|_req: &Recorded| json_reply(json!({"ok": true})));
    let t = TestApp::new(&db, &[("PYTHON_SERVICE_URL", python.base.as_str())]);
    for _ in 0..3 {
        let r = t
            .send(
                Request::builder()
                    .method(Method::POST)
                    .uri("/webhooks/photon")
                    .header("x-request-id", "fixed-id")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    }
    let n = python
        .requests()
        .into_iter()
        .filter(|r| r.path == "/webhooks/photon")
        .count();
    assert_eq!(n, 3);
}
