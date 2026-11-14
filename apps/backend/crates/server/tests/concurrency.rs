//! Concurrent writers, isolation and mid-handler rollback, on both stores (added by the
//! independent review). On PostgreSQL every test uses real concurrent connections; the in-memory
//! store serializes transactions, so there the same assertions check that serialization.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{json, Value};
use ss_core::{Market, ProjectContext};
use ss_server::app::AppState;
use ss_server::contracts::{ActionType, ActorType, ApprovalStatus, EventType, JsonMap};
use ss_server::error::AppError;
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::events::Emit;
use ss_server::store::repo::{events, users, User, APPROVALS, PROJECTS};
use ss_server::store::Query;
use uuid::Uuid;

use common::*;

const OVERLAP: Duration = Duration::from_millis(150);

fn obj(v: Value) -> JsonMap {
    match v {
        Value::Object(m) => m,
        _ => panic!("not an object"),
    }
}

fn app(db: &TestDb) -> AppState {
    ss_server::build_app(test_settings(&[]), db.store.clone())
}

async fn seed(app: &AppState) -> (Uuid, ProjectContext) {
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
    (owner, p)
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

async fn all_seqs(app: &AppState, project_id: Uuid) -> Vec<i64> {
    let mut tx = app.store.begin().await.unwrap();
    let rows = events::select(&mut *tx, Query::new().eq("project_id", project_id))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows.iter().map(|r| r.seq).collect()
}

both_backends!(seq_is_strictly_increasing_under_concurrent_writers, |db| {
    let app = app(db);
    let (_, p) = seed(&app).await;
    let mut handles = Vec::new();
    for w in 0..8u32 {
        let app = app.clone();
        let pid = p.project_id;
        handles.push(tokio::spawn(async move {
            let mut tx = app.store.begin().await.unwrap();
            for i in 0..10u32 {
                app.events()
                    .emit(&mut *tx, confirmed(pid, &format!("w{w}:e{i}")))
                    .await
                    .unwrap()
                    .expect("new key");
                tokio::task::yield_now().await;
            }
            // Every third writer rolls back: its identity values become gaps.
            if w % 3 == 0 {
                tx.rollback().await.unwrap();
                0
            } else {
                tx.commit().await.unwrap();
                10
            }
        }));
    }
    let mut committed = 0;
    for h in handles {
        committed += h.await.unwrap();
    }
    let seqs = all_seqs(&app, p.project_id).await;
    assert_eq!(seqs.len(), committed);
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    // Rolled-back writers left gaps; the list tolerates them.
    let span = (seqs[seqs.len() - 1] - seqs[0] + 1) as usize;
    assert!(span > seqs.len(), "expected gaps, got {seqs:?}");
    let mut tx = app.store.begin().await.unwrap();
    let page = app
        .events()
        .list(&mut *tx, p.project_id, seqs[4], 500)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        seqs[5..].to_vec()
    );
});

both_backends!(concurrent_emits_of_one_key_store_one_event, |db| {
    let app = app(db);
    let (_, p) = seed(&app).await;
    let run = |app: AppState, pid: Uuid| async move {
        let mut tx = app.store.begin().await?;
        let ev = app
            .events()
            .emit(&mut *tx, confirmed(pid, "same-key"))
            .await?;
        tokio::time::sleep(OVERLAP).await;
        tx.commit().await?;
        Ok::<bool, AppError>(ev.is_some())
    };
    let (a, b) = tokio::join!(
        tokio::spawn(run(app.clone(), p.project_id)),
        tokio::spawn(run(app.clone(), p.project_id))
    );
    let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());
    assert!(a ^ b, "exactly one emit is new: {a} {b}");
    assert_eq!(all_seqs(&app, p.project_id).await.len(), 1);
});

fn spec(project: Uuid, owner: Uuid, key: &str) -> ApprovalSpec {
    ApprovalSpec::new(
        project,
        owner,
        "procurement",
        ActionType::Purchase,
        obj(json!({"sku": "bed-1", "qty": 3})),
        "Buy 3 beds",
        key,
    )
}

both_backends!(concurrent_approval_requests_of_one_key_store_one, |db| {
    let app = app(db);
    let (owner, p) = seed(&app).await;
    let run = |app: AppState, pid: Uuid| async move {
        let mut tx = app.store.begin().await?;
        let a = app
            .approvals()
            .request(&mut *tx, spec(pid, owner, "k1"))
            .await?;
        tokio::time::sleep(OVERLAP).await;
        tx.commit().await?;
        Ok::<Uuid, AppError>(a.approval_id)
    };
    let (a, b) = tokio::join!(
        tokio::spawn(run(app.clone(), p.project_id)),
        tokio::spawn(run(app.clone(), p.project_id))
    );
    let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());
    assert_eq!(a, b, "both callers get the one stored approval");
    let mut tx = app.store.begin().await.unwrap();
    let stored = APPROVALS
        .list(&mut *tx, Query::new().eq("project_id", p.project_id))
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    tx.rollback().await.unwrap();
    // One APPROVAL_REQUESTED event, not two.
    assert_eq!(all_seqs(&app, p.project_id).await.len(), 1);
});

// Python's `decide` does not lock the row: two concurrent decisions could both succeed.
both_backends!(concurrent_decisions_cannot_both_succeed, |db| {
    let app = app(db);
    let (owner, p) = seed(&app).await;
    let mut tx = app.store.begin().await.unwrap();
    let a = app
        .approvals()
        .request(&mut *tx, spec(p.project_id, owner, "k2"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let run = |app: AppState, decision: ApprovalStatus, hash: String| async move {
        let mut tx = app.store.begin().await?;
        let d = app
            .approvals()
            .decide(&mut *tx, a.approval_id, owner, decision, &hash)
            .await?;
        tokio::time::sleep(OVERLAP).await;
        tx.commit().await?;
        Ok::<ApprovalStatus, AppError>(d.approval_status)
    };
    let h = a.action_payload_hash.clone();
    let (x, y) = tokio::join!(
        tokio::spawn(run(app.clone(), ApprovalStatus::Approved, h.clone())),
        tokio::spawn(run(app.clone(), ApprovalStatus::Rejected, h))
    );
    let results = [x.unwrap(), y.unwrap()];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    let err = results.iter().find_map(|r| r.as_ref().err()).unwrap();
    assert_eq!(err.error_code(), "INVALID_STATE_TRANSITION", "{err:?}");
    // One APPROVAL_DECIDED event besides APPROVAL_REQUESTED.
    assert_eq!(all_seqs(&app, p.project_id).await.len(), 2);
});

both_backends!(
    concurrent_executions_cannot_both_pass_on_either_store,
    |db| {
        let app = app(db);
        let (owner, p) = seed(&app).await;
        let mut tx = app.store.begin().await.unwrap();
        let a = app
            .approvals()
            .request(&mut *tx, spec(p.project_id, owner, "k3"))
            .await
            .unwrap();
        app.approvals()
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
        let (pid, aid) = (p.project_id, a.approval_id);
        let run = move |app: AppState| async move {
            let payload = obj(json!({"sku": "bed-1", "qty": 3}));
            let mut tx = app.store.begin().await?;
            let ok = app
                .approvals()
                .require_approved(&mut *tx, pid, Some(aid), ActionType::Purchase, &payload)
                .await?;
            tokio::time::sleep(OVERLAP).await;
            app.approvals().mark_executed(&mut *tx, ok).await?;
            tx.commit().await?;
            Ok::<(), AppError>(())
        };
        let (r1, r2) = tokio::join!(
            tokio::spawn(run(app.clone())),
            tokio::spawn(run(app.clone()))
        );
        let results = [r1.unwrap(), r2.unwrap()];
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        let err = results.iter().find_map(|r| r.as_ref().err()).unwrap();
        assert_eq!(err.to_string(), "approval is EXECUTED");
    }
);

both_backends!(stale_version_is_refused_under_concurrency, |db| {
    let app = app(db);
    let (_, p) = seed(&app).await;
    let run = |app: AppState, name: &'static str| async move {
        let mut tx = app.store.begin().await?;
        let (mut proj, version) = PROJECTS
            .get_versioned(&mut *tx, p.project_id)
            .await?
            .expect("project");
        tokio::time::sleep(OVERLAP).await;
        proj.project_name = name.into();
        let v = PROJECTS.update_versioned(&mut *tx, &proj, version).await?;
        tx.commit().await?;
        Ok::<i64, AppError>(v)
    };
    let (x, y) = tokio::join!(
        tokio::spawn(run(app.clone(), "first")),
        tokio::spawn(run(app.clone(), "second"))
    );
    let results = [x.unwrap(), y.unwrap()];
    match db.kind {
        // Both read version 1 concurrently; the second writer must be refused.
        Kind::Postgres => {
            assert_eq!(
                results.iter().filter(|r| r.is_ok()).count(),
                1,
                "{results:?}"
            );
            let err = results.iter().find_map(|r| r.as_ref().err()).unwrap();
            assert_eq!(err.error_code(), "STALE_VERSION");
        }
        // Serialized: the second transaction reads version 2 and succeeds.
        Kind::Memory => assert!(results.iter().all(|r| r.is_ok()), "{results:?}"),
    }
});

both_backends!(
    uncommitted_writes_are_invisible_to_other_transactions,
    |db| {
        let app = app(db);
        let id = Uuid::new_v4();
        let mut tx = app.store.begin().await.unwrap();
        users::insert(
            &mut *tx,
            &User {
                id,
                display_name: "pending".into(),
                email: None,
                phone: None,
            },
        )
        .await
        .unwrap();
        let reader = {
            let app = app.clone();
            tokio::spawn(async move {
                let mut tx = app.store.begin().await.unwrap();
                let seen = users::get(&mut *tx, id).await.unwrap();
                tx.rollback().await.unwrap();
                seen
            })
        };
        tokio::time::sleep(OVERLAP).await;
        if db.kind == Kind::Memory {
            // The in-memory store serializes: the reader is still waiting for the writer.
            assert!(!reader.is_finished());
        }
        tx.rollback().await.unwrap();
        assert_eq!(reader.await.unwrap(), None);
    }
);

both_backends!(
    concurrent_first_requests_of_new_users_and_dev_tokens,
    |db| {
        let t = TestApp::new(db, &[]);
        let body = json!({"project_name": "p", "market": {"city": "Los Angeles", "state": "CA"}});
        for _ in 0..5 {
            let token = t.token_for(Uuid::new_v4());
            let (a, b, c) = tokio::join!(
                t.post("/api/v1/projects", Some(&token), &body),
                t.post("/api/v1/projects", Some(&token), &body),
                t.post("/api/v1/projects", Some(&token), &body)
            );
            for r in [&a, &b, &c] {
                assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            }
            let list = t.get("/api/v1/projects", Some(&token)).await;
            assert_eq!(list.body.as_array().map(Vec::len), Some(3));
        }
        let (a, b, c, d) = tokio::join!(
            t.call(axum::http::Method::POST, "/api/v1/dev/token", None, None),
            t.call(axum::http::Method::POST, "/api/v1/dev/token", None, None),
            t.call(axum::http::Method::POST, "/api/v1/dev/token", None, None),
            t.call(axum::http::Method::POST, "/api/v1/dev/token", None, None)
        );
        let ids: Vec<&Value> = [&a, &b, &c, &d]
            .iter()
            .map(|r| &r.body["user_id"])
            .collect();
        for r in [&a, &b, &c, &d] {
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        }
        assert!(ids.iter().all(|i| *i == ids[0]), "{ids:?}");
    }
);

both_backends!(a_failing_multi_write_handler_leaves_nothing_behind, |db| {
    let t = TestApp::new(db, &[]);
    let (token, _) = t.dev_token().await;
    let first = t.create_project(&token, &demo_project_body()).await;
    let profile_id = first["business_profile"]["profile_id"].clone();
    let before_events = t
        .get(
            &format!(
                "/api/v1/projects/{}/events",
                first["project"]["project_id"].as_str().unwrap()
            ),
            Some(&token),
        )
        .await;
    // POST /projects writes the users row and the project, then finds the client-chosen
    // profile id taken and fails: the request's transaction must roll every write back.
    let mut body = demo_project_body();
    body["business_profile"]["profile_id"] = profile_id;
    let r = t.post("/api/v1/projects", Some(&token), &body).await;
    assert_problem(&r, 404, "NOT_FOUND");
    let list = t.get("/api/v1/projects", Some(&token)).await;
    assert_eq!(list.body.as_array().map(Vec::len), Some(1), "{}", list.text);
    let after_events = t
        .get(
            &format!(
                "/api/v1/projects/{}/events",
                first["project"]["project_id"].as_str().unwrap()
            ),
            Some(&token),
        )
        .await;
    assert_eq!(before_events.body, after_events.body);
    // A brand-new user whose first request fails leaves no users row behind either.
    let newcomer = Uuid::new_v4();
    let r = t
        .post("/api/v1/projects", Some(&t.token_for(newcomer)), &body)
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    let mut tx = t.app.store.begin().await.unwrap();
    assert_eq!(users::get(&mut *tx, newcomer).await.unwrap(), None);
});
