//! REVIEW (temporary, part 4 independent review): the approval gate at the action boundary.
//! Every refused case asserts zero dials on the recording fixture carrier, no call row, and the
//! approval unchanged.

mod common;

use std::str::FromStr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{Duration, Utc};
use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_core::UtcDateTime;
use ss_domain::integrations::telnyx::FixtureTelephonyProvider;
use ss_server::contracts::{ActionType, ApprovalStatus};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::call_orchestration::{CallOrchestrator, StartCall};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::store::comms::{CALLS, COMMUNICATIONS};
use ss_server::store::repo::APPROVALS;
use ss_server::store::Query;
use tower::ServiceExt;
use uuid::Uuid;

use common::{demo_project_body, TestApp, TestDb};

fn rcpt(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

async fn project(t: &TestApp, owner: Uuid) -> Uuid {
    let token = t.token_for(owner);
    let p = t
        .create_project(
            &token,
            &json!({"project_name": "t", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    p["project"]["project_id"].as_str().unwrap().parse().unwrap()
}

/// An approval for `payload_project` / `recipient` / `property`, stored in `pid`, decided
/// APPROVED, then given `status` (and an expiry in the past when `expired`).
#[allow(clippy::too_many_arguments)]
async fn approval(
    t: &TestApp,
    pid: Uuid,
    owner: Uuid,
    payload_project: Uuid,
    recipient: &IndexMap<String, String>,
    property: Option<Uuid>,
    action: ActionType,
    status: Option<ApprovalStatus>,
    expired: bool,
) -> Uuid {
    let payload = ss_domain::communications::call_payload(payload_project, recipient, property);
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
                action,
                payload,
                "call",
                format!("rv-{}", Uuid::new_v4()),
            ),
        )
        .await
        .unwrap();
    let mut a = if status == Some(ApprovalStatus::Pending) {
        a
    } else {
        t.app
            .approvals()
            .decide(
                &mut *tx,
                a.approval_id,
                owner,
                ApprovalStatus::Approved,
                &a.action_payload_hash,
            )
            .await
            .unwrap()
    };
    if let Some(s) = status {
        a.approval_status = s;
    }
    if expired {
        a.expires_at = UtcDateTime::from_datetime(Utc::now() - Duration::seconds(5));
    }
    APPROVALS.upsert(&mut *tx, &a).await.unwrap();
    tx.commit().await.unwrap();
    a.approval_id
}

async fn approval_status(t: &TestApp, id: Uuid) -> ApprovalStatus {
    let mut tx = t.app.store.begin().await.unwrap();
    let a = APPROVALS.require(&mut *tx, id).await.unwrap();
    tx.commit().await.unwrap();
    a.approval_status
}

async fn call_rows(t: &TestApp, pid: Uuid) -> usize {
    let mut tx = t.app.store.begin().await.unwrap();
    let n = CALLS
        .list(&mut *tx, Query::new().eq("project_id", pid))
        .await
        .unwrap()
        .len();
    tx.commit().await.unwrap();
    n
}

fn fixture(t: &TestApp) -> Arc<FixtureTelephonyProvider> {
    t.app.voice().fixture.clone().expect("fixture telephony")
}

both_backends!(gate_matrix_direct, |db| {
    let t = TestApp::new(db, &[]);
    let fx = fixture(&t);
    let owner = Uuid::new_v4();
    let pid = project(&t, owner).await;
    let pid_other = project(&t, owner).await;
    let stranger = Uuid::new_v4();
    let pid_stranger = project(&t, stranger).await;
    let prop = Uuid::new_v4();
    let prop2 = Uuid::new_v4();
    let base = rcpt(&[("role", "broker"), ("name", "Dana Broker"), ("phone", "+15555550100")]);
    let pc = ActionType::PlaceCall;

    // (label, approval id, recipient at start, property at start, expected status, code)
    let mut cases: Vec<(String, Option<Uuid>, IndexMap<String, String>, Option<Uuid>, u16, &str)> =
        Vec::new();
    cases.push(("no approval".into(), None, base.clone(), Some(prop), 409, "APPROVAL_REQUIRED"));
    cases.push((
        "unknown approval".into(),
        Some(Uuid::new_v4()),
        base.clone(),
        Some(prop),
        404,
        "NOT_FOUND",
    ));
    for (label, st, expired) in [
        ("PENDING", Some(ApprovalStatus::Pending), false),
        ("REJECTED", Some(ApprovalStatus::Rejected), false),
        ("EXPIRED", Some(ApprovalStatus::Expired), false),
        ("CANCELLED", Some(ApprovalStatus::Cancelled), false),
        ("EXECUTED", Some(ApprovalStatus::Executed), false),
        ("APPROVED but past expires_at", None, true),
        ("PENDING and past expires_at", Some(ApprovalStatus::Pending), true),
    ] {
        let id = approval(&t, pid, owner, pid, &base, Some(prop), pc, st, expired).await;
        cases.push((label.into(), Some(id), base.clone(), Some(prop), 409, "APPROVAL_REQUIRED"));
    }
    for other_action in [
        ActionType::SendMessage,
        ActionType::ContactSupplier,
        ActionType::Purchase,
        ActionType::FinancialCommitment,
        ActionType::ApproveLayout,
        ActionType::ApproveBom,
    ] {
        let id = approval(&t, pid, owner, pid, &base, Some(prop), other_action, None, false).await;
        cases.push((
            format!("action {}", other_action.as_str()),
            Some(id),
            base.clone(),
            Some(prop),
            409,
            "APPROVAL_REQUIRED",
        ));
    }
    // Another project of the same owner, and a stranger's project.
    let foreign = approval(&t, pid_other, owner, pid_other, &base, Some(prop), pc, None, false).await;
    cases.push(("same owner, other project".into(), Some(foreign), base.clone(), Some(prop), 404, "NOT_FOUND"));
    let foreign2 =
        approval(&t, pid_stranger, stranger, pid_stranger, &base, Some(prop), pc, None, false).await;
    cases.push(("stranger's project".into(), Some(foreign2), base.clone(), Some(prop), 404, "NOT_FOUND"));
    // An approval stored in this project whose payload names another project.
    let cross = approval(&t, pid, owner, pid_other, &base, Some(prop), pc, None, false).await;
    cases.push(("payload names another project".into(), Some(cross), base.clone(), Some(prop), 409, "APPROVAL_PAYLOAD_MISMATCH"));
    // One valid approval, every field of the payload changed at start.
    let good = approval(&t, pid, owner, pid, &base, Some(prop), pc, None, false).await;
    let mut variants: Vec<(&str, IndexMap<String, String>, Option<Uuid>)> = vec![
        ("other phone", rcpt(&[("role", "broker"), ("name", "Dana Broker"), ("phone", "+15555550999")]), Some(prop)),
        ("phone formatted differently", rcpt(&[("role", "broker"), ("name", "Dana Broker"), ("phone", "+1 555-555-0100")]), Some(prop)),
        ("phone with trailing space", rcpt(&[("role", "broker"), ("name", "Dana Broker"), ("phone", "+15555550100 ")]), Some(prop)),
        ("other name", rcpt(&[("role", "broker"), ("name", "Dana Brokers"), ("phone", "+15555550100")]), Some(prop)),
        ("name case", rcpt(&[("role", "broker"), ("name", "dana broker"), ("phone", "+15555550100")]), Some(prop)),
        ("other role", rcpt(&[("role", "landlord"), ("name", "Dana Broker"), ("phone", "+15555550100")]), Some(prop)),
        ("extra key", rcpt(&[("role", "broker"), ("name", "Dana Broker"), ("phone", "+15555550100"), ("email", "d@x.com")]), Some(prop)),
        ("missing name", rcpt(&[("role", "broker"), ("phone", "+15555550100")]), Some(prop)),
        ("other property", base.clone(), Some(prop2)),
        ("no property", base.clone(), None),
    ];
    for (label, r, p) in variants.drain(..) {
        cases.push((format!("payload: {label}"), Some(good), r, p, 409, "APPROVAL_PAYLOAD_MISMATCH"));
    }
    let mut statuses_before = Vec::new();
    for (_, a, _, _, _, _) in &cases {
        if let Some(a) = a {
            let mut tx = t.app.store.begin().await.unwrap();
            let s = APPROVALS.get(&mut *tx, *a).await.unwrap().map(|x| x.approval_status);
            tx.commit().await.unwrap();
            statuses_before.push(s);
        } else {
            statuses_before.push(None);
        }
    }
    let n = cases.len();
    for (i, (label, a, r, p, status, code)) in cases.into_iter().enumerate() {
        let mut tx = t.app.store.begin().await.unwrap();
        let mut orch = CallOrchestrator::new(&t.app);
        let err = orch
            .start(
                &mut *tx,
                StartCall {
                    project_id: pid,
                    owner_id: owner,
                    recipient: r,
                    property_id: p,
                    approval_id: a,
                    intro_recording_reference: None,
                    communication_id: None,
                    simulated: false,
                },
            )
            .await
            .expect_err(&label);
        drop(tx); // the route rolls back on error
        println!("REVIEW gate case {label:?}: {} {}", err.status().as_u16(), err.error_code());
        assert_eq!(err.status().as_u16(), status, "{label}: {err:?}");
        assert_eq!(err.error_code(), code, "{label}");
        assert_eq!(fx.calls().len(), 0, "{label}: dialed");
        assert_eq!(call_rows(&t, pid).await, 0, "{label}: stored a call");
        if let Some(a) = a {
            let mut tx = t.app.store.begin().await.unwrap();
            let s = APPROVALS.get(&mut *tx, a).await.unwrap().map(|x| x.approval_status);
            tx.commit().await.unwrap();
            assert_eq!(s, statuses_before[i], "{label}: approval changed");
        }
    }
    println!("REVIEW gate cases refused with 0 dials: {n}");
    // The good approval still works exactly once.
    for round in 0..2 {
        let mut tx = t.app.store.begin().await.unwrap();
        let mut orch = CallOrchestrator::new(&t.app);
        let r = orch
            .start(
                &mut *tx,
                StartCall {
                    project_id: pid,
                    owner_id: owner,
                    recipient: base.clone(),
                    property_id: Some(prop),
                    approval_id: Some(good),
                    intro_recording_reference: None,
                    communication_id: None,
                    simulated: false,
                },
            )
            .await;
        if round == 0 {
            r.unwrap();
            tx.commit().await.unwrap();
        } else {
            assert_eq!(r.unwrap_err().error_code(), "APPROVAL_REQUIRED");
        }
    }
    assert_eq!(fx.calls().len(), 1);
    assert_eq!(approval_status(&t, good).await, ApprovalStatus::Executed);
    // Key order of the recipient does not matter (canonical hash), and is not a refusal case.
});

struct Ctx {
    t: TestApp,
    token: String,
}

impl Ctx {
    fn new(t: TestApp) -> Ctx {
        let fake = Arc::new(RecordingNotifier::new());
        t.app.extensions.insert(Arc::new(OwnerNotify::new(fake)));
        let token = t.token_for(Uuid::new_v4());
        Ctx { t, token }
    }

    async fn project(&self) -> (Uuid, String) {
        let p = self.t.create_project(&self.token, &demo_project_body()).await;
        let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
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
            .get(&format!("/api/v1/projects/{pid}/properties"), Some(&self.token))
            .await;
        let prop = props.body.as_array().unwrap()[0]["property_id"]
            .as_str()
            .unwrap()
            .to_owned();
        (pid, prop)
    }

    async fn outreach(&self, pid: Uuid, prop: &str, name: &str, phone: &str) -> Value {
        let r = self
            .t
            .post(
                &format!("/api/v1/projects/{pid}/outreach"),
                Some(&self.token),
                &json!({"property_id": prop, "channel": "VOICE",
                        "recipient": {"role": "broker", "name": name, "phone": phone},
                        "purpose": "Verify electrical"}),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
        r.body
    }

    async fn approve(&self, out: &Value) {
        let r = self
            .t
            .post(
                &format!("/api/v1/approvals/{}/decision", out["approval"]["approval_id"].as_str().unwrap()),
                Some(&self.token),
                &json!({"decision": "APPROVED", "action_payload_hash": out["approval"]["action_payload_hash"]}),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    }

    async fn start(&self, pid: Uuid, comm: &str) -> common::Resp {
        self.t
            .post(
                &format!("/api/v1/projects/{pid}/calls"),
                Some(&self.token),
                &json!({"communication_id": comm}),
            )
            .await
    }
}

both_backends!(gate_through_the_route, |db| {
    let c = Ctx::new(TestApp::new(db, &[]));
    let fx = fixture(&c.t);
    let (pid, prop) = c.project().await;
    let a = c.outreach(pid, &prop, "Dana", "+15555550100").await;
    let b = c.outreach(pid, &prop, "Eve", "+15555550200").await;
    c.approve(&a).await;
    let a_comm = a["communication"]["communication_id"].as_str().unwrap().to_owned();
    let b_comm = b["communication"]["communication_id"].as_str().unwrap().to_owned();
    // B is still PENDING.
    let r = c.start(pid, &b_comm).await;
    println!("REVIEW route: pending B -> {} {}", r.status, r.body["error_code"]);
    assert_eq!(r.status.as_u16(), 409);
    // Tamper: B's communication points at A's (approved) approval: payload mismatch.
    let a_appr: Uuid = a["approval"]["approval_id"].as_str().unwrap().parse().unwrap();
    let mut tx = c.t.app.store.begin().await.unwrap();
    let mut bc = COMMUNICATIONS
        .require(&mut *tx, b_comm.parse().unwrap())
        .await
        .unwrap();
    bc.approval_id = Some(a_appr);
    COMMUNICATIONS.upsert(&mut *tx, &bc).await.unwrap();
    // A copy of A's communication for another property, and one without an approval.
    let mut ac = COMMUNICATIONS
        .require(&mut *tx, a_comm.parse().unwrap())
        .await
        .unwrap();
    let mut a_other_prop = ac.clone();
    a_other_prop.communication_id = Uuid::new_v4();
    a_other_prop.property_id = Some(Uuid::new_v4());
    COMMUNICATIONS.add(&mut *tx, &a_other_prop).await.unwrap();
    let mut a_no_appr = ac.clone();
    a_no_appr.communication_id = Uuid::new_v4();
    a_no_appr.approval_id = None;
    COMMUNICATIONS.add(&mut *tx, &a_no_appr).await.unwrap();
    let mut a_no_participant = ac.clone();
    a_no_participant.communication_id = Uuid::new_v4();
    a_no_participant.external_participants = Vec::new();
    COMMUNICATIONS.add(&mut *tx, &a_no_participant).await.unwrap();
    ac.communication_id = Uuid::new_v4();
    ac.external_participants[0].insert("phone".into(), "+15555550999".into());
    COMMUNICATIONS.add(&mut *tx, &ac).await.unwrap();
    tx.commit().await.unwrap();
    for (label, comm, code) in [
        ("B -> A's approval", b_comm.clone(), "APPROVAL_PAYLOAD_MISMATCH"),
        ("A for another property", a_other_prop.communication_id.to_string(), "APPROVAL_PAYLOAD_MISMATCH"),
        ("A without approval", a_no_appr.communication_id.to_string(), "APPROVAL_REQUIRED"),
        ("A without participant", a_no_participant.communication_id.to_string(), "APPROVAL_PAYLOAD_MISMATCH"),
        ("A with another phone", ac.communication_id.to_string(), "APPROVAL_PAYLOAD_MISMATCH"),
    ] {
        let r = c.start(pid, &comm).await;
        println!("REVIEW route case {label}: {} {}", r.status, r.body["error_code"]);
        assert_eq!(r.status.as_u16(), 409, "{label}: {}", r.text);
        assert_eq!(r.body["error_code"], code, "{label}");
        assert_eq!(fx.calls().len(), 0, "{label}: dialed");
    }
    // A communication of another owner's project, and of the same owner's other project.
    let c2 = Ctx::new(TestApp {
        app: c.t.app.clone(),
        router: c.t.router.clone(),
        kind: c.t.kind,
    });
    let (pid2, prop2) = c2.project().await;
    let x = c2.outreach(pid2, &prop2, "Dana", "+15555550100").await;
    c2.approve(&x).await;
    let x_comm = x["communication"]["communication_id"].as_str().unwrap().to_owned();
    let r = c.start(pid, &x_comm).await;
    println!("REVIEW route: stranger's communication under own project -> {} {}", r.status, r.body["error_code"]);
    assert_eq!(r.status.as_u16(), 404);
    let r = c.start(pid2, &x_comm).await;
    assert_eq!(r.status.as_u16(), 404, "{}", r.text);
    // Same owner, second project.
    let (pid3, _) = c.project().await;
    let r = c.start(pid3, &a_comm).await;
    println!("REVIEW route: own communication under own other project -> {} {}", r.status, r.body["error_code"]);
    assert_eq!(r.status.as_u16(), 404);
    assert_eq!(fx.calls().len(), 0);
    // A works once.
    assert_eq!(c.start(pid, &a_comm).await.status.as_u16(), 202);
    assert_eq!(c.start(pid, &a_comm).await.status.as_u16(), 409);
    assert_eq!(fx.calls().len(), 1);
});

/// A PostgreSQL store with a pool large enough for 20 concurrent requests.
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
    let opts = base
        .options([("search_path", schema.as_str())])
        .application_name(&schema);
    let store = ss_server::store::postgres::PgStore::connect_with(opts, pool)
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

async fn twenty_starts(c: &Ctx, pid: Uuid, comms: Vec<String>) -> Vec<u16> {
    let mut handles = Vec::new();
    for i in 0..20 {
        let comm = comms[i % comms.len()].clone();
        let router = c.t.router.clone();
        let token = c.token.clone();
        handles.push(tokio::spawn(async move {
            let req = Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/projects/{pid}/calls"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"communication_id": comm})).unwrap(),
                ))
                .unwrap();
            router.oneshot(req).await.unwrap().status().as_u16()
        }));
    }
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn twenty_concurrent_starts_postgres() {
    let Some((t, schema)) = pg_app(24).await else {
        eprintln!("SKIPPED: TEST_DATABASE_URL is not set");
        return;
    };
    let c = Ctx::new(t);
    let fx = fixture(&c.t);
    let (pid, prop) = c.project().await;
    // One communication, twenty starts.
    let a = c.outreach(pid, &prop, "Dana", "+15555550100").await;
    c.approve(&a).await;
    let a_comm = a["communication"]["communication_id"].as_str().unwrap().to_owned();
    let st = twenty_starts(&c, pid, vec![a_comm]).await;
    println!("REVIEW 20 starts, one communication: {st:?}; dials {}", fx.calls().len());
    assert_eq!(st.iter().filter(|s| **s == 202).count(), 1, "{st:?}");
    assert_eq!(st.iter().filter(|s| **s == 409).count(), 19, "{st:?}");
    assert_eq!(fx.calls().len(), 1);
    // Two communications sharing one approval (a repeated outreach reuses it).
    let b1 = c.outreach(pid, &prop, "Eve", "+15555550200").await;
    let b2 = c.outreach(pid, &prop, "Eve", "+15555550200").await;
    assert_eq!(b1["approval"]["approval_id"], b2["approval"]["approval_id"]);
    c.approve(&b1).await;
    let st = twenty_starts(
        &c,
        pid,
        vec![
            b1["communication"]["communication_id"].as_str().unwrap().to_owned(),
            b2["communication"]["communication_id"].as_str().unwrap().to_owned(),
        ],
    )
    .await;
    println!("REVIEW 20 starts, two communications one approval: {st:?}; dials {}", fx.calls().len());
    assert_eq!(st.iter().filter(|s| **s == 202).count(), 1, "{st:?}");
    assert_eq!(fx.calls().len(), 2);
    let mut tx = c.t.app.store.begin().await.unwrap();
    let calls = CALLS.list(&mut *tx, Query::new().eq("project_id", pid)).await.unwrap();
    let appr = APPROVALS
        .require(&mut *tx, b1["approval"]["approval_id"].as_str().unwrap().parse().unwrap())
        .await
        .unwrap();
    let executed = ss_server::store::repo::events::select(
        &mut *tx,
        Query::new().eq("project_id", pid),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(appr.approval_status, ApprovalStatus::Executed);
    let started = executed
        .iter()
        .filter_map(|r| ss_server::store::repo::events::parse(r).ok())
        .filter(|e| e.event_type.as_str() == "call.started")
        .count();
    println!("REVIEW call.started events: {started}");
    assert_eq!(started, 2);
    common::drop_schema(&schema).await;
}
