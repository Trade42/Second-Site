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
