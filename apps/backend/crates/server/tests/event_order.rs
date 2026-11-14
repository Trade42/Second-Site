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
