//! REVIEW (temporary, part 4 independent review): the owner notifier against a slow mock of the
//! Python route. Sent after the commit, no lock or connection held while waiting, no automatic
//! retry, honest records, token shape, and the "linked later" re-notify.

mod common;

use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use ss_server::contracts::{ActorType, EventType};
use ss_server::services::events::Emit;
use ss_server::services::notifier::{HttpOwnerNotifier, OwnerNotify};
use ss_server::store::Query;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer, Recorded};
use common::TestApp;

const SECRET: &str = "test-secret-not-for-production-0123456789";

async fn pg_app(pool: u32) -> Option<(TestApp, String)> {
    let url = common::pg_url()?;
    let base = sqlx::postgres::PgConnectOptions::from_str(
        &ss_server::config::normalize_database_url(&url),
    )
    .unwrap();
    let schema = format!("rt_{}", Uuid::new_v4().simple());
    let mut admin = sqlx::ConnectOptions::connect(&base).await.unwrap();
    sqlx::Executor::execute(&mut admin, format!("CREATE SCHEMA {schema}").as_str())
        .await
        .unwrap();
    drop(admin);
    let store = ss_server::store::postgres::PgStore::connect_with(
        base.options([("search_path", schema.as_str())])
            .application_name(&schema),
        pool,
    )
    .await
    .unwrap();
    store.migrate().await.unwrap();
    let store: Arc<dyn ss_server::store::Store> = Arc::new(store);
    let app = ss_server::build_app(common::test_settings(&[]), store);
    let router = ss_server::http::router(app.clone());
    Some((
        TestApp {
            app,
            router,
            kind: common::Kind::Postgres,
        },
        schema,
    ))
}

/// Sessions of this test's schema that are inside a transaction right now.
async fn in_transaction(schema: &str) -> i64 {
    let url = common::pg_url().unwrap();
    let base = sqlx::postgres::PgConnectOptions::from_str(
        &ss_server::config::normalize_database_url(&url),
    )
    .unwrap();
    let mut c = sqlx::ConnectOptions::connect(&base).await.unwrap();
    let row: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_stat_activity WHERE application_name = $1 \
         AND state IN ('active', 'idle in transaction')",
    )
    .bind(schema)
    .fetch_one(&mut c)
    .await
    .unwrap();
    row.0
}

async fn project(t: &TestApp, token: &str) -> (Uuid, String) {
    let p = t.create_project(token, &common::demo_project_body()).await;
    let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(token),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 202);
    let props = t.get(&format!("/api/v1/projects/{pid}/properties"), Some(token)).await;
    (pid, props.body[0]["property_id"].as_str().unwrap().to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn slow_python_blocks_nothing_postgres() {
    let Some((t, schema)) = pg_app(4).await else {
        eprintln!("SKIPPED");
        return;
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let h2 = hits.clone();
    let mock = MockServer::start_with(move |r: &Recorded| {
        h2.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(2500));
        let body: Value = serde_json::from_slice(&r.body).unwrap();
        let delivery = if body["idempotency_key"].as_str().unwrap().contains("fail500") {
            return Canned {
                status: 500,
                content_type: Some("application/json".into()),
                body: br#"{"error_code":"INTERNAL_ERROR"}"#.to_vec(),
            };
        } else {
            "recorded"
        };
        Canned {
            status: 200,
            content_type: Some("application/json".into()),
            body: serde_json::to_vec(&json!({"communication_id": Uuid::new_v4(), "delivery": delivery})).unwrap(),
        }
    });
    t.app.extensions.insert(Arc::new(OwnerNotify::new(Arc::new(
        HttpOwnerNotifier::with_timeouts(&mock.base, SECRET, Duration::from_secs(2), Duration::from_secs(10)),
    ))));
    let owner = Uuid::new_v4();
    let token = t.token_for(owner);
    let (pid, prop) = project(&t, &token).await;
    // Five outreaches in a row, each answered at once although Python takes 2.5 s per notify.
    let start = Instant::now();
    let mut slowest = Duration::ZERO;
    for i in 0..5 {
        let t0 = Instant::now();
        let r = t
            .post(
                &format!("/api/v1/projects/{pid}/outreach"),
                Some(&token),
                &json!({"property_id": prop, "channel": "VOICE",
                        "recipient": {"name": format!("N{i}"), "phone": "+15555550100"},
                        "purpose": "p"}),
            )
            .await;
        assert_eq!(r.status.as_u16(), 202, "{}", r.text);
        slowest = slowest.max(t0.elapsed());
    }
    // While the deliveries wait on the mock: other requests of the same project, no open transaction.
    let mut reads = Duration::ZERO;
    for path in ["communications", "calls", "handoffs", "approvals", "events?limit=50"] {
        let t0 = Instant::now();
        let r = t.get(&format!("/api/v1/projects/{pid}/{path}"), Some(&token)).await;
        assert_eq!(r.status.as_u16(), 200, "{path}");
        reads = reads.max(t0.elapsed());
    }
    let open = in_transaction(&schema).await;
    let in_flight_hits = hits.load(Ordering::SeqCst);
    println!(
        "REVIEW slow notify: 5 outreach answers, slowest {:?}; 5 reads during deliveries, slowest {:?}; \
         sessions of this schema in a transaction meanwhile: {open}; mock hits so far {in_flight_hits}; elapsed {:?}",
        slowest, reads, start.elapsed()
    );
    assert!(slowest < Duration::from_millis(2000));
    assert_eq!(open, 0);
    t.app.owner_notify().idle().await;
    println!("REVIEW all deliveries done after {:?}; mock hits {}", start.elapsed(), hits.load(Ordering::SeqCst));
    assert_eq!(hits.load(Ordering::SeqCst), 5);
    // The token: purpose service, the owner's id, short expiry.
    let reqs = mock.requests();
    let auth = reqs[0].headers.get("authorization").unwrap().clone();
    let tok = auth.strip_prefix("Bearer ").unwrap().to_owned();
    assert_eq!(ss_server::security::verify_token(SECRET, &tok, "service").unwrap(), owner);
    assert!(ss_server::security::verify_token(SECRET, &tok, "access").is_err());
    let payload = tok.split('.').next().unwrap();
    let raw = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload.trim_end_matches('=')).unwrap();
    let exp: f64 = String::from_utf8(raw).unwrap().rsplit('.').next().unwrap().parse().unwrap();
    let ttl = exp - chrono::Utc::now().timestamp() as f64;
    println!("REVIEW service token ttl remaining {ttl:.0}s; signature tail for the log search: TOKENTAIL={}", &tok[tok.len() - 24..]);
    assert!(ttl <= 121.0);
    // Non-2xx and timeout: one request each, never retried.
    let before = hits.load(Ordering::SeqCst);
    let req = ss_server::services::notifier::NotifyRequest {
        owner_id: owner,
        project_id: pid,
        text: "x".into(),
        purpose: "p".into(),
        idempotency_key: "review-fail500".into(),
    };
    t.app.owner_notify().dispatch(t.app.clone(), vec![req.clone()]);
    t.app.owner_notify().idle().await;
    let quick = HttpOwnerNotifier::with_timeouts(&mock.base, SECRET, Duration::from_secs(2), Duration::from_millis(500));
    t.app.extensions.insert(Arc::new(OwnerNotify::new(Arc::new(quick))));
    let mut req2 = req.clone();
    req2.idempotency_key = "review-timeout".into();
    t.app.owner_notify().dispatch(t.app.clone(), vec![req2]);
    t.app.owner_notify().idle().await;
    std::thread::sleep(Duration::from_millis(2600));
    let after = hits.load(Ordering::SeqCst);
    let mut tx = t.app.store.begin().await.unwrap();
    let evs = ss_server::store::repo::events::select(&mut *tx, Query::new().eq("project_id", pid)).await.unwrap();
    tx.commit().await.unwrap();
    let recs: Vec<(String, Value)> = evs
        .iter()
        .filter_map(|r| ss_server::store::repo::events::parse(r).ok())
        .filter(|e| e.idempotency_key.starts_with("notify-unreached:"))
        .map(|e| (e.idempotency_key.clone(), e.payload.get("delivery").cloned().unwrap_or(Value::Null)))
        .collect();
    println!("REVIEW 500 and timeout: mock requests {} (expected 2); Rust records {recs:?}", after - before);
    assert_eq!(after - before, 2);
    common::drop_schema(&schema).await;
}

both_backends!(linked_renotify_after_an_unknown_outcome, |db| {
    let t = TestApp::new(db, &[]);
    let hits: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let h2 = hits.clone();
    let status = Arc::new(AtomicUsize::new(500));
    let s2 = status.clone();
    let mock = MockServer::start_with(move |r: &Recorded| {
        let body: Value = serde_json::from_slice(&r.body).unwrap();
        h2.lock().unwrap().push(body["idempotency_key"].as_str().unwrap().to_owned());
        let code = s2.load(Ordering::SeqCst) as u16;
        if code == 200 {
            Canned { status: 200, content_type: Some("application/json".into()), body: serde_json::to_vec(&json!({"communication_id": Uuid::new_v4(), "delivery": "recorded"})).unwrap() }
        } else {
            Canned { status: code, content_type: Some("application/json".into()), body: b"{}".to_vec() }
        }
    });
    t.app.extensions.insert(Arc::new(OwnerNotify::new(Arc::new(HttpOwnerNotifier::new(&mock.base, SECRET)))));
    let owner = Uuid::new_v4();
    let token = t.token_for(owner);
    let (pid, prop) = project(&t, &token).await;
    let body = json!({"property_id": prop, "channel": "VOICE",
                      "recipient": {"name": "Dana", "phone": "+15555550100"}, "purpose": "p"});
    let out = t.post(&format!("/api/v1/projects/{pid}/outreach"), Some(&token), &body).await;
    t.app.owner_notify().idle().await;
    let appr = out.body["approval"]["approval_id"].as_str().unwrap().to_owned();
    // What Python's notify_owner would have recorded for the first key: not_linked.
    let mut tx = t.app.store.begin().await.unwrap();
    t.app
        .events()
        .emit(&mut *tx, Emit::new(EventType::CommunicationOwnerNotified, pid, ActorType::Agent, "owner-messaging",
            format!("notify:approval-notify:{appr}"),
            json!({"communication_id": Uuid::new_v4().to_string(), "delivery": "not_linked", "purpose": "approval_request"})))
        .await
        .unwrap();
    ss_server::store::comms::OWNER_CHANNELS
        .add(&mut *tx, &ss_domain::contracts::OwnerChannel {
            owner_channel_id: Uuid::new_v4(),
            owner_id: owner,
            platform: "imessage".into(),
            handle: "+15555550142".into(),
            status: ss_domain::contracts::OwnerChannelStatus::Linked,
            space_id: Some("spc".into()),
            agent_user_id: Some("usr".into()),
            last_inbound_message_id: None,
            linked_at: None,
            updated_at: ss_core::UtcDateTime::now(),
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Repeat 1: the :linked key is sent; Python answers 500 (it may have texted).
    t.post(&format!("/api/v1/projects/{pid}/outreach"), Some(&token), &body).await;
    t.app.owner_notify().idle().await;
    // Repeat 2 (the owner clicks again): is the :linked key sent a second time?
    t.post(&format!("/api/v1/projects/{pid}/outreach"), Some(&token), &body).await;
    t.app.owner_notify().idle().await;
    let keys = hits.lock().unwrap().clone();
    let linked = keys.iter().filter(|k| k.ends_with(":linked")).count();
    println!("REVIEW linked re-notify after a 500: keys sent {keys:?}; ':linked' sent {linked} time(s)");
    // Also after a timeout-like unknown, and after not_sent (404): count again.
    let _ = status.swap(200, Ordering::SeqCst);
    t.post(&format!("/api/v1/projects/{pid}/outreach"), Some(&token), &body).await;
    t.app.owner_notify().idle().await;
    let keys = hits.lock().unwrap().clone();
    println!("REVIEW after Python recovers: ':linked' sent {} time(s) in total", keys.iter().filter(|k| k.ends_with(":linked")).count());
});
