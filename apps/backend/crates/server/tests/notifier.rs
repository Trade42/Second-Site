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
        [
            "idempotency_key",
            "owner_id",
            "project_id",
            "purpose",
            "text"
        ]
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
    assert!(
        matches!(n.notify(&req).await, NotifyOutcome::NotSent { reason } if reason.contains("409 IDEMPOTENCY_KEY_REUSED"))
    );
    assert!(matches!(
        n.notify(&req).await,
        NotifyOutcome::NotSent { .. }
    ));
    // A 500 may come after the text went out: unknown, not "not sent".
    assert!(matches!(
        n.notify(&req).await,
        NotifyOutcome::Unknown { .. }
    ));
    assert!(matches!(
        n.notify(&req).await,
        NotifyOutcome::Unknown { .. }
    ));
    // A delivery value outside the route's vocabulary is never taken as sent.
    assert!(matches!(
        n.notify(&req).await,
        NotifyOutcome::Unknown { .. }
    ));
    let reqs = server.requests();
    assert_eq!(reqs.len(), 6, "no automatic retries");
    for r in &reqs {
        check_request(r, owner);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            v["idempotency_key"], "call-summary:1",
            "the key is passed through unchanged"
        );
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
    let out = n
        .notify(&request(Uuid::new_v4(), Uuid::new_v4(), "k"))
        .await;
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
    let out = n
        .notify(&request(Uuid::new_v4(), Uuid::new_v4(), "k"))
        .await;
    assert!(matches!(out, NotifyOutcome::NotSent { .. }), "{out:?}");
    let out = NotConfiguredNotifier
        .notify(&request(Uuid::new_v4(), Uuid::new_v4(), "k"))
        .await;
    assert!(matches!(out, NotifyOutcome::NotSent { .. }));
}

both_backends!(
    routes_notify_after_commit_and_record_what_was_not_sent,
    |db| {
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
                answers
                    .lock()
                    .unwrap()
                    .pop()
                    .unwrap_or_else(|| status(500, "x"))
            } else {
                ok_answer(Uuid::new_v4(), "recorded")
            }
        });
        let t = TestApp::new(db, &[("PYTHON_SERVICE_URL", server.base.as_str())]);
        let (token, user) = t.dev_token().await;
        let p = t.create_project(&token, &common::demo_project_body()).await;
        let pid: Uuid = p["project"]["project_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
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
        assert_eq!(
            r2.status.as_u16(),
            202,
            "the request never fails because of the notify"
        );
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
        assert_eq!(
            notified.len(),
            2,
            "only the two notifies Python did not answer 200"
        );
        let key = |r: &common::Resp| {
            format!(
                "notify-unreached:approval-notify:{}",
                r.body["approval"]["approval_id"].as_str().unwrap()
            )
        };
        let e2 = notified
            .iter()
            .find(|e| e.idempotency_key == key(&r2))
            .unwrap();
        assert_eq!(e2.payload["delivery"], "unknown");
        assert_eq!(e2.payload["purpose"], "approval_request");
        assert_eq!(e2.actor_id, "owner-messaging");
        let e3 = notified
            .iter()
            .find(|e| e.idempotency_key == key(&r3))
            .unwrap();
        assert_eq!(e3.payload["delivery"], "failed");
        for e in [e2, e3] {
            let cid: Uuid = e.payload["communication_id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            let c = comms.iter().find(|c| c.communication_id == cid).unwrap();
            assert_eq!(c.state.as_str(), "FAILED");
            assert_eq!(c.purpose, "approval_request");
            assert_eq!(c.external_participants[0]["role"], "owner");
        }
        // No record for the answered notify; three outreach drafts plus two honest records.
        assert_eq!(comms.len(), 5);
    }
);

#[tokio::test]
async fn texts_beyond_the_route_limits_are_cut_and_bad_labels_refused() {
    let server = MockServer::start_with(|_r: &Recorded| ok_answer(Uuid::new_v4(), "recorded"));
    let db = common::TestDb::new(common::Kind::Memory).await.unwrap();
    let t = TestApp::new(&db, &[]);
    let n: Arc<dyn OwnerNotifier> = Arc::new(HttpOwnerNotifier::new(&server.base, SECRET));
    t.app.extensions.insert(Arc::new(OwnerNotify::new(n)));
    let (token, user) = t.dev_token().await;
    let p = t.create_project(&token, &common::demo_project_body()).await;
    let pid: Uuid = p["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
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

/// What Python's `notify_owner` records for a key (its `communication.owner_notified` event).
async fn python_recorded(t: &TestApp, project: Uuid, key: &str, delivery: &str) {
    let mut tx = t.app.store.begin().await.unwrap();
    t.app
        .events()
        .emit(
            &mut *tx,
            ss_server::services::events::Emit::new(
                ss_server::contracts::EventType::CommunicationOwnerNotified,
                project,
                ss_server::contracts::ActorType::Agent,
                "owner-messaging",
                format!("notify:{key}"),
                json!({"communication_id": Uuid::new_v4().to_string(), "delivery": delivery,
                       "purpose": "approval_request"}),
            ),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

both_backends!(
    a_not_linked_approval_request_is_sent_once_more_after_linking,
    |db| {
        let server = MockServer::start_with(|r: &Recorded| {
            if r.path == "/internal/v1/owner/notify" {
                ok_answer(Uuid::new_v4(), "not_linked")
            } else {
                ok_answer(Uuid::new_v4(), "recorded")
            }
        });
        let t = TestApp::new(db, &[("PYTHON_SERVICE_URL", server.base.as_str())]);
        let (token, user) = t.dev_token().await;
        let p = t.create_project(&token, &common::demo_project_body()).await;
        let pid: Uuid = p["project"]["project_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        t.post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&token),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
        let props = t
            .get(&format!("/api/v1/projects/{pid}/properties"), Some(&token))
            .await;
        let prop = props.body[0]["property_id"].as_str().unwrap().to_owned();
        let outreach = |name: &'static str| {
            let (t, token, prop) = (&t, token.clone(), prop.clone());
            async move {
                let r = t
                .post(
                    &format!("/api/v1/projects/{pid}/outreach"),
                    Some(&token),
                    &json!({"property_id": prop, "channel": "VOICE",
                            "recipient": {"name": name, "phone": "+15555550100"}, "purpose": "check"}),
                )
                .await;
                assert_eq!(r.status.as_u16(), 202, "{}", r.text);
                t.app.owner_notify().idle().await;
                r.body["approval"]["approval_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }
        };
        let keys = || -> Vec<String> {
            server
                .requests()
                .into_iter()
                .filter(|r| r.path == "/internal/v1/owner/notify")
                .map(|r| {
                    serde_json::from_slice::<Value>(&r.body).unwrap()["idempotency_key"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        };
        let a = outreach("Dana").await;
        let first = format!("approval-notify:{a}");
        let linked_key = format!("approval-notify:{a}:linked");
        assert_eq!(keys(), std::slice::from_ref(&first));
        python_recorded(&t, pid, &first, "not_linked").await;
        // Repeated while the channel is still not linked: only the (deduplicated) first key.
        assert_eq!(outreach("Dana").await, a, "the approval is reused");
        assert_eq!(keys(), [first.clone(), first.clone()]);
        // The owner links the channel (Python's Photon code writes this row).
        let mut tx = t.app.store.begin().await.unwrap();
        ss_server::store::comms::OWNER_CHANNELS
            .add(
                &mut *tx,
                &ss_domain::contracts::OwnerChannel {
                    owner_channel_id: Uuid::new_v4(),
                    owner_id: user,
                    platform: "imessage".into(),
                    handle: "+15555550142".into(),
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
        outreach("Dana").await;
        assert_eq!(keys()[2..], [first.clone(), linked_key.clone()]);
        // Python answered the derived key: never again.
        python_recorded(&t, pid, &linked_key, "accepted").await;
        outreach("Dana").await;
        assert_eq!(keys()[4..], *std::slice::from_ref(&first));
        // An approval whose first answer was recorded, accepted or failed is never re-sent.
        for (name, delivery) in [("Lee", "recorded"), ("Kim", "accepted"), ("Ana", "failed")] {
            let b = outreach(name).await;
            python_recorded(&t, pid, &format!("approval-notify:{b}"), delivery).await;
            let before = keys().len();
            outreach(name).await;
            assert_eq!(
                keys()[before..],
                [format!("approval-notify:{b}")],
                "{delivery}"
            );
        }
        assert_eq!(
            keys().iter().filter(|k| k.ends_with(":linked")).count(),
            1,
            "at most once per approval"
        );
    }
);

// ---- added by the independent review (rust-server-4-review F3, F5) --------------------------

// The derived ":linked" key is sent at most once per approval: after an attempt whose outcome is
// unknown (a 5xx: Python may have texted), a repeated outreach does not send it again. After a
// definite non-send (404: refused before notify_owner) it may be sent again.
both_backends!(
    a_linked_renotify_with_an_unknown_outcome_is_not_sent_again,
    |db| {
        let server = MockServer::start_with(|r: &Recorded| {
            if r.path != "/internal/v1/owner/notify" {
                return ok_answer(Uuid::new_v4(), "recorded");
            }
            let body = serde_json::from_slice::<Value>(&r.body).unwrap();
            let key = body["idempotency_key"].as_str().unwrap().to_owned();
            if !key.ends_with(":linked") {
                ok_answer(Uuid::new_v4(), "not_linked")
            } else if body["text"].as_str().unwrap().contains("refused") {
                status(404, "NOT_FOUND")
            } else {
                status(500, "INTERNAL_ERROR")
            }
        });
        let t = TestApp::new(db, &[("PYTHON_SERVICE_URL", server.base.as_str())]);
        let (token, user) = t.dev_token().await;
        let p = t.create_project(&token, &common::demo_project_body()).await;
        let pid: Uuid = p["project"]["project_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        t.post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&token),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
        let props = t
            .get(&format!("/api/v1/projects/{pid}/properties"), Some(&token))
            .await;
        let prop = props.body[0]["property_id"].as_str().unwrap().to_owned();
        let outreach = |name: &'static str| {
            let (t, token, prop) = (&t, token.clone(), prop.clone());
            async move {
                let r = t
                .post(
                    &format!("/api/v1/projects/{pid}/outreach"),
                    Some(&token),
                    &json!({"property_id": prop, "channel": "VOICE",
                            "recipient": {"name": name, "phone": "+15555550100"}, "purpose": "check"}),
                )
                .await;
                assert_eq!(r.status.as_u16(), 202, "{}", r.text);
                t.app.owner_notify().idle().await;
                r.body["approval"]["approval_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }
        };
        let linked_sent = |a: &str| {
            server
                .requests()
                .into_iter()
                .filter(|r| {
                    serde_json::from_slice::<Value>(&r.body)
                        .map(|v| v["idempotency_key"] == format!("approval-notify:{a}:linked"))
                        .unwrap_or(false)
                })
                .count()
        };
        let a = outreach("Dana").await;
        python_recorded(&t, pid, &format!("approval-notify:{a}"), "not_linked").await;
        let mut tx = t.app.store.begin().await.unwrap();
        ss_server::store::comms::OWNER_CHANNELS
            .add(
                &mut *tx,
                &ss_domain::contracts::OwnerChannel {
                    owner_channel_id: Uuid::new_v4(),
                    owner_id: user,
                    platform: "imessage".into(),
                    handle: "+15555550142".into(),
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
        outreach("Dana").await;
        assert_eq!(linked_sent(&a), 1);
        outreach("Dana").await;
        outreach("Dana").await;
        assert_eq!(linked_sent(&a), 1, "sent again after an unknown outcome");
        // A recipient whose ":linked" request is refused (404, nothing sent) may be tried again.
        let b = outreach("refused").await;
        python_recorded(&t, pid, &format!("approval-notify:{b}"), "not_linked").await;
        outreach("refused").await;
        outreach("refused").await;
        assert_eq!(linked_sent(&b), 2);
    }
);

/// Answers 200 after looking, from its own PostgreSQL connection, for the approval its key names.
struct CommitProbe {
    opts: sqlx::postgres::PgConnectOptions,
    seen: std::sync::Mutex<Vec<bool>>,
}

#[async_trait::async_trait]
impl OwnerNotifier for CommitProbe {
    async fn notify(&self, r: &NotifyRequest) -> NotifyOutcome {
        let id = r
            .idempotency_key
            .trim_start_matches("approval-notify:")
            .to_owned();
        let mut c = sqlx::ConnectOptions::connect(&self.opts).await.unwrap();
        let n: (i64,) = sqlx::query_as("SELECT count(*) FROM approvals WHERE id::text = $1")
            .bind(&id)
            .fetch_one(&mut c)
            .await
            .unwrap();
        self.seen.lock().unwrap().push(n.0 == 1);
        NotifyOutcome::Answered {
            communication_id: Uuid::new_v4(),
            delivery: "recorded".into(),
        }
    }
    fn kind(&self) -> &'static str {
        "probe"
    }
}

// PostgreSQL only. A deferred trigger makes the outreach commit take one second; the notifier
// looks for the approval from its own connection. Sent after the commit it sees the approval;
// sent before or while the commit runs, it would not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_outreach_notify_is_sent_after_the_commit_postgres() {
    let Some(url) = common::pg_url() else {
        eprintln!("SKIPPED: TEST_DATABASE_URL is not set; PostgreSQL variant not run");
        return;
    };
    let base = <sqlx::postgres::PgConnectOptions as std::str::FromStr>::from_str(
        &ss_server::config::normalize_database_url(&url),
    )
    .unwrap();
    let schema = format!("rt_{}", Uuid::new_v4().simple());
    let mut admin = sqlx::ConnectOptions::connect(&base).await.unwrap();
    sqlx::Executor::execute(&mut admin, format!("CREATE SCHEMA {schema}").as_str())
        .await
        .unwrap();
    drop(admin);
    let opts = base
        .options([("search_path", schema.as_str())])
        .application_name(&schema);
    let store = ss_server::store::postgres::PgStore::connect_with(opts.clone(), 4)
        .await
        .unwrap();
    store.migrate().await.unwrap();
    let app = ss_server::build_app(common::test_settings(&[]), Arc::new(store));
    let probe = Arc::new(CommitProbe {
        opts: opts.clone(),
        seen: std::sync::Mutex::new(Vec::new()),
    });
    app.extensions
        .insert(Arc::new(OwnerNotify::new(probe.clone())));
    let router = ss_server::http::router(app.clone());
    let t = TestApp {
        app,
        router,
        kind: common::Kind::Postgres,
    };
    let token = t.token_for(Uuid::new_v4());
    let p = t.create_project(&token, &common::demo_project_body()).await;
    let pid = p["project"]["project_id"].as_str().unwrap().to_owned();
    t.post(
        &format!("/api/v1/projects/{pid}/properties/search"),
        Some(&token),
        &json!({"city": "Los Angeles", "state": "CA"}),
    )
    .await;
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&token))
        .await;
    let prop = props.body[0]["property_id"].as_str().unwrap().to_owned();
    let mut c = sqlx::ConnectOptions::connect(&opts).await.unwrap();
    for sql in [
        "CREATE FUNCTION slow_commit() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN PERFORM pg_sleep(1); RETURN NULL; END $$",
        "CREATE CONSTRAINT TRIGGER slow_commit AFTER INSERT ON approvals \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION slow_commit()",
    ] {
        sqlx::Executor::execute(&mut c, sql).await.unwrap();
    }
    drop(c);
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/outreach"),
            Some(&token),
            &json!({"property_id": prop, "channel": "VOICE",
                    "recipient": {"name": "D", "phone": "+15555550100"}, "purpose": "p"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 202, "{}", r.text);
    t.app.owner_notify().idle().await;
    assert_eq!(
        *probe.seen.lock().unwrap(),
        [true],
        "the notify ran before the commit"
    );
    common::drop_schema(&schema).await;
}
