//! R5 of the part 1 review: `seq > after` paging must never skip an event whose transaction
//! commits after a higher-numbered one. PostgreSQL only: the in-memory store serializes whole
//! transactions, so a second writer (and the reader) cannot even begin while the first is open.

mod common;

use std::time::Duration;

use serde_json::json;
use ss_core::{Market, ProjectContext};
use ss_server::app::AppState;
use ss_server::contracts::{ActorType, EventType};
use ss_server::services::events::{event_lock_key, Emit};
use ss_server::store::repo::{users, User, PROJECTS};
use uuid::Uuid;

use common::*;

const OVERLAP: Duration = Duration::from_millis(300);

async fn seed(app: &AppState) -> ProjectContext {
    let mut tx = app.store.begin().await.unwrap();
    let owner = Uuid::new_v4();
    users::insert(
        &mut *tx,
        &User {
            id: owner,
            display_name: "o".into(),
            email: None,
            phone: None,
        },
    )
    .await
    .unwrap();
    let p = ProjectContext::new(owner, "t", Market::new("Los Angeles", "CA"));
    PROJECTS.add(&mut *tx, &p).await.unwrap();
    tx.commit().await.unwrap();
    p
}

fn confirmed(project_id: Uuid, key: &str) -> Emit<'static> {
    Emit::new(
        EventType::ProjectProfileConfirmed,
        project_id,
        ActorType::User,
        "u",
        key,
        json!({"profile_id": Uuid::new_v4().to_string()}),
    )
}

/// One `/events?after=` poll: append what it returns and advance the cursor.
async fn poll(app: &AppState, project_id: Uuid, cursor: &mut i64, seen: &mut Vec<Uuid>) {
    let mut tx = app.store.begin().await.unwrap();
    let page = app
        .events()
        .list(&mut *tx, project_id, *cursor, 500)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    for (seq, ev) in page {
        *cursor = seq;
        seen.push(ev.event_id);
    }
}

/// The reviewer's five steps: T1 emits and stays open; T2 emits and commits; a reader pages and
/// moves its cursor; T1 commits; the reader pages again. Without the per-project advisory lock
/// the reader's cursor passed T1's lower `seq` before T1 committed, and T1's event was never
/// delivered. With the lock, T2 cannot insert until T1 has committed, so within a project `seq`
/// order equals commit order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paging_never_skips_an_event_that_commits_late() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let app = ss_server::build_app(test_settings(&[]), db.store.clone());
    let p = seed(&app).await;
    let pid = p.project_id;

    // 1. T1 emits and stays open.
    let mut t1 = app.store.begin().await.unwrap();
    let e1 = app
        .events()
        .emit(&mut *t1, confirmed(pid, "t1"))
        .await
        .unwrap()
        .expect("new event");

    // 2. T2 emits and commits (concurrently: with the lock it has to wait for T1).
    let app2 = app.clone();
    let t2 = tokio::spawn(async move {
        let mut tx = app2.store.begin().await.unwrap();
        let e = app2
            .events()
            .emit(&mut *tx, confirmed(pid, "t2"))
            .await
            .unwrap()
            .expect("new event");
        tx.commit().await.unwrap();
        e.event_id
    });
    tokio::time::sleep(OVERLAP).await;

    // 3. A reader pages and moves its cursor.
    let (mut cursor, mut seen) = (0i64, Vec::new());
    poll(&app, pid, &mut cursor, &mut seen).await;

    // 4. T1 commits; T2 finishes.
    t1.commit().await.unwrap();
    let e2 = t2.await.unwrap();

    // 5. The reader pages again.
    poll(&app, pid, &mut cursor, &mut seen).await;
    poll(&app, pid, &mut cursor, &mut seen).await;

    assert!(
        seen.contains(&e1.event_id),
        "the event of the transaction that committed last was skipped: seen {seen:?}, \
         missing {} (cursor {cursor})",
        e1.event_id
    );
    assert!(seen.contains(&e2), "seen {seen:?}");
    assert_eq!(seen.len(), 2, "{seen:?}");
    db.finish().await;
}

/// Events of different projects do not wait for each other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_projects_are_not_serialized() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let app = ss_server::build_app(test_settings(&[]), db.store.clone());
    let a = seed(&app).await;
    let b = seed(&app).await;
    let mut t1 = app.store.begin().await.unwrap();
    app.events()
        .emit(&mut *t1, confirmed(a.project_id, "a"))
        .await
        .unwrap()
        .expect("new");
    let app2 = app.clone();
    let other = tokio::spawn(async move {
        let mut tx = app2.store.begin().await.unwrap();
        app2.events()
            .emit(&mut *tx, confirmed(b.project_id, "b"))
            .await
            .unwrap()
            .expect("new");
        tx.commit().await.unwrap();
    });
    // Project B's writer finishes while project A's transaction is still open.
    tokio::time::timeout(Duration::from_secs(5), other)
        .await
        .expect("project B waited for project A's lock")
        .unwrap();
    t1.commit().await.unwrap();
    db.finish().await;
}

/// A search holds the project's event lock from `task.started` until it commits and updates the
/// `projects` row at the end of the slice; PUT business-profile updates that row first and emits
/// afterwards. Taken in opposite orders, PostgreSQL aborted one of the two with a deadlock
/// (reviewer finding: the profile update failed after `deadlock_timeout`). The search route now
/// locks the project row before its first event, so both take the row lock first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_search_and_a_profile_update_of_one_project_do_not_deadlock() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let t = TestApp::new(&db, &[]);
    let (tok, _) = t.dev_token().await;
    let created = t.create_project(&tok, &demo_project_body()).await;
    let pid: Uuid = created["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // 1. The profile update's statements in `save_profile`'s order: the project row first ...
    let mut put = t.app.store.begin().await.unwrap();
    let project = PROJECTS.require(&mut *put, pid).await.unwrap();
    PROJECTS.upsert(&mut *put, &project).await.unwrap();

    // 2. ... while a search of the same project runs.
    let router = t.router.clone();
    let path = format!("/api/v1/projects/{pid}/properties/search");
    let search = tokio::spawn(async move {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {tok}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 3. ... then the profile update emits its event and commits.
    let emitted = app_emit(&t.app, &mut *put, pid).await;
    let committed = put.commit().await;
    let body = search.await.unwrap();
    assert!(emitted.is_ok(), "profile update failed: {emitted:?}");
    assert!(committed.is_ok(), "{committed:?}");
    assert_eq!(body["task"]["status"], "COMPLETED", "{body}");
    db.finish().await;
}

async fn app_emit(
    app: &AppState,
    tx: &mut dyn ss_server::store::Tx,
    pid: Uuid,
) -> Result<(), String> {
    app.events()
        .emit(tx, confirmed(pid, "profile-update"))
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

#[test]
fn lock_keys_are_stable_and_distinct() {
    let a: Uuid = "6a8f3a52-1d3b-4f6e-9c4e-0b7c1f2a3d4e".parse().unwrap();
    let b: Uuid = "6a8f3a52-1d3b-4f6e-9c4e-0b7c1f2a3d4f".parse().unwrap();
    assert_eq!(event_lock_key(Some(a)), event_lock_key(Some(a)));
    assert_ne!(event_lock_key(Some(a)), event_lock_key(Some(b)));
    assert_ne!(event_lock_key(Some(a)), event_lock_key(None));
    // Python, for writers that adopt the lock later:
    //   int.from_bytes(hashlib.sha256(b"second-site:activity_events:" + str(pid).encode())
    //                  .digest()[:8], "big", signed=True)
    // Value printed by that Python expression (apps/api/.venv, CPython 3.12).
    assert_eq!(event_lock_key(Some(a)), -9055379089551683476);
}

/// Send one request through the router from a spawned task; returns (status, body).
fn spawn_request(
    t: &TestApp,
    method: &'static str,
    path: String,
    token: &str,
    body: serde_json::Value,
) -> tokio::task::JoinHandle<(u16, serde_json::Value)> {
    let router = t.router.clone();
    let auth = format!("Bearer {token}");
    tokio::spawn(async move {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", auth)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    })
}

fn new_profile_body() -> serde_json::Value {
    let mut profile = demo_project_body()["business_profile"].clone();
    // No `profile_id`: the PUT creates a new profile and points the project at it.
    profile["confirmation_status"] = json!("CONFIRMED");
    profile
}

/// F2 of the part 2 review (lost update). A transaction that does what the search slice does to
/// the project (lock the row, emit, set `project_status`) stays open while a PUT business-profile
/// arrives. Before the fix the PUT read the project without a lock, waited on its own update,
/// and then wrote that stale copy back: `project_status` returned to ONBOARDING. Now the PUT
/// locks the row first, waits, and writes onto the committed row: both changes survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_profile_update_that_overlaps_a_search_keeps_the_search_result() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let t = TestApp::new(&db, &[]);
    let (tok, _) = t.dev_token().await;
    let created = t.create_project(&tok, &demo_project_body()).await;
    let pid: Uuid = created["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(created["project"]["project_status"], "ONBOARDING");

    // 1. The search's statements on the project: row lock, an event, the status update ...
    let mut search = t.app.store.begin().await.unwrap();
    let (mut project, _) = PROJECTS
        .get_for_update(&mut *search, pid)
        .await
        .unwrap()
        .unwrap();
    app_emit(&t.app, &mut *search, pid).await.unwrap();
    project.project_status = ss_core::ProjectStatus::Comparing;
    PROJECTS.upsert(&mut *search, &project).await.unwrap();

    // 2. ... while the PUT arrives; it must wait for the row.
    let put = spawn_request(
        &t,
        "PUT",
        format!("/api/v1/projects/{pid}/business-profile"),
        &tok,
        new_profile_body(),
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !put.is_finished(),
        "the PUT did not wait for the project row"
    );

    // 3. The search commits; the PUT finishes.
    search.commit().await.unwrap();
    let (status, body) = put.await.unwrap();
    assert_eq!(status, 200, "{body}");
    let new_profile = body["profile_id"].as_str().unwrap().to_owned();

    let got = t.get(&format!("/api/v1/projects/{pid}"), Some(&tok)).await;
    assert_eq!(
        got.body["project"]["project_status"], "COMPARING",
        "the PUT wrote back a stale project: {}",
        got.text
    );
    assert_eq!(
        got.body["project"]["business_profile_id"],
        new_profile.as_str()
    );
    db.finish().await;
}

/// The same race with a real search over HTTP: a PUT sent while the search runs never loses the
/// search's `project_status`, and the project points at the PUT's profile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_search_and_a_profile_update_keep_both_changes() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let t = TestApp::new(&db, &[]);
    let (tok, _) = t.dev_token().await;
    for (round, delay_ms) in [5u64, 20, 40, 60, 90].into_iter().enumerate() {
        let created = t.create_project(&tok, &demo_project_body()).await;
        let pid = created["project"]["project_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let search = spawn_request(
            &t,
            "POST",
            format!("/api/v1/projects/{pid}/properties/search"),
            &tok,
            json!({"city": "Los Angeles", "state": "CA"}),
        );
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let put = spawn_request(
            &t,
            "PUT",
            format!("/api/v1/projects/{pid}/business-profile"),
            &tok,
            new_profile_body(),
        );
        let (s_status, s_body) = search.await.unwrap();
        let (p_status, p_body) = put.await.unwrap();
        assert_eq!(s_status, 202, "round {round}: {s_body}");
        assert_eq!(s_body["task"]["status"], "COMPLETED", "round {round}");
        assert_eq!(p_status, 200, "round {round}: {p_body}");
        let got = t.get(&format!("/api/v1/projects/{pid}"), Some(&tok)).await;
        assert_eq!(
            got.body["project"]["project_status"], "COMPARING",
            "round {round}: {}",
            got.text
        );
        assert_eq!(
            got.body["project"]["business_profile_id"], p_body["profile_id"],
            "round {round}"
        );
    }
    db.finish().await;
}
