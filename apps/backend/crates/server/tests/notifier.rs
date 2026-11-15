//! Server part 4: the owner notifier's HTTP client against an in-process mock of the Python route
//! `POST /internal/v1/owner/notify` (path, body and `service` bearer token checked on every
//! request), the honest records Rust writes when the Python service was not reached, and the
//! route-level wiring (notify after the commit, never blocking the request).

mod common;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};
use ss_server::services::notifier::{
    HttpOwnerNotifier, NotConfiguredNotifier, NotifyOutcome, NotifyRequest, OwnerNotifier,
    OwnerNotify,
};
use ss_server::store::comms::COMMUNICATIONS;
use ss_server::store::Query;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer, Recorded};
use common::{TestApp, SECRET};

fn ok_answer(communication_id: Uuid, delivery: &str) -> Canned {
    Canned {
        status: 200,
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(
            &json!({"communication_id": communication_id, "delivery": delivery}),
        )
        .unwrap(),
    }
}

fn status(code: u16, error_code: &str) -> Canned {
    Canned {
        status: code,
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&json!({"error_code": error_code, "message": "x",
            "details": {}, "request_id": null, "retryable": false}))
        .unwrap(),
    }
}

/// What the route requires of every request (python-link review, "What the client requires").
fn check_request(r: &Recorded, owner: Uuid) {
    assert_eq!(r.method, "POST");
    assert_eq!(r.path, "/internal/v1/owner/notify");
    assert!(r
        .headers
        .get("content-type")
        .unwrap()
        .starts_with("application/json"));
    let auth = r.headers.get("authorization").expect("bearer token");
    let token = auth.strip_prefix("Bearer ").expect("bearer");
    assert_eq!(
        ss_server::security::verify_token(SECRET, token, "service").unwrap(),
        owner,
        "a service token for the owner"
    );
    assert!(ss_server::security::verify_token(SECRET, token, "access").is_err());
    let body = token.split('.').next().unwrap();
    let decoded = String::from_utf8(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .unwrap(),
    )
    .unwrap();
    let exp: i64 = decoded.rsplit('.').next().unwrap().parse().unwrap();
    let left = exp - chrono::Utc::now().timestamp();
    assert!((0..=125).contains(&left), "short-lived token: {left} s");
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    let mut keys = keys;
    keys.sort();
    assert_eq!(
        keys,
        ["idempotency_key", "owner_id", "project_id", "purpose", "text"]
    );
}

fn request(owner: Uuid, project: Uuid, key: &str) -> NotifyRequest {
    NotifyRequest {
        owner_id: owner,
        project_id: project,
        text: "hello".into(),
        purpose: "call_summary".into(),
        idempotency_key: key.into(),
    }
}

#[tokio::test]
async fn http_client_outcomes() {
    let owner = Uuid::new_v4();
    let project = Uuid::new_v4();
    let comm = Uuid::new_v4();
    let server = MockServer::start(vec![
        ok_answer(comm, "accepted"),
        status(409, "IDEMPOTENCY_KEY_REUSED"),
        status(422, "REQUEST_VALIDATION_ERROR"),
        status(500, "INTERNAL_ERROR"),
        Canned {
            status: 200,
            content_type: Some("application/json".into()),
            body: br#"{"communication_id": "nope"}"#.to_vec(),
        },
        ok_answer(comm, "delivered"),
    ]);
    let n = HttpOwnerNotifier::new(&format!("{}/", server.base), SECRET);
    assert_eq!(n.url(), format!("{}/internal/v1/owner/notify", server.base));
    let req = request(owner, project, "call-summary:1");
    assert_eq!(
        n.notify(&req).await,
        NotifyOutcome::Answered {
            communication_id: comm,
            delivery: "accepted".into()
        }
    );
    assert!(matches!(n.notify(&req).await, NotifyOutcome::NotSent { reason } if reason.contains("409 IDEMPOTENCY_KEY_REUSED")));
    assert!(matches!(n.notify(&req).await, NotifyOutcome::NotSent { .. }));
    // A 500 may come after the text went out: unknown, not "not sent".
    assert!(matches!(n.notify(&req).await, NotifyOutcome::Unknown { .. }));
    assert!(matches!(n.notify(&req).await, NotifyOutcome::Unknown { .. }));
    // A delivery value outside the route's vocabulary is never taken as sent.
    assert!(matches!(n.notify(&req).await, NotifyOutcome::Unknown { .. }));
    let reqs = server.requests();
    assert_eq!(reqs.len(), 6, "no automatic retries");
    for r in &reqs {
        check_request(r, owner);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["idempotency_key"], "call-summary:1", "the key is passed through unchanged");
        assert_eq!(v["owner_id"], owner.to_string());
        assert_eq!(v["project_id"], project.to_string());
        assert_eq!(v["text"], "hello");
        assert_eq!(v["purpose"], "call_summary");
    }
}

#[tokio::test]
async fn timeouts_are_unknown_and_refused_connections_not_sent() {
    let server = MockServer::start_with(|_r: &Recorded| {
        std::thread::sleep(Duration::from_millis(1500));
        ok_answer(Uuid::new_v4(), "recorded")
    });
    let n = HttpOwnerNotifier::with_timeouts(
        &server.base,
        SECRET,
        Duration::from_millis(500),
        Duration::from_millis(400),
    );
    let out = n.notify(&request(Uuid::new_v4(), Uuid::new_v4(), "k")).await;
    assert!(matches!(out, NotifyOutcome::Unknown { .. }), "{out:?}");
    // Nothing listens on this port any more.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let n = HttpOwnerNotifier::with_timeouts(
        &format!("http://127.0.0.1:{closed}"),
        SECRET,
        Duration::from_millis(500),
        Duration::from_millis(500),
    );
    let out = n.notify(&request(Uuid::new_v4(), Uuid::new_v4(), "k")).await;
    assert!(matches!(out, NotifyOutcome::NotSent { .. }), "{out:?}");
    let out = NotConfiguredNotifier
        .notify(&request(Uuid::new_v4(), Uuid::new_v4(), "k"))
        .await;
    assert!(matches!(out, NotifyOutcome::NotSent { .. }));
}

both_backends!(routes_notify_after_commit_and_record_what_was_not_sent, |db| {
    let comm = Uuid::new_v4();
    // Notify requests get these answers in order; anything else (the forwarder's health probe of
    // the same service) gets a plain 200.
    let answers = std::sync::Mutex::new(vec![
        status(404, "NOT_FOUND"),
        status(500, "INTERNAL_ERROR"),
        ok_answer(comm, "recorded"),
    ]);
    let server = MockServer::start_with(move |r: &Recorded| {
        if r.path == "/internal/v1/owner/notify" {
            answers.lock().unwrap().pop().unwrap_or_else(|| status(500, "x"))
        } else {
            ok_answer(Uuid::new_v4(), "recorded")
        }
    });
    let t = TestApp::new(db, &[("PYTHON_SERVICE_URL", server.base.as_str())]);
    let (token, user) = t.dev_token().await;
    let p = t
        .create_project(&token, &common::demo_project_body())
        .await;
    let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&token),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 202);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&token))
        .await;
    let prop = props.body[0]["property_id"].as_str().unwrap().to_owned();
    let outreach = |n: u32| {
        let t = &t;
        let token = token.clone();
        let prop = prop.clone();
        async move {
            t.post(
                &format!("/api/v1/projects/{pid}/outreach"),
                Some(&token),
                &json!({"property_id": prop, "channel": "VOICE",
                        "recipient": {"name": format!("Broker {n}"), "phone": "+15555550100"},
                        "purpose": "check"}),
            )
            .await
        }
    };
    // 1: Python answers 200: Python wrote its record, Rust writes none.
    let r1 = outreach(1).await;
    assert_eq!(r1.status.as_u16(), 202, "{}", r1.text);
    t.app.owner_notify().idle().await;
    // 2: Python answers 500: outcome unknown, recorded as such.
    let r2 = outreach(2).await;
    assert_eq!(r2.status.as_u16(), 202, "the request never fails because of the notify");
    t.app.owner_notify().idle().await;
    // 3: Python refuses (404): not sent, recorded as failed.
    let r3 = outreach(3).await;
    assert_eq!(r3.status.as_u16(), 202);
    t.app.owner_notify().idle().await;
    let reqs: Vec<Recorded> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/internal/v1/owner/notify")
        .collect();
    assert_eq!(reqs.len(), 3);
    for (r, out) in reqs.iter().zip([&r1, &r2, &r3]) {
        check_request(r, user);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["purpose"], "approval_request");
        assert_eq!(
            v["idempotency_key"],
            format!(
                "approval-notify:{}",
                out.body["approval"]["approval_id"].as_str().unwrap()
            )
        );
    }
    let mut tx = t.app.store.begin().await.unwrap();
    let comms = COMMUNICATIONS
        .list(&mut *tx, Query::new().eq("project_id", pid))
        .await
        .unwrap();
    let events = t.app.events().list(&mut *tx, pid, 0, 500).await.unwrap();
    tx.commit().await.unwrap();
    let notified: Vec<_> = events
        .iter()
        .filter(|(_, e)| e.event_type.as_str() == "communication.owner_notified")
        .map(|(_, e)| e.clone())
        .collect();
    assert_eq!(notified.len(), 2, "only the two notifies Python did not answer 200");
    let key = |r: &common::Resp| {
        format!(
            "notify-unreached:approval-notify:{}",
            r.body["approval"]["approval_id"].as_str().unwrap()
        )
    };
    let e2 = notified.iter().find(|e| e.idempotency_key == key(&r2)).unwrap();
    assert_eq!(e2.payload["delivery"], "unknown");
    assert_eq!(e2.payload["purpose"], "approval_request");
    assert_eq!(e2.actor_id, "owner-messaging");
    let e3 = notified.iter().find(|e| e.idempotency_key == key(&r3)).unwrap();
    assert_eq!(e3.payload["delivery"], "failed");
    for e in [e2, e3] {
        let cid: Uuid = e.payload["communication_id"].as_str().unwrap().parse().unwrap();
        let c = comms.iter().find(|c| c.communication_id == cid).unwrap();
        assert_eq!(c.state.as_str(), "FAILED");
        assert_eq!(c.purpose, "approval_request");
        assert_eq!(c.external_participants[0]["role"], "owner");
    }
    // No record for the answered notify; three outreach drafts plus two honest records.
    assert_eq!(comms.len(), 5);
});

#[tokio::test]
async fn texts_beyond_the_route_limits_are_cut_and_bad_labels_refused() {
    let server = MockServer::start_with(|_r: &Recorded| ok_answer(Uuid::new_v4(), "recorded"));
    let db = common::TestDb::new(common::Kind::Memory).await.unwrap();
    let t = TestApp::new(&db, &[]);
    let n: Arc<dyn OwnerNotifier> = Arc::new(HttpOwnerNotifier::new(&server.base, SECRET));
    t.app.extensions.insert(Arc::new(OwnerNotify::new(n)));
    let (token, user) = t.dev_token().await;
    let p = t
        .create_project(&token, &common::demo_project_body())
        .await;
    let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
    let long = "é".repeat(2000);
    t.app.owner_notify().dispatch(
        t.app.clone(),
        vec![
            NotifyRequest {
                owner_id: user,
                project_id: pid,
                text: long,
                purpose: "call_summary".into(),
                idempotency_key: "k-long".into(),
            },
            NotifyRequest {
                owner_id: user,
                project_id: pid,
                text: "hi".into(),
                purpose: "call_summary".into(),
                idempotency_key: "clé".into(),
            },
        ],
    );
    t.app.owner_notify().idle().await;
    let reqs: Vec<Recorded> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/internal/v1/owner/notify")
        .collect();
    assert_eq!(reqs.len(), 1, "the non-ASCII key is refused before calling");
    let v: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let text = v["text"].as_str().unwrap();
    assert_eq!(text.chars().count(), 1500);
    assert!(text.ends_with('…'));
    let mut tx = t.app.store.begin().await.unwrap();
    assert!(t
        .app
        .events()
        .exists(&mut *tx, "notify-unreached:clé")
        .await
        .unwrap());
    tx.commit().await.unwrap();
    db.finish().await;
}
