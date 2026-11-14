//! Service-level tests on both stores: approvals (`test_approvals_comms.py`), events
//! (`test_events_workflow.py`: idempotent re-emit, redelivery guard, causation chain) and tasks.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Duration;
use serde_json::{json, Value};
use ss_core::{Market, ProjectContext};
use ss_server::app::{App, AppState, EventHandler};
use ss_server::contracts::{
    ActionType, ActivityEvent, ActorType, ApprovalStatus, EventType, JsonMap, Subsystem,
    TaskStatus, TaskType,
};
use ss_server::error::{AppError, AppResult};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::events::{Emit, MAX_CAUSATION_DEPTH};
use ss_server::store::repo::{users, User, APPROVALS, PROJECTS, TASKS};
use ss_server::store::Tx;
use uuid::Uuid;

use common::*;

fn obj(v: Value) -> JsonMap {
    match v {
        Value::Object(m) => m,
        _ => panic!("not an object"),
    }
}

fn app(db: &TestDb) -> AppState {
    ss_server::build_app(test_settings(&[]), db.store.clone())
}

/// A user and a project (as the Python tests seed them directly).
async fn seed(app: &App, tx: &mut dyn Tx) -> (Uuid, ProjectContext) {
    let owner = Uuid::new_v4();
    users::insert(
        tx,
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
    PROJECTS.add(tx, &p).await.unwrap();
    let _ = app;
    (owner, p)
}

fn purchase(project: Uuid, owner: Uuid, key: &str) -> ApprovalSpec {
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

both_backends!(expired_and_rejected_approvals, |db| {
    let app = app(db);
    let mut tx = app.store.begin().await.unwrap();
    let tx = &mut *tx;
    let (owner, p) = seed(&app, tx).await;
    let payload = obj(json!({"sku": "bed-1", "qty": 3}));
    let a = app
        .approvals()
        .request(tx, purchase(p.project_id, owner, "purchase-1"))
        .await
        .unwrap();
    assert_eq!(a.approval_status, ApprovalStatus::Pending);
    assert_eq!(a.action_payload_hash.len(), 64);
    assert_eq!(a.risk_category.as_str(), "CONSEQUENTIAL");
    app.approvals()
        .decide(
            tx,
            a.approval_id,
            owner,
            ApprovalStatus::Rejected,
            &a.action_payload_hash,
        )
        .await
        .unwrap();
    // The owner declined the purchase.
    let e = app
        .approvals()
        .require_approved(
            tx,
            p.project_id,
            Some(a.approval_id),
            ActionType::Purchase,
            &payload,
        )
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "APPROVAL_REQUIRED");
    assert_eq!(e.public_details()["approval_status"], "REJECTED");

    let exp = app
        .approvals()
        .request(
            tx,
            purchase(p.project_id, owner, "purchase-2").ttl(Duration::seconds(-1)),
        )
        .await
        .unwrap();
    let e = app
        .approvals()
        .decide(
            tx,
            exp.approval_id,
            owner,
            ApprovalStatus::Approved,
            &exp.action_payload_hash,
        )
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("EXPIRED") || e.to_string().contains("not allowed"),
        "{e}"
    );
    assert_eq!(e.error_code(), "INVALID_STATE_TRANSITION");
    assert_eq!(
        APPROVALS
            .require(tx, exp.approval_id)
            .await
            .unwrap()
            .approval_status,
        ApprovalStatus::Expired
    );
    // Someone else cannot decide.
    let e = app
        .approvals()
        .decide(
            tx,
            a.approval_id,
            Uuid::new_v4(),
            ApprovalStatus::Approved,
            &a.action_payload_hash,
        )
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "FORBIDDEN");
    assert_eq!(e.status().as_u16(), 403);
    // An approved-then-expired approval no longer authorizes.
    let late = app
        .approvals()
        .request(tx, purchase(p.project_id, owner, "purchase-3"))
        .await
        .unwrap();
    app.approvals()
        .decide(
            tx,
            late.approval_id,
            owner,
            ApprovalStatus::Approved,
            &late.action_payload_hash,
        )
        .await
        .unwrap();
    let mut stored = APPROVALS.require(tx, late.approval_id).await.unwrap();
    stored.expires_at = ss_core::UtcDateTime::from(chrono::Utc::now() - Duration::seconds(1));
    APPROVALS.upsert(tx, &stored).await.unwrap();
    let e = app
        .approvals()
        .require_approved(
            tx,
            p.project_id,
            Some(late.approval_id),
            ActionType::Purchase,
            &payload,
        )
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "approval is EXPIRED");
});

both_backends!(approval_enforcement_rules, |db| {
    let app = app(db);
    let mut txb = app.store.begin().await.unwrap();
    let tx = &mut *txb;
    let (owner, p) = seed(&app, tx).await;
    let payload = obj(json!({"sku": "bed-1", "qty": 3}));
    let e = app
        .approvals()
        .require_approved(tx, p.project_id, None, ActionType::Purchase, &payload)
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "APPROVAL_REQUIRED");
    assert_eq!(e.to_string(), "PURCHASE requires owner approval");
    let a = app
        .approvals()
        .request(tx, purchase(p.project_id, owner, "k1"))
        .await
        .unwrap();
    // Idempotent within the project.
    let again = app
        .approvals()
        .request(tx, purchase(p.project_id, owner, "k1"))
        .await
        .unwrap();
    assert_eq!(again.approval_id, a.approval_id);
    app.approvals()
        .decide(
            tx,
            a.approval_id,
            owner,
            ApprovalStatus::Approved,
            &a.action_payload_hash,
        )
        .await
        .unwrap();
    let e = app
        .approvals()
        .require_approved(
            tx,
            p.project_id,
            Some(a.approval_id),
            ActionType::SendMessage,
            &payload,
        )
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "approval is for a different action type");
    let e = app
        .approvals()
        .require_approved(
            tx,
            p.project_id,
            Some(a.approval_id),
            ActionType::Purchase,
            &obj(json!({"sku": "bed-1", "qty": 4})),
        )
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "APPROVAL_PAYLOAD_MISMATCH");
    assert_eq!(e.status().as_u16(), 409);
    // Same payload with keys in another order hashes the same.
    let reordered = obj(json!({"qty": 3, "sku": "bed-1"}));
    let ok = app
        .approvals()
        .require_approved(
            tx,
            p.project_id,
            Some(a.approval_id),
            ActionType::Purchase,
            &reordered,
        )
        .await
        .unwrap();
    let done = app.approvals().mark_executed(tx, ok).await.unwrap();
    assert_eq!(done.approval_status, ApprovalStatus::Executed);
    let e = app
        .approvals()
        .require_approved(
            tx,
            p.project_id,
            Some(a.approval_id),
            ActionType::Purchase,
            &payload,
        )
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "approval is EXECUTED");
    let e = app
        .approvals()
        .decide(
            tx,
            a.approval_id,
            owner,
            ApprovalStatus::Rejected,
            &a.action_payload_hash,
        )
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "INVALID_STATE_TRANSITION");
    let e = app
        .approvals()
        .require_approved(
            tx,
            p.project_id,
            Some(Uuid::new_v4()),
            ActionType::Purchase,
            &payload,
        )
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "NOT_FOUND");
    txb.commit().await.unwrap();
});

both_backends!(approval_from_another_project_cannot_authorize, |db| {
    let app = app(db);
    let mut txb = app.store.begin().await.unwrap();
    let tx = &mut *txb;
    let (owner, a_project) = seed(&app, tx).await;
    let b_project = ProjectContext::new(owner, "b", Market::new("Los Angeles", "CA"));
    PROJECTS.add(tx, &b_project).await.unwrap();
    let payload = obj(json!({"sku": "bed-1", "qty": 3}));
    let b_appr = app
        .approvals()
        .request(tx, purchase(b_project.project_id, owner, "shared-key"))
        .await
        .unwrap();
    app.approvals()
        .decide(
            tx,
            b_appr.approval_id,
            owner,
            ApprovalStatus::Approved,
            &b_appr.action_payload_hash,
        )
        .await
        .unwrap();
    // Approved in project B, presented for project A: refused exactly like a missing approval.
    let e = app
        .approvals()
        .require_approved(
            tx,
            a_project.project_id,
            Some(b_appr.approval_id),
            ActionType::Purchase,
            &payload,
        )
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "NOT_FOUND");
    assert_eq!(
        e.to_string(),
        format!("ApprovalRequest {} not found", b_appr.approval_id)
    );
    // B's idempotency key does not hand B's approval to A.
    let e = app
        .approvals()
        .request(tx, purchase(a_project.project_id, owner, "shared-key"))
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "IDEMPOTENCY_KEY_CONFLICT");
    assert_eq!(e.status().as_u16(), 409);
    // Still usable in its own project.
    let ok = app
        .approvals()
        .require_approved(
            tx,
            b_project.project_id,
            Some(b_appr.approval_id),
            ActionType::Purchase,
            &payload,
        )
        .await
        .unwrap();
    assert_eq!(ok.approval.approval_id, b_appr.approval_id);
});

both_backends!(stale_execution_is_refused, |db| {
    // Two executions that both read the approval as APPROVED: only the first can spend it.
    let app = app(db);
    let mut txb = app.store.begin().await.unwrap();
    let tx = &mut *txb;
    let (owner, p) = seed(&app, tx).await;
    let payload = obj(json!({"sku": "bed-1", "qty": 3}));
    let a = app
        .approvals()
        .request(tx, purchase(p.project_id, owner, "k"))
        .await
        .unwrap();
    app.approvals()
        .decide(
            tx,
            a.approval_id,
            owner,
            ApprovalStatus::Approved,
            &a.action_payload_hash,
        )
        .await
        .unwrap();
    let first = app
        .approvals()
        .require_approved(
            tx,
            p.project_id,
            Some(a.approval_id),
            ActionType::Purchase,
            &payload,
        )
        .await
        .unwrap();
    let second = app
        .approvals()
        .require_approved(
            tx,
            p.project_id,
            Some(a.approval_id),
            ActionType::Purchase,
            &payload,
        )
        .await
        .unwrap();
    app.approvals().mark_executed(tx, first).await.unwrap();
    let e = app.approvals().mark_executed(tx, second).await.unwrap_err();
    assert_eq!(e.error_code(), "STALE_VERSION");
    assert_eq!(e.status().as_u16(), 409);
});

#[tokio::test]
async fn concurrent_executions_on_postgres_cannot_both_pass() {
    // Real concurrency needs two connections; the in-memory store serializes transactions.
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let app = app(&db);
    let mut tx = app.store.begin().await.unwrap();
    let (owner, p) = seed(&app, &mut *tx).await;
    let a = app
        .approvals()
        .request(&mut *tx, purchase(p.project_id, owner, "k"))
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
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
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
    db.finish().await;
}

/// Records calls; emits a result event keyed `"<handler>:<cause>"` unless already handled.
struct Recompute {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EventHandler for Recompute {
    async fn handle(&self, app: &App, tx: &mut dyn Tx, cause: &ActivityEvent) -> AppResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if app
            .events()
            .already_handled(tx, cause, EventType::PropertyAnalysisCompleted)
            .await?
        {
            return Ok(());
        }
        let property_id = cause.payload["property_id"].clone();
        app.events()
            .emit(
                tx,
                Emit::new(
                    EventType::PropertyAnalysisCompleted,
                    cause.project_id,
                    ActorType::Agent,
                    "recompute",
                    format!("recompute:{}", cause.event_id),
                    json!({"property_id": property_id, "assessment_ids": []}),
                )
                .caused_by(cause),
            )
            .await?;
        Ok(())
    }
}

struct RankAfterAnalysis;

#[async_trait]
impl EventHandler for RankAfterAnalysis {
    async fn handle(&self, app: &App, tx: &mut dyn Tx, cause: &ActivityEvent) -> AppResult<()> {
        app.events()
            .emit(
                tx,
                Emit::new(EventType::PropertyRankingUpdated, cause.project_id, ActorType::Agent, "ranking", format!("ranking:{}", cause.event_id), json!({"ranking_id": Uuid::new_v4().to_string(), "ranking_version": 2, "top_property_id": null}))
                    .caused_by(cause),
            )
            .await?;
        Ok(())
    }
}

both_backends!(event_chain_idempotency_and_redelivery, |db| {
    let app = app(db);
    let calls = Arc::new(AtomicUsize::new(0));
    app.subscribe(
        EventType::InfrastructureEvidenceUpdated,
        "recompute",
        Arc::new(Recompute {
            calls: calls.clone(),
        }),
    );
    // Registering the same name twice is a no-op.
    app.subscribe(
        EventType::InfrastructureEvidenceUpdated,
        "recompute",
        Arc::new(Recompute {
            calls: calls.clone(),
        }),
    );
    app.subscribe(
        EventType::PropertyAnalysisCompleted,
        "ranking",
        Arc::new(RankAfterAnalysis),
    );
    let mut txb = app.store.begin().await.unwrap();
    let tx = &mut *txb;
    let (_, p) = seed(&app, tx).await;
    let property = Uuid::new_v4();
    let evidence = Uuid::new_v4();
    let emit = || {
        Emit::new(
            EventType::InfrastructureEvidenceUpdated,
            p.project_id,
            ActorType::Agent,
            "voice",
            format!("evidence:{evidence}"),
            json!({"property_id": property.to_string(), "evidence_ids": [evidence.to_string()], "reason": "property manager answered electrical question"}),
        )
    };
    let cause = app
        .events()
        .emit(tx, emit())
        .await
        .unwrap()
        .expect("new event");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let chain = app
        .events()
        .by_correlation(tx, cause.correlation_id)
        .await
        .unwrap();
    let types: Vec<&str> = chain.iter().map(|(_, e)| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "infrastructure.evidence_updated",
            "property.analysis.completed",
            "property.ranking.updated"
        ]
    );
    // Causation tracked hop by hop; seq strictly increasing.
    assert_eq!(chain[1].1.causation_id, Some(chain[0].1.event_id));
    assert_eq!(chain[2].1.causation_id, Some(chain[1].1.event_id));
    assert!(chain[0].0 < chain[1].0 && chain[1].0 < chain[2].0);
    assert!(chain
        .iter()
        .all(|(_, e)| e.correlation_id == cause.correlation_id));
    // Re-emitting the same logical event is a no-op; redelivery does not re-run the handler's work.
    assert!(app.events().emit(tx, emit()).await.unwrap().is_none());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no dispatch for a duplicate"
    );
    app.events().dispatch(tx, &cause).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        app.events()
            .by_correlation(tx, cause.correlation_id)
            .await
            .unwrap()
            .len(),
        3,
        "nothing new"
    );
    // Stored envelopes round-trip and validate.
    let stored = app.events().get(tx, cause.event_id).await.unwrap().unwrap();
    assert_eq!(stored, cause);
    assert!(app
        .events()
        .get(tx, Uuid::new_v4())
        .await
        .unwrap()
        .is_none());
    txb.commit().await.unwrap();
});

both_backends!(causation_depth_limit, |db| {
    let app = app(db);
    let mut txb = app.store.begin().await.unwrap();
    let tx = &mut *txb;
    let (_, p) = seed(&app, tx).await;
    let ev = |i: usize| {
        Emit::new(
            EventType::LogisticsTimelineUpdated,
            p.project_id,
            ActorType::System,
            "t",
            format!("hop:{i}"),
            json!({"milestone": format!("m{i}")}),
        )
        .without_dispatch()
    };
    let mut last = app.events().emit(tx, ev(0)).await.unwrap().unwrap();
    for i in 1..MAX_CAUSATION_DEPTH {
        last = app
            .events()
            .emit(tx, ev(i).caused_by(&last))
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(
        app.events().depth(tx, Some(last.event_id)).await.unwrap(),
        MAX_CAUSATION_DEPTH
    );
    let e = app
        .events()
        .emit(tx, ev(99).caused_by(&last))
        .await
        .unwrap_err();
    assert_eq!(e.status().as_u16(), 500);
    assert_eq!(e.python_class_name(), "RuntimeError");
});

struct Failing;

#[async_trait]
impl EventHandler for Failing {
    async fn handle(&self, _app: &App, _tx: &mut dyn Tx, _e: &ActivityEvent) -> AppResult<()> {
        Err(AppError::conflict("subscriber failed"))
    }
}

both_backends!(subscriber_failure_rolls_back_the_producer, |db| {
    // Python's observable behaviour (kept on purpose): the handler error propagates and the
    // request transaction, including the producer's event, rolls back.
    let app = app(db);
    app.subscribe(
        EventType::LogisticsTimelineUpdated,
        "failing",
        Arc::new(Failing),
    );
    let pid = {
        let mut tx = app.store.begin().await.unwrap();
        let (_, p) = seed(&app, &mut *tx).await;
        tx.commit().await.unwrap();
        p.project_id
    };
    let mut tx = app.store.begin().await.unwrap();
    let e = app
        .events()
        .emit(
            &mut *tx,
            Emit::new(
                EventType::LogisticsTimelineUpdated,
                pid,
                ActorType::System,
                "t",
                "k",
                json!({"milestone": "m"}),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "subscriber failed");
    drop(tx);
    let mut tx = app.store.begin().await.unwrap();
    assert!(app
        .events()
        .list(&mut *tx, pid, 0, 100)
        .await
        .unwrap()
        .is_empty());
});

both_backends!(event_payload_validation_at_emit, |db| {
    let app = app(db);
    let mut txb = app.store.begin().await.unwrap();
    let tx = &mut *txb;
    let (_, p) = seed(&app, tx).await;
    let e = app
        .events()
        .emit(
            tx,
            Emit::new(
                EventType::TaskStarted,
                p.project_id,
                ActorType::System,
                "x",
                "k1",
                json!({"wrong": 1}),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "CONTRACT_VALIDATION_ERROR");
    let e = app.events().emit(tx, Emit::new(EventType::CallOwnerHandoffRequested, p.project_id, ActorType::Agent, "x", "k2", json!({"call_id": Uuid::new_v4().to_string(), "handoff_request_id": Uuid::new_v4().to_string(), "reason": "X", "join_token": "t"}))).await.unwrap_err();
    assert!(e.to_string().contains("tokens or secrets"), "{e}");
    // Normalized payload (defaults filled in).
    let ok = app.events().emit(tx, Emit::new(EventType::TaskStarted, p.project_id, ActorType::System, "x", "k3", json!({"task_id": Uuid::new_v4().to_string(), "task_type": "x", "status": "RUNNING"}))).await.unwrap().unwrap();
    assert_eq!(ok.payload["error_code"], Value::Null);
    assert_eq!(ok.schema_version, "1.0");
});

both_backends!(task_run_success_and_failure, |db| {
    let app = app(db);
    let mut txb = app.store.begin().await.unwrap();
    let tx = &mut *txb;
    let (_, p) = seed(&app, tx).await;
    let task = app
        .tasks()
        .create(
            tx,
            p.project_id,
            TaskType::SearchProperties,
            Subsystem::PropertySpatial,
            obj(json!({"city": "Los Angeles"})),
        )
        .await
        .unwrap();
    assert_eq!(task.status, TaskStatus::Queued);
    let done = app
        .tasks()
        .run(tx, task.clone(), |_, t| {
            let id = t.task_id;
            Box::pin(async move { Ok(obj(json!({"property_ids": [], "task": id.to_string()}))) })
        })
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    assert_eq!(done.attempt_count, 1);
    assert_eq!(done.output_reference["task"], task.task_id.to_string());
    let stored = TASKS.require(tx, task.task_id).await.unwrap();
    assert_eq!(stored, done);
    // A finished task cannot run again.
    let e = app
        .tasks()
        .run(tx, done, |_, _| Box::pin(async { Ok(JsonMap::new()) }))
        .await
        .unwrap_err();
    assert_eq!(e.error_code(), "INVALID_STATE_TRANSITION");

    // Failure: the work's writes are rolled back (savepoint), the task is FAILED.
    let t2 = app
        .tasks()
        .create(
            tx,
            p.project_id,
            TaskType::SearchProperties,
            Subsystem::PropertySpatial,
            JsonMap::new(),
        )
        .await
        .unwrap();
    let written = Uuid::new_v4();
    let failed = app
        .tasks()
        .run(tx, t2.clone(), move |tx, _| {
            Box::pin(async move {
                users::insert(
                    tx,
                    &User {
                        id: written,
                        display_name: "inside".into(),
                        email: None,
                        phone: None,
                    },
                )
                .await?;
                Err(AppError::not_found("no business profile for this project"))
            })
        })
        .await
        .unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("NotFoundError"));
    assert_eq!(
        failed.error_message.as_deref(),
        Some("no business profile for this project")
    );
    assert!(
        users::get(tx, written).await.unwrap().is_none(),
        "work rolled back"
    );
    // A database error inside the work (unique violation) is also contained by the savepoint.
    let t3 = app
        .tasks()
        .create(
            tx,
            p.project_id,
            TaskType::RankProperties,
            Subsystem::AgentsFinance,
            JsonMap::new(),
        )
        .await
        .unwrap();
    let owner = p.owner_id;
    let failed = app
        .tasks()
        .run(tx, t3, move |tx, _| {
            Box::pin(async move {
                users::insert(
                    tx,
                    &User {
                        id: owner,
                        display_name: "dup".into(),
                        email: None,
                        phone: None,
                    },
                )
                .await?;
                Ok(JsonMap::new())
            })
        })
        .await
        .unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("ConflictError"));
    // Internal errors never reach the record.
    let t4 = app
        .tasks()
        .create(
            tx,
            p.project_id,
            TaskType::RankProperties,
            Subsystem::AgentsFinance,
            JsonMap::new(),
        )
        .await
        .unwrap();
    let failed = app
        .tasks()
        .run(tx, t4, |_, _| {
            Box::pin(async { Err(AppError::internal("password=hunter2")) })
        })
        .await
        .unwrap();
    assert_eq!(
        failed.error_message.as_deref(),
        Some("unexpected server error")
    );
    // Long messages are cut at 500 characters.
    let t5 = app
        .tasks()
        .create(
            tx,
            p.project_id,
            TaskType::RankProperties,
            Subsystem::AgentsFinance,
            JsonMap::new(),
        )
        .await
        .unwrap();
    let failed = app
        .tasks()
        .run(tx, t5, |_, _| {
            Box::pin(async { Err(AppError::bad_request("é".repeat(800))) })
        })
        .await
        .unwrap();
    assert_eq!(failed.error_message.as_ref().unwrap().chars().count(), 500);
    assert_eq!(failed.error_code.as_deref(), Some("AppError"));

    let events = app.events().list(tx, p.project_id, 0, 500).await.unwrap();
    let keys: Vec<String> = events
        .iter()
        .filter(|(_, e)| e.payload["task_id"] == task.task_id.to_string())
        .map(|(_, e)| e.idempotency_key.clone())
        .collect();
    assert_eq!(
        keys,
        [
            format!("task:{}:RUNNING:1", task.task_id),
            format!("task:{}:COMPLETED:1", task.task_id)
        ]
    );
    let failed_events: Vec<&ActivityEvent> = events
        .iter()
        .map(|(_, e)| e)
        .filter(|e| e.payload["task_id"] == t2.task_id.to_string())
        .collect();
    assert_eq!(failed_events.len(), 2);
    assert_eq!(failed_events[1].event_type, EventType::TaskFailed);
    assert_eq!(failed_events[1].payload["error_code"], "NotFoundError");
    assert_eq!(failed_events[1].correlation_id, t2.correlation_id);
    assert_eq!(failed_events[1].actor_id, "task-service");
    txb.commit().await.unwrap();
});
