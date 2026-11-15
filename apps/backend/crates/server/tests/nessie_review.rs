//! Independent review probes of the Nessie routes (nessie-2 review, launch 0f8160e5): the approval
//! gate against every wrong approval, `MAY_EXIST` for every unclear bank answer, a request dropped
//! during the bank call, PostgreSQL concurrency and lock interplay, isolation between projects and
//! owners, labels, the API key, and the payload dump for the Python `payload_hash` parity check.
//!
//! No test calls the real Nessie API: the "sandbox" is a loopback stub (`common::mock_http`)
//! answered by `LiveNessieBank`, and the fixture bank is in process.

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use common::mock_http::{Canned, MockServer, Recorded};
use serde_json::{json, Value};
use ss_core::Contract;
use ss_domain::contracts::CostScenario;
use ss_domain::integrations::base::RetryPolicy;
use ss_nessie::config::ApiKey;
use ss_nessie::plan::{
    ITEM_DEPOSIT, ITEM_FIRST_MONTH, ITEM_FURNISHING, ITEM_OCCUPANCY_BILL, ITEM_RENOVATION,
};
use ss_nessie::{plan_move_in_payments, BomTotals, LiveNessieBank, SandboxTargets, Simulation};
use ss_server::contracts::{payload_hash, ActionType, ApprovalRequest, ApprovalStatus, JsonMap};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::nessie::{
    self as svc, attempt_id_for, base_payload_hash, install, NessieState,
};
use ss_server::store::repo::{APPROVALS, COST_SCENARIOS};
use ss_server::store::Query;
use uuid::Uuid;

use common::*;

const FIXTURE: &str = "FIXTURE_IN_MEMORY_BANK";
const SANDBOX: &str = "CAPITAL_ONE_NESSIE_SANDBOX";
/// The stub's API key: it must appear in the `Authorization` header and nowhere else.
const CANARY_KEY: &str = "nsk_REVIEWCANARY_0f8160e5c4a7";

// ---- Projects ---------------------------------------------------------------------------------

#[derive(Clone)]
struct Demo {
    tok: String,
    user: Uuid,
    pid: String,
    hollywood: String,
    alhambra: String,
}

async fn demo_for(t: &TestApp, tok: &str, user: Uuid) -> Demo {
    let pid = t.create_project(tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(tok),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(tok))
        .await
        .body;
    let find = |street: &str| {
        props
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["property_identity"]["street_address"] == street)
            .unwrap()["property_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    Demo {
        tok: tok.to_owned(),
        user,
        pid,
        hollywood: find("6922 W HOLLYWOOD BLVD"),
        alhambra: find("5380 E ALHAMBRA AVE"),
    }
}

async fn demo(t: &TestApp) -> Demo {
    let (tok, user) = t.dev_token().await;
    demo_for(t, &tok, user).await
}

fn sandbox_path(pid: &str) -> String {
    format!("/api/v1/projects/{pid}/nessie/sandbox")
}

fn prop_path(pid: &str, prop: &str, what: &str) -> String {
    format!("/api/v1/projects/{pid}/properties/{prop}/nessie/{what}")
}

fn pay_path(pid: &str, prop: &str, item: &str, what: &str) -> String {
    format!("/api/v1/projects/{pid}/properties/{prop}/nessie/payments/{item}/{what}")
}

fn fork(t: &TestApp) -> TestApp {
    TestApp {
        app: t.app.clone(),
        router: t.router.clone(),
        kind: t.kind,
    }
}

async fn set_up(t: &TestApp, d: &Demo) -> Resp {
    let r = t
        .post(&sandbox_path(&d.pid), Some(&d.tok), &json!({}))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    r
}

async fn request_approval(t: &TestApp, d: &Demo, prop: &str, item: &str) -> Resp {
    t.call(
        Method::POST,
        &pay_path(&d.pid, prop, item, "approval"),
        Some(&d.tok),
        None,
    )
    .await
}

async fn decide(t: &TestApp, tok: &str, approval: &Value, decision: &str) {
    let r = t
        .post(
            &format!(
                "/api/v1/approvals/{}/decision",
                approval["approval_id"].as_str().unwrap()
            ),
            Some(tok),
            &json!({"decision": decision, "action_payload_hash": approval["action_payload_hash"]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
}

async fn approved(t: &TestApp, d: &Demo, prop: &str, item: &str) -> Value {
    let a = request_approval(t, d, prop, item).await;
    assert!(a.status.is_success(), "{}", a.text);
    decide(t, &d.tok, &a.body["approval"], "APPROVED").await;
    a.body["approval"].clone()
}

async fn execute(t: &TestApp, d: &Demo, prop: &str, item: &str, approval_id: &Value) -> Resp {
    t.post(
        &pay_path(&d.pid, prop, item, "execute"),
        Some(&d.tok),
        &json!({"approval_id": approval_id}),
    )
    .await
}

async fn approval_row(t: &TestApp, id: &Value) -> ApprovalRequest {
    let mut tx = t.app.store.begin().await.unwrap();
    let a = APPROVALS
        .require(&mut *tx, id.as_str().unwrap().parse().unwrap())
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    a
}

async fn edit_approval(t: &TestApp, id: &Value, f: impl FnOnce(&mut ApprovalRequest)) {
    let mut tx = t.app.store.begin().await.unwrap();
    let mut a = APPROVALS
        .require(&mut *tx, id.as_str().unwrap().parse().unwrap())
        .await
        .unwrap();
    f(&mut a);
    APPROVALS.upsert(&mut *tx, &a).await.unwrap();
    tx.commit().await.unwrap();
}

/// An approval created through the approval service with any payload (what any other code path
/// that can request approvals could create), decided by the owner through the decision route.
async fn other_approval(
    t: &TestApp,
    d: &Demo,
    action: ActionType,
    payload: JsonMap,
    decision: Option<&str>,
) -> Value {
    let mut tx = t.app.store.begin().await.unwrap();
    let spec = ApprovalSpec::new(
        d.pid.parse().unwrap(),
        d.user,
        "review-probe",
        action,
        payload,
        "review probe",
        format!("review:{}", Uuid::new_v4()),
    );
    let a = t.app.approvals().request(&mut *tx, spec).await.unwrap();
    tx.commit().await.unwrap();
    let v = serde_json::to_value(&a).unwrap();
    if let Some(dec) = decision {
        decide(t, &d.tok, &v, dec).await;
    }
    v
}

async fn records(t: &TestApp, pid: &str) -> Vec<svc::NessieSimulationRecord> {
    let mut tx = t.app.store.begin().await.unwrap();
    let r = svc::records_of(&mut *tx, pid.parse().unwrap())
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    r
}

async fn attempt(
    t: &TestApp,
    pid: &str,
    prop: &str,
    item: &str,
) -> Option<svc::PaymentAttemptData> {
    records(t, pid)
        .await
        .into_iter()
        .find(|r| r.record_key == format!("{prop}:{item}"))
        .and_then(|r| r.payment)
}

// ---- The stub that claims to be the Capital One sandbox -----------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Normal,
    /// A problem+json answer with this status to every payment POST.
    Status(u16),
    WrongAmount,
    WrongKind,
    WrongMerchant,
    WrongPayer,
    NoPayer,
    WrongBill,
    /// A 200 that is not JSON.
    Html,
}

#[derive(Clone)]
struct Ctl {
    mode: Arc<Mutex<Mode>>,
    /// Delay before answering a payment or bill POST (ms).
    pay_delay_ms: Arc<AtomicU64>,
    /// Delay before answering a set-up POST (customer, account, merchant) (ms).
    setup_delay_ms: Arc<AtomicU64>,
}

impl Ctl {
    fn mode(&self, m: Mode) {
        *self.mode.lock().unwrap() = m;
    }
}

#[derive(Default)]
struct StubState {
    seq: u64,
    accounts: std::collections::HashMap<String, Value>,
    bills: std::collections::HashMap<String, Value>,
    txns: std::collections::HashMap<String, Value>,
}

fn reply(status: u16, v: Value) -> Canned {
    Canned {
        status,
        content_type: Some("application/json".into()),
        body: v.to_string().into_bytes(),
    }
}

fn problem(status: u16, slug: &str) -> Canned {
    Canned {
        status,
        content_type: Some("application/problem+json".into()),
        body: json!({"type": format!("https://nessieisreal.com/problems/{slug}"), "title": slug, "status": status, "detail": "stub"})
            .to_string()
            .into_bytes(),
    }
}

fn stub() -> (MockServer, Ctl) {
    let ctl = Ctl {
        mode: Arc::new(Mutex::new(Mode::Normal)),
        pay_delay_ms: Arc::new(AtomicU64::new(0)),
        setup_delay_ms: Arc::new(AtomicU64::new(0)),
    };
    let c = ctl.clone();
    let state = Mutex::new(StubState::default());
    let srv = MockServer::start_with(move |r: &Recorded| {
        let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        let parts: Vec<&str> = r.path.trim_start_matches('/').split('/').collect();
        let paying = r.method == "POST"
            && (r.path.ends_with("/purchases")
                || r.path.ends_with("/payments")
                || r.path.ends_with("/bills"));
        if paying {
            std::thread::sleep(Duration::from_millis(c.pay_delay_ms.load(Ordering::SeqCst)));
        } else if r.method == "POST" {
            std::thread::sleep(Duration::from_millis(
                c.setup_delay_ms.load(Ordering::SeqCst),
            ));
        }
        let mode = *c.mode.lock().unwrap();
        let mut st = state.lock().unwrap();
        st.seq += 1;
        let n = st.seq;
        let ts = "2026-11-14T12:00:00Z";
        let pay_mode = |t: &mut Value| -> Option<Canned> {
            match mode {
                Mode::Normal => None,
                Mode::Status(s) => Some(problem(s, "stub-status")),
                Mode::Html => Some(Canned {
                    status: 200,
                    content_type: Some("text/html".into()),
                    body: b"<html>ok</html>".to_vec(),
                }),
                Mode::WrongAmount => {
                    t["amount_cents"] = json!(t["amount_cents"].as_i64().unwrap_or(0) + 1);
                    None
                }
                Mode::WrongKind => {
                    t["kind"] = json!("deposit");
                    None
                }
                Mode::WrongMerchant => {
                    t["merchant_id"] = json!("merch_someone_else");
                    None
                }
                Mode::WrongPayer => {
                    t["payer_id"] = json!("acct_someone_else");
                    None
                }
                Mode::NoPayer => {
                    if let Some(o) = t.as_object_mut() {
                        o.remove("payer_id");
                    }
                    None
                }
                Mode::WrongBill => {
                    t["bill_id"] = json!("bill_someone_else");
                    None
                }
            }
        };
        match (r.method.as_str(), parts.as_slice()) {
            ("POST", ["customers"]) => reply(
                200,
                json!({"id": format!("cust_{n}"), "first_name": body["first_name"], "last_name": body["last_name"], "address": body["address"], "created_at": ts, "updated_at": ts}),
            ),
            ("POST", ["customers", cid, "accounts"]) => {
                let id = format!("acct_{n}");
                let a = json!({"id": id, "customer_id": cid, "type": body["type"], "nickname": body["nickname"], "balance_cents": body["balance_cents"], "pending_debits_cents": 0, "available_cents": body["balance_cents"], "account_number": "1", "rewards": 0, "created_at": ts, "updated_at": ts});
                st.accounts.insert(id, a.clone());
                reply(200, a)
            }
            ("GET", ["accounts", id]) => match st.accounts.get(*id) {
                Some(a) => reply(200, a.clone()),
                None => problem(404, "account-not-found"),
            },
            ("POST", ["merchants"]) => reply(
                200,
                json!({"id": format!("merch_{n}"), "name": body["name"], "category": body["category"], "created_at": ts, "updated_at": ts}),
            ),
            ("POST", ["accounts", id, "purchases"]) => {
                let mut t = json!({"id": format!("txn_{n}"), "kind": "purchase", "status": "pending", "medium": "balance", "amount_cents": body["amount_cents"], "payer_id": id, "merchant_id": body["merchant_id"], "description": body["description"], "settle_at": ts, "created_at": ts, "updated_at": ts});
                if let Some(canned) = pay_mode(&mut t) {
                    return canned;
                }
                st.txns.insert(format!("txn_{n}"), t.clone());
                reply(200, t)
            }
            ("POST", ["accounts", id, "bills"]) => {
                let b = json!({"id": format!("bill_{n}"), "account_id": id, "status": body["status"], "payee": body["payee"], "nickname": body["nickname"], "creation_date": "2026-11-14", "upcoming_payment_date": "2026-12-01", "payment_amount": body["payment_amount"], "recurring_date": body["recurring_date"], "created_at": ts, "updated_at": ts});
                st.bills.insert(format!("bill_{n}"), b.clone());
                reply(200, b)
            }
            ("POST", ["bills", bid, "payments"]) => {
                let account = st
                    .bills
                    .get(*bid)
                    .map(|b| b["account_id"].clone())
                    .unwrap_or(Value::Null);
                let mut t = json!({"id": format!("txn_{n}"), "kind": "bill_payment", "status": "pending", "medium": "balance", "amount_cents": body["amount_cents"], "payer_id": account, "bill_id": bid, "settle_at": ts, "created_at": ts, "updated_at": ts});
                if let Some(canned) = pay_mode(&mut t) {
                    return canned;
                }
                st.txns.insert(format!("txn_{n}"), t.clone());
                reply(200, t)
            }
            ("GET", ["transactions", id]) => match st.txns.get(*id) {
                Some(t) => reply(200, t.clone()),
                None => problem(404, "transaction-not-found"),
            },
            _ => problem(404, "not-found"),
        }
    });
    (srv, ctl)
}

fn stub_state(srv: &MockServer, timeout_s: f64) -> NessieState {
    let bank = LiveNessieBank::with_policy(
        &srv.base,
        ApiKey::new(CANARY_KEY).unwrap(),
        RetryPolicy {
            attempts: 1,
            backoff_s: 0.0,
            timeout_s,
        },
    );
    NessieState::with_bank(Arc::new(bank), format!("sandbox:{}", srv.base))
}

fn app_with_stub(db: &TestDb, srv: &MockServer, timeout_s: f64) -> TestApp {
    let t = TestApp::new(db, &[]);
    install(&t.app, stub_state(srv, timeout_s));
    t
}

fn app_with_fixture(db: &TestDb) -> TestApp {
    let t = TestApp::new(db, &[]);
    install(&t.app, NessieState::fixture());
    t
}

/// POSTs that move mock money or commit to it (purchases, bill payments, bills, deposits,
/// transfers, withdrawals).
fn money_posts(srv: &MockServer) -> Vec<Recorded> {
    srv.requests()
        .into_iter()
        .filter(|r| {
            r.method == "POST"
                && [
                    "/purchases",
                    "/payments",
                    "/bills",
                    "/deposits",
                    "/transfers",
                    "/withdrawals",
                ]
                .iter()
                .any(|s| r.path.ends_with(s))
        })
        .collect()
}

// ---- Labels -----------------------------------------------------------------------------------------

const BANK_OUTCOME_CODES: [&str; 4] = [
    "NESSIE_PAYMENT_REFUSED",
    "NESSIE_PAYMENT_MAY_EXIST",
    "NESSIE_SANDBOX_INCOMPLETE",
    "NESSIE_NOT_SENT",
];

/// A success answer carries the label at the top level; a bank-outcome error carries it in
/// `details` (`ProblemDetail` has a closed key set). Other errors are returned for the report.
fn check_label(r: &Resp, sim: &str) -> Option<String> {
    if r.status.is_success() {
        assert_eq!(r.body["simulation"], sim, "{}", r.text);
        let d = r.body["disclaimer"].as_str().unwrap_or_default();
        assert!(d.to_lowercase().contains("mock money"), "{}", r.text);
        return None;
    }
    let code = r.body["error_code"].as_str().unwrap_or_default().to_owned();
    if BANK_OUTCOME_CODES.contains(&code.as_str()) {
        assert_eq!(r.body["details"]["simulation"], sim, "{}", r.text);
        assert!(r.body["details"]["disclaimer"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("mock money"));
        return None;
    }
    Some(format!("{} {code}", r.status.as_u16()))
}

/// Sentences that use a word that could claim a real or completed payment, for the report.
fn claim_words(text: &str, out: &mut BTreeSet<String>) {
    for s in text.split(['.', '"']) {
        let l = s.to_lowercase();
        if ["paid", "complet", "settle", " real", "succe"]
            .iter()
            .any(|w| l.contains(w))
        {
            out.insert(s.trim().to_owned());
        }
    }
}

// ---- 1. The gate: no mock money moves without the exact approval --------------------------------

both_backends!(review_gate_refuses_every_wrong_approval, |db| {
    let (srv, _ctl) = stub();
    let t = app_with_stub(db, &srv, 5.0);
    let a = demo(&t).await;
    let b = demo_for(&t, &a.tok, a.user).await;
    let y = Uuid::new_v4();
    let c = demo_for(&t, &t.token_for(y), y).await;
    for d in [&a, &b, &c] {
        set_up(&t, d).await;
    }
    let prop = a.hollywood.clone();
    let honest = request_approval(&t, &a, &prop, ITEM_DEPOSIT).await;
    assert_eq!(honest.status, StatusCode::CREATED, "{}", honest.text);
    let honest = honest.body["approval"].clone();
    let exact: JsonMap = serde_json::from_value(honest["action_payload"].clone()).unwrap();
    let fc = ActionType::FinancialCommitment;
    let mut cases: Vec<(String, Value)> = Vec::new();

    // States other than APPROVED, each on an approval of exactly the right payload.
    cases.push(("PENDING".into(), honest["approval_id"].clone()));
    let v = other_approval(&t, &a, fc, exact.clone(), Some("REJECTED")).await;
    cases.push(("REJECTED".into(), v["approval_id"].clone()));
    let v = other_approval(&t, &a, fc, exact.clone(), Some("APPROVED")).await;
    edit_approval(&t, &v["approval_id"], |x| {
        x.expires_at =
            ss_core::UtcDateTime::from_datetime(chrono::Utc::now() - chrono::Duration::seconds(1))
    })
    .await;
    cases.push(("APPROVED but expired".into(), v["approval_id"].clone()));
    let v = other_approval(&t, &a, fc, exact.clone(), Some("APPROVED")).await;
    edit_approval(&t, &v["approval_id"], |x| {
        x.approval_status = ApprovalStatus::Cancelled
    })
    .await;
    cases.push(("CANCELLED".into(), v["approval_id"].clone()));
    let v = other_approval(&t, &a, fc, exact.clone(), Some("APPROVED")).await;
    edit_approval(&t, &v["approval_id"], |x| {
        x.approval_status = ApprovalStatus::Executed
    })
    .await;
    cases.push(("EXECUTED".into(), v["approval_id"].clone()));
    let v = other_approval(&t, &a, fc, exact.clone(), Some("APPROVED")).await;
    edit_approval(&t, &v["approval_id"], |x| {
        x.approval_status = ApprovalStatus::Expired
    })
    .await;
    cases.push(("EXPIRED".into(), v["approval_id"].clone()));
    // Another action type for exactly the right payload.
    for action in [ActionType::Purchase, ActionType::PlaceCall] {
        let v = other_approval(&t, &a, action, exact.clone(), Some("APPROVED")).await;
        cases.push((
            format!("action {}", action.as_str()),
            v["approval_id"].clone(),
        ));
    }
    // Another project of the same owner, and another owner's project (their own honest approvals).
    let v = approved(&t, &b, &b.hollywood, ITEM_DEPOSIT).await;
    cases.push((
        "same owner's other project".into(),
        v["approval_id"].clone(),
    ));
    let v = approved(&t, &c, &c.hollywood, ITEM_DEPOSIT).await;
    cases.push(("another owner's project".into(), v["approval_id"].clone()));
    // Another plan item and another property of this project (honest, APPROVED).
    let v = approved(&t, &a, &prop, ITEM_RENOVATION).await;
    cases.push(("another plan item".into(), v["approval_id"].clone()));
    let v = approved(&t, &a, &a.alhambra, ITEM_DEPOSIT).await;
    cases.push(("another property".into(), v["approval_id"].clone()));
    // One payload field changed, APPROVED by the owner.
    let b_account = records(&t, &b.pid).await[0]
        .sandbox
        .as_ref()
        .unwrap()
        .account_id
        .clone()
        .unwrap();
    let other_merchant = records(&t, &a.pid).await[0]
        .sandbox
        .as_ref()
        .unwrap()
        .contractor_merchant_id
        .clone()
        .unwrap();
    let attempt_id = exact["attempt_id"].as_str().unwrap().to_owned();
    // nessie-3: an attempt id of digits only (about 1 run in 280) has no upper case, so the
    // "upper case" tamper would be the exact payload; then one digit becomes an upper-case letter.
    let upper = if attempt_id.chars().any(|c| c.is_ascii_alphabetic()) {
        attempt_id.to_uppercase()
    } else {
        format!("{}A", &attempt_id[..attempt_id.len() - 1])
    };
    let tampers: Vec<(&str, Value)> = vec![
        (
            "amount_minor",
            json!(exact["amount_minor"].as_i64().unwrap() + 1),
        ),
        (
            "amount_minor",
            json!(exact["amount_minor"].as_i64().unwrap() - 1),
        ),
        ("amount_minor", json!(exact["amount_minor"].to_string())),
        ("account_id", json!(b_account)),
        ("merchant_id", json!(other_merchant)),
        (
            "description",
            json!(format!("{} ", exact["description"].as_str().unwrap())),
        ),
        ("attempt_id", json!("0123456789ab")),
        ("attempt_id", json!(upper)),
        ("simulation", json!(FIXTURE)),
        ("scenario_id", json!(Uuid::new_v4())),
        ("property_id", json!(a.alhambra)),
        ("project_id", json!(b.pid)),
        ("plan_item_id", json!(ITEM_RENOVATION)),
        ("operation", json!("pay_bill")),
        ("moves_money", json!(false)),
        ("currency", json!("usd")),
        ("integration", json!("capital_one_nessie_v1")),
        ("disclaimer", json!("Real money.")),
        ("extra_key", json!(1)),
    ];
    for (field, value) in tampers {
        let mut p = exact.clone();
        p.insert(field.into(), value.clone());
        let v = other_approval(&t, &a, fc, p, Some("APPROVED")).await;
        cases.push((format!("{field} = {value}"), v["approval_id"].clone()));
    }
    let mut p = exact.clone();
    p.remove("disclaimer");
    let v = other_approval(&t, &a, fc, p, Some("APPROVED")).await;
    cases.push(("disclaimer removed".into(), v["approval_id"].clone()));
    // No approval at all, a null one, and an unknown one.
    let path = pay_path(&a.pid, &prop, ITEM_DEPOSIT, "execute");
    for body in [json!({}), json!({"approval_id": null})] {
        let r = t.post(&path, Some(&a.tok), &body).await;
        assert_problem(&r, 422, "VALIDATION_ERROR");
        eprintln!(
            "REVIEW-GATE {:<44} {} VALIDATION_ERROR",
            body.to_string(),
            422
        );
    }
    cases.push(("unknown approval id".into(), json!(Uuid::new_v4())));

    let before: Vec<ApprovalStatus> = {
        let mut s = Vec::new();
        for (name, id) in &cases {
            if name != "unknown approval id" {
                s.push(approval_row(&t, id).await.approval_status);
            }
        }
        s
    };
    for (name, id) in &cases {
        let r = execute(&t, &a, &prop, ITEM_DEPOSIT, id).await;
        eprintln!(
            "REVIEW-GATE {:<44} {} {}",
            name,
            r.status.as_u16(),
            r.body["error_code"].as_str().unwrap_or_default()
        );
        assert!(
            [404, 409].contains(&r.status.as_u16()),
            "{name}: {} {}",
            r.status,
            r.text
        );
        assert_problem(
            &r,
            r.status.as_u16(),
            r.body["error_code"].as_str().unwrap(),
        );
    }
    assert_eq!(
        money_posts(&srv).len(),
        0,
        "a refused case reached the bank"
    );
    assert!(attempt(&t, &a.pid, &prop, ITEM_DEPOSIT).await.is_none());
    let mut after = Vec::new();
    for (name, id) in &cases {
        if name != "unknown approval id" {
            after.push(approval_row(&t, id).await.approval_status);
        }
    }
    // EXPIRED-by-time is reported EXPIRED but the write is rolled back with the refusal.
    assert_eq!(before, after, "a refused execute changed an approval");

    // The honest approval, once APPROVED, executes once; what the stub received is exactly the
    // approved payload, with the plan item and attempt id in the description.
    decide(&t, &a.tok, &honest, "APPROVED").await;
    let r = execute(&t, &a, &prop, ITEM_DEPOSIT, &honest["approval_id"]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let sent = money_posts(&srv);
    assert_eq!(sent.len(), 1);
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(
        sent[0].path,
        format!(
            "/accounts/{}/purchases",
            exact["account_id"].as_str().unwrap()
        )
    );
    assert_eq!(body["merchant_id"], exact["merchant_id"]);
    assert_eq!(body["amount_cents"], exact["amount_minor"]);
    assert_eq!(body["description"], exact["description"]);
    assert!(body["description"].as_str().unwrap().starts_with(&format!(
        "SIMULATION (Second Site plan item {ITEM_DEPOSIT}, attempt {attempt_id}): "
    )));
});

// ---- 2. An approval made against one bank never executes against the other ----------------------

both_backends!(review_an_approval_is_bound_to_its_bank, |db| {
    let (srv, _ctl) = stub();
    // Fixture approval, then the server is switched to the "sandbox".
    let t = app_with_fixture(db);
    let d = demo(&t).await;
    let sb = set_up(&t, &d).await;
    let account = sb.body["account_id"].as_str().unwrap().to_owned();
    let a = approved(&t, &d, &d.hollywood, ITEM_DEPOSIT).await;
    install(&t.app, stub_state(&srv, 5.0));
    let r = execute(&t, &d, &d.hollywood, ITEM_DEPOSIT, &a["approval_id"]).await;
    assert_problem(&r, 409, "NESSIE_SANDBOX_GONE");
    // A new sandbox in the stub: the old approval names the fixture ids and the fixture label.
    set_up(&t, &d).await;
    let r = execute(&t, &d, &d.hollywood, ITEM_DEPOSIT, &a["approval_id"]).await;
    assert_problem(&r, 409, "APPROVAL_PAYLOAD_MISMATCH");
    assert_eq!(money_posts(&srv).len(), 0);

    // Sandbox approval, then a fixture bank that even claims the stub's identity, so the stored
    // sandbox looks usable: the label in the approved payload still refuses it.
    let (srv2, _ctl2) = stub();
    let t2 = app_with_stub(db, &srv2, 5.0);
    let d2 = demo(&t2).await;
    let sb2 = set_up(&t2, &d2).await;
    let a2 = approved(&t2, &d2, &d2.hollywood, ITEM_DEPOSIT).await;
    install(
        &t2.app,
        NessieState::with_bank(
            Arc::new(ss_nessie::FixtureBank::new()),
            format!("sandbox:{}", srv2.base),
        ),
    );
    let r = execute(&t2, &d2, &d2.hollywood, ITEM_DEPOSIT, &a2["approval_id"]).await;
    eprintln!(
        "REVIEW-BANK sandbox approval on a fixture bank posing as the sandbox: {} {}",
        r.status.as_u16(),
        r.body["error_code"]
    );
    assert!([409, 404].contains(&r.status.as_u16()), "{}", r.text);
    assert_eq!(money_posts(&srv2).len(), 0);
    assert_eq!(
        approval_row(&t2, &a2["approval_id"]).await.approval_status,
        ApprovalStatus::Approved
    );
    let _ = (account, sb2);
});

// ---- 3. MAY_EXIST is durable, a listed refusal leaves the approval usable -------------------------

/// Purchase plan items of a project's two properties, as (property, item) slots.
fn slots(d: &Demo) -> Vec<(String, &'static str)> {
    let mut v = Vec::new();
    for p in [&d.hollywood, &d.alhambra] {
        for i in [ITEM_DEPOSIT, ITEM_RENOVATION, ITEM_FURNISHING] {
            v.push((p.clone(), i));
        }
    }
    v
}

both_backends!(review_may_exist_is_durable_and_refusals_are_not, |db| {
    let (srv, ctl) = stub();
    let timeout_s = 0.6;
    let t = app_with_stub(db, &srv, timeout_s);
    let mut free: Vec<(Demo, String, &'static str)> = Vec::new();
    let (tok, user) = t.dev_token().await;
    for _ in 0..4 {
        let d = demo_for(&t, &tok, user).await;
        set_up(&t, &d).await;
        for (p, i) in slots(&d) {
            free.push((d.clone(), p, i));
        }
    }
    let mut unlabelled = BTreeSet::new();
    let mut words = BTreeSet::new();
    let unclear = [
        ("timeout", Mode::Normal, 1200u64),
        ("500", Mode::Status(500), 0),
        ("503", Mode::Status(503), 0),
        ("502", Mode::Status(502), 0),
        ("408", Mode::Status(408), 0),
        ("418", Mode::Status(418), 0),
        ("499", Mode::Status(499), 0),
        ("410", Mode::Status(410), 0),
        ("2xx another amount", Mode::WrongAmount, 0),
        ("2xx another kind", Mode::WrongKind, 0),
        ("2xx another merchant", Mode::WrongMerchant, 0),
        ("2xx another payer", Mode::WrongPayer, 0),
        ("2xx no payer", Mode::NoPayer, 0),
        ("2xx not JSON", Mode::Html, 0),
    ];
    for (name, mode, delay) in unclear {
        let (d, prop, item) = free.pop().unwrap();
        let a = approved(&t, &d, &prop, item).await;
        let id = a["approval_id"].clone();
        ctl.mode(mode);
        ctl.pay_delay_ms.store(delay, Ordering::SeqCst);
        let before = money_posts(&srv).len();
        let r = execute(&t, &d, &prop, item, &id).await;
        ctl.mode(Mode::Normal);
        ctl.pay_delay_ms.store(0, Ordering::SeqCst);
        // Let a stalled stub finish before counting.
        tokio::time::sleep(Duration::from_millis(delay)).await;
        eprintln!(
            "REVIEW-MAYEXIST {:<22} {} {}",
            name,
            r.status.as_u16(),
            r.body["error_code"].as_str().unwrap_or_default()
        );
        assert_problem(&r, 502, "NESSIE_PAYMENT_MAY_EXIST");
        if let Some(u) = check_label(&r, SANDBOX) {
            unlabelled.insert(u);
        }
        claim_words(&r.text, &mut words);
        let msg = r.body["message"].as_str().unwrap();
        assert!(msg.contains("may have been made"), "{msg}");
        assert!(msg.contains("before trying again"), "{msg}");
        assert_eq!(r.body["details"]["approval_status"], "EXECUTED");
        assert_eq!(
            approval_row(&t, &id).await.approval_status,
            ApprovalStatus::Executed,
            "{name}: the approval must be spent in the database"
        );
        let stored = attempt(&t, &d.pid, &prop, item).await.unwrap();
        assert_eq!(stored.attempt_status, "MAY_EXIST", "{name}");
        assert_eq!(money_posts(&srv).len(), before + 1, "{name}: sent once");
        let r = execute(&t, &d, &prop, item, &id).await;
        assert_problem(&r, 409, "NESSIE_ALREADY_ATTEMPTED");
        let r = request_approval(&t, &d, &prop, item).await;
        assert_problem(&r, 409, "NESSIE_ALREADY_ATTEMPTED");
        assert_eq!(money_posts(&srv).len(), before + 1, "{name}: never twice");
    }
    // A bill payment answered about another bill.
    let d = free[0].0.clone();
    let prop = d.hollywood.clone();
    let bill = approved(&t, &d, &prop, ITEM_OCCUPANCY_BILL).await;
    let r = execute(&t, &d, &prop, ITEM_OCCUPANCY_BILL, &bill["approval_id"]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["record"]["outcome"], "SIMULATED_BILL_RECORDED");
    claim_words(&r.text, &mut words);
    let first = approved(&t, &d, &prop, ITEM_FIRST_MONTH).await;
    ctl.mode(Mode::WrongBill);
    let r = execute(&t, &d, &prop, ITEM_FIRST_MONTH, &first["approval_id"]).await;
    ctl.mode(Mode::Normal);
    eprintln!(
        "REVIEW-MAYEXIST {:<22} {} {}",
        "2xx another bill",
        r.status.as_u16(),
        r.body["error_code"].as_str().unwrap_or_default()
    );
    assert_problem(&r, 502, "NESSIE_PAYMENT_MAY_EXIST");
    assert_eq!(
        approval_row(&t, &first["approval_id"])
            .await
            .approval_status,
        ApprovalStatus::Executed
    );

    // The listed refusals: 422, nothing spent, the same approval executes once fixed.
    for status in [400u16, 401, 403, 404, 409, 422, 429] {
        let (d, prop, item) = free.pop().unwrap();
        let a = approved(&t, &d, &prop, item).await;
        let id = a["approval_id"].clone();
        ctl.mode(Mode::Status(status));
        let before = money_posts(&srv).len();
        let r = execute(&t, &d, &prop, item, &id).await;
        ctl.mode(Mode::Normal);
        eprintln!(
            "REVIEW-REFUSED {:<23} {} {}",
            status,
            r.status.as_u16(),
            r.body["error_code"].as_str().unwrap_or_default()
        );
        assert_problem(&r, 422, "NESSIE_PAYMENT_REFUSED");
        if let Some(u) = check_label(&r, SANDBOX) {
            unlabelled.insert(u);
        }
        claim_words(&r.text, &mut words);
        assert!(r.body["message"]
            .as_str()
            .unwrap()
            .contains("the approval is not spent"));
        assert_eq!(r.body["details"]["approval_status"], "APPROVED");
        assert_eq!(
            approval_row(&t, &id).await.approval_status,
            ApprovalStatus::Approved
        );
        assert_eq!(
            attempt(&t, &d.pid, &prop, item)
                .await
                .unwrap()
                .attempt_status,
            "REFUSED"
        );
        let r = execute(&t, &d, &prop, item, &id).await;
        assert_eq!(r.status, StatusCode::OK, "{status}: {}", r.text);
        assert_eq!(r.body["record"]["outcome"], "SIMULATED_PENDING");
        claim_words(&r.text, &mut words);
        assert_eq!(money_posts(&srv).len(), before + 2);
    }
    eprintln!("REVIEW-UNLABELLED {unlabelled:?}");
    for w in &words {
        eprintln!("REVIEW-WORDS {w}");
    }
});

// ---- 4. A request that goes away during the bank call ---------------------------------------------

both_backends!(review_a_request_dropped_during_the_bank_call, |db| {
    let (srv, ctl) = stub();
    let t = app_with_stub(db, &srv, 5.0);
    let d = demo(&t).await;
    set_up(&t, &d).await;
    let prop = d.hollywood.clone();
    let a = approved(&t, &d, &prop, ITEM_DEPOSIT).await;
    let id = a["approval_id"].clone();
    ctl.pay_delay_ms.store(1500, Ordering::SeqCst);
    // The client gives up after 300 ms (hyper drops the request future the same way when the
    // connection closes).
    let dropped = tokio::time::timeout(
        Duration::from_millis(300),
        execute(&t, &d, &prop, ITEM_DEPOSIT, &id),
    )
    .await;
    assert!(dropped.is_err(), "the request was expected to be cut off");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    ctl.pay_delay_ms.store(0, Ordering::SeqCst);
    let sent = money_posts(&srv).len();
    let status = approval_row(&t, &id).await.approval_status;
    let row = attempt(&t, &d.pid, &prop, ITEM_DEPOSIT).await;
    eprintln!(
        "REVIEW-DROPPED after the cut: bank POSTs {sent}, approval {status:?}, attempt {:?}",
        row.as_ref().map(|p| p.attempt_status.clone())
    );
    assert_eq!(sent, 1, "the bank received the payment");
    // A server restart (the live bank's identity is its base URL, so it is the same after one).
    install(&t.app, stub_state(&srv, 5.0));
    let r = execute(&t, &d, &prop, ITEM_DEPOSIT, &id).await;
    eprintln!(
        "REVIEW-DROPPED the same approval after a restart: {} {}; bank POSTs {}",
        r.status.as_u16(),
        r.body["error_code"].as_str().unwrap_or_default(),
        money_posts(&srv).len()
    );
    assert_eq!(
        money_posts(&srv).len(),
        1,
        "a second payment for one approval"
    );
    assert_eq!(status, ApprovalStatus::Executed);
    assert_eq!(row.unwrap().attempt_status, "SENT");
});

/// The same through a real socket: `axum::serve`, a raw HTTP/1.1 request, then the client closes
/// the connection while the bank call is in progress.
#[tokio::test]
async fn review_a_closed_connection_during_the_bank_call() {
    use tokio::io::AsyncWriteExt;
    let db = TestDb::new(Kind::Memory).await.unwrap();
    let (srv, ctl) = stub();
    let t = app_with_stub(&db, &srv, 5.0);
    let d = demo(&t).await;
    set_up(&t, &d).await;
    let prop = d.hollywood.clone();
    let a = approved(&t, &d, &prop, ITEM_RENOVATION).await;
    let id = a["approval_id"].clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = t.router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    ctl.pay_delay_ms.store(1500, Ordering::SeqCst);
    let body = json!({"approval_id": id}).to_string();
    let path = pay_path(&d.pid, &prop, ITEM_RENOVATION, "execute");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        d.tok,
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(s);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    ctl.pay_delay_ms.store(0, Ordering::SeqCst);
    let status = approval_row(&t, &id).await.approval_status;
    let row = attempt(&t, &d.pid, &prop, ITEM_RENOVATION).await;
    eprintln!(
        "REVIEW-SOCKET after the client closed: bank POSTs {}, approval {status:?}, attempt {:?}",
        money_posts(&srv).len(),
        row.as_ref().map(|p| p.attempt_status.clone())
    );
    assert_eq!(money_posts(&srv).len(), 1);
    assert_eq!(status, ApprovalStatus::Executed);
    assert_eq!(row.unwrap().attempt_status, "SENT");
    db.finish().await;
}

// ---- 5. Isolation between projects and owners ------------------------------------------------------

both_backends!(review_records_are_isolated, |db| {
    let t = app_with_fixture(db);
    let a = demo(&t).await;
    let b = demo_for(&t, &a.tok, a.user).await;
    let y = Uuid::new_v4();
    let c = demo_for(&t, &t.token_for(y), y).await;
    let mut accounts = Vec::new();
    for d in [&a, &b, &c] {
        accounts.push(set_up(&t, d).await.body["account_id"].clone());
    }
    assert_ne!(accounts[0], accounts[1]);
    assert_ne!(accounts[0], accounts[2]);
    let pa = approved(&t, &a, &a.hollywood, ITEM_DEPOSIT).await;
    let r = execute(&t, &a, &a.hollywood, ITEM_DEPOSIT, &pa["approval_id"]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let a_txn = r.body["record"]["nessie_transaction_id"].clone();
    let pb = approved(&t, &b, &b.hollywood, ITEM_DEPOSIT).await;
    let pc = approved(&t, &c, &c.hollywood, ITEM_DEPOSIT).await;

    // Each project sees its own sandbox.
    for (d, acct) in [(&a, &accounts[0]), (&b, &accounts[1]), (&c, &accounts[2])] {
        let r = t.get(&sandbox_path(&d.pid), Some(&d.tok)).await;
        assert_eq!(&r.body["account_id"], acct);
    }
    // Another project's property under this project's URL, both ways, on every property route.
    for (me, other) in [(&a, &b), (&b, &a), (&a, &c), (&c, &a)] {
        for what in ["plan", "affordability"] {
            let r = t
                .get(&prop_path(&me.pid, &other.hollywood, what), Some(&me.tok))
                .await;
            assert_problem(&r, 404, "NOT_FOUND");
            assert_eq!(r.body["message"], "property not found");
        }
        for what in ["approval", "refresh"] {
            let r = t
                .call(
                    Method::POST,
                    &pay_path(&me.pid, &other.hollywood, ITEM_DEPOSIT, what),
                    Some(&me.tok),
                    None,
                )
                .await;
            assert_problem(&r, 404, "NOT_FOUND");
        }
        let r = t
            .post(
                &pay_path(&me.pid, &other.hollywood, ITEM_DEPOSIT, "execute"),
                Some(&me.tok),
                &json!({"approval_id": pa["approval_id"]}),
            )
            .await;
        assert_problem(&r, 404, "NOT_FOUND");
    }
    // This project's property with another project's approval.
    for foreign in [&pb, &pc] {
        let r = execute(&t, &a, &a.alhambra, ITEM_DEPOSIT, &foreign["approval_id"]).await;
        assert_problem(&r, 404, "NOT_FOUND");
        let r = execute(&t, &a, &a.hollywood, ITEM_DEPOSIT, &foreign["approval_id"]).await;
        assert!([404, 409].contains(&r.status.as_u16()), "{}", r.text);
    }
    // B never sees A's attempt or transaction.
    let r = t
        .call(
            Method::POST,
            &pay_path(&b.pid, &b.hollywood, ITEM_DEPOSIT, "refresh"),
            Some(&b.tok),
            Some(&json!({"transaction_id": a_txn})),
        )
        .await;
    assert_problem(&r, 404, "NESSIE_NO_ATTEMPT");
    assert!(records(&t, &b.pid)
        .await
        .iter()
        .all(|r| r.payment.is_none()));
    // Refresh ignores a client transaction id.
    let r = t
        .call(
            Method::POST,
            &pay_path(&a.pid, &a.hollywood, ITEM_DEPOSIT, "refresh"),
            Some(&a.tok),
            Some(&json!({"transaction_id": "fixture_txn_9999"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["transaction"]["record"]["id"], a_txn);
    // Another owner on every route of A.
    let ytok = t.token_for(y);
    for (m, path) in [
        (Method::POST, sandbox_path(&a.pid)),
        (Method::GET, sandbox_path(&a.pid)),
        (Method::GET, prop_path(&a.pid, &a.hollywood, "plan")),
        (
            Method::GET,
            prop_path(&a.pid, &a.hollywood, "affordability"),
        ),
        (
            Method::POST,
            pay_path(&a.pid, &a.hollywood, ITEM_DEPOSIT, "approval"),
        ),
        (
            Method::POST,
            pay_path(&a.pid, &a.hollywood, ITEM_DEPOSIT, "refresh"),
        ),
    ] {
        let r = t.call(m, &path, Some(&ytok), None).await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "project not found");
    }
    let r = t
        .post(
            &pay_path(&a.pid, &a.hollywood, ITEM_DEPOSIT, "execute"),
            Some(&ytok),
            &json!({"approval_id": pc["approval_id"]}),
        )
        .await;
    assert_eq!(r.body["message"], "project not found");
});

// ---- 6. Labels, unknown rent, pending stays pending, the key ---------------------------------------

both_backends!(review_labels_unknown_rent_and_the_key, |db| {
    let (srv, ctl) = stub();
    let mut words = BTreeSet::new();
    let mut unlabelled = BTreeSet::new();
    let mut texts: Vec<String> = Vec::new();
    for sim in [FIXTURE, SANDBOX] {
        let t = if sim == FIXTURE {
            app_with_fixture(db)
        } else {
            app_with_stub(db, &srv, 0.6)
        };
        let d = demo(&t).await;
        let prop = d.hollywood.clone();
        let mut answers = vec![
            t.get(&sandbox_path(&d.pid), Some(&d.tok)).await,
            t.get(&prop_path(&d.pid, &prop, "affordability"), Some(&d.tok))
                .await,
            request_approval(&t, &d, &prop, ITEM_DEPOSIT).await,
        ];
        answers.push(set_up(&t, &d).await);
        answers.push(
            t.post(&sandbox_path(&d.pid), Some(&d.tok), &json!({}))
                .await,
        );
        answers.push(t.get(&sandbox_path(&d.pid), Some(&d.tok)).await);
        answers.push(t.get(&prop_path(&d.pid, &prop, "plan"), Some(&d.tok)).await);
        answers.push(
            t.get(&prop_path(&d.pid, &prop, "affordability"), Some(&d.tok))
                .await,
        );
        let a = request_approval(&t, &d, &prop, ITEM_DEPOSIT).await;
        let deposit_id = a.body["approval"]["approval_id"].clone();
        answers.push(execute(&t, &d, &prop, ITEM_DEPOSIT, &deposit_id).await);
        decide(&t, &d.tok, &a.body["approval"], "APPROVED").await;
        answers.push(a);
        let a2 = approved(&t, &d, &prop, ITEM_RENOVATION).await;
        let r = execute(&t, &d, &prop, ITEM_DEPOSIT, &deposit_id).await;
        assert_eq!(
            r.body["record"]["outcome"], "SIMULATED_PENDING",
            "{}",
            r.text
        );
        assert!(!r.text.contains("SIMULATED_COMPLETED"));
        answers.push(r);
        answers.push(
            t.call(
                Method::POST,
                &pay_path(&d.pid, &prop, ITEM_DEPOSIT, "refresh"),
                Some(&d.tok),
                None,
            )
            .await,
        );
        answers.push(execute(&t, &d, &prop, ITEM_DEPOSIT, &a2["approval_id"]).await);
        answers.push(request_approval(&t, &d, &prop, ITEM_FIRST_MONTH).await);
        answers.push(
            t.call(
                Method::POST,
                &pay_path(&d.pid, &prop, ITEM_RENOVATION, "refresh"),
                Some(&d.tok),
                None,
            )
            .await,
        );
        if sim == SANDBOX {
            ctl.mode(Mode::Status(503));
            answers.push(execute(&t, &d, &prop, ITEM_RENOVATION, &a2["approval_id"]).await);
            ctl.mode(Mode::Status(401));
            let f = approved(&t, &d, &prop, ITEM_FURNISHING).await;
            answers.push(execute(&t, &d, &prop, ITEM_FURNISHING, &f["approval_id"]).await);
            ctl.mode(Mode::Normal);
        }
        // Unknown rent: every MONTHLY line $0 and the scenario says so.
        let mut tx = t.app.store.begin().await.unwrap();
        for s in COST_SCENARIOS
            .list(
                &mut *tx,
                Query::new().eq("property_id", prop.parse::<Uuid>().unwrap()),
            )
            .await
            .unwrap()
        {
            let mut v = serde_json::to_value(&s).unwrap();
            for li in v["line_items"].as_array_mut().unwrap() {
                if li["kind"] == "MONTHLY" {
                    li["low"] = json!(0);
                    li["expected"] = json!(0);
                    li["high"] = json!(0);
                }
            }
            v["monthly_operating_cost"] = json!(0);
            v["assumptions"]
                .as_array_mut()
                .unwrap()
                .push(json!("Asking rent unknown"));
            let s2 = CostScenario::from_json_value(v).unwrap();
            COST_SCENARIOS.upsert(&mut *tx, &s2).await.unwrap();
        }
        tx.commit().await.unwrap();
        let r = t
            .get(&prop_path(&d.pid, &prop, "affordability"), Some(&d.tok))
            .await;
        eprintln!(
            "REVIEW-UNKNOWN-RENT {sim}: verdict {} / {}",
            r.body["report"]["verdict"], r.body["report"]["headline"]
        );
        assert_eq!(r.body["report"]["verdict"], "UNKNOWN", "{}", r.text);
        answers.push(r);
        for r in &answers {
            if let Some(u) = check_label(r, sim) {
                unlabelled.insert(u);
            }
            claim_words(&r.text, &mut words);
            texts.push(r.text.clone());
        }
        // The stored records and approvals.
        for rec in records(&t, &d.pid).await {
            texts.push(serde_json::to_string(&rec).unwrap());
        }
        let ev = t
            .get(
                &format!("/api/v1/projects/{}/events?limit=500", d.pid),
                Some(&d.tok),
            )
            .await;
        texts.push(ev.text);
        let ap = t
            .get(
                &format!("/api/v1/projects/{}/approvals", d.pid),
                Some(&d.tok),
            )
            .await;
        texts.push(ap.text);
        texts.push(t.get("/ready", None).await.text);
    }
    for (i, text) in texts.iter().enumerate() {
        assert!(
            !text.contains(CANARY_KEY),
            "the key leaked in answer {i}: {text}"
        );
        assert!(
            !text.contains("REVIEWCANARY"),
            "part of the key leaked in answer {i}"
        );
    }
    for rq in srv.requests() {
        assert_eq!(
            rq.headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {CANARY_KEY}").as_str())
        );
        assert!(!rq.path.contains("REVIEWCANARY"));
        assert!(!String::from_utf8_lossy(&rq.body).contains("REVIEWCANARY"));
    }
    eprintln!("REVIEW-UNLABELLED {unlabelled:?}");
    for w in &words {
        eprintln!("REVIEW-WORDS {w}");
    }
    eprintln!("REVIEW-KEY scanned {} answers and records", texts.len());
});

/// A key with a forbidden character is refused before anything is sent and never echoed.
#[test]
fn review_a_bad_key_is_not_echoed() {
    let canary = "nsk_BADCANARY\r\nX-Injected: 1";
    let r = ss_nessie::NessieConfig::from_lookup(|k| match k {
        "NESSIE_PROVIDER_MODE" => Some("live".into()),
        "NESSIE_API_KEY" => Some(canary.into()),
        _ => None,
    });
    let text = format!("{r:?}");
    eprintln!("REVIEW-BADKEY {text}");
    assert!(!text.contains("BADCANARY"), "{text}");
}

// ---- 7. The memory store during a slow bank call (N5) ---------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_memory_store_requests_wait_for_a_slow_bank_call() {
    let db = TestDb::new(Kind::Memory).await.unwrap();
    let (srv, ctl) = stub();
    let t = app_with_stub(&db, &srv, 5.0);
    let d = demo(&t).await;
    set_up(&t, &d).await;
    let a = approved(&t, &d, &d.hollywood, ITEM_DEPOSIT).await;
    ctl.pay_delay_ms.store(1500, Ordering::SeqCst);
    let (t1, d1) = (fork(&t), d.clone());
    let slow = tokio::spawn(async move {
        execute(&t1, &d1, &d1.hollywood, ITEM_DEPOSIT, &a["approval_id"]).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    let r = t.get("/api/v1/projects", Some(&d.tok)).await;
    let waited = started.elapsed();
    assert_eq!(r.status, StatusCode::OK);
    let r = slow.await.unwrap();
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    eprintln!("REVIEW-N5 memory store: GET /projects during a 1.5 s bank call took {waited:?}");
    // nessie-3 (F5 for execute): the bank call runs with no transaction open, so the store's
    // global lock is free during it (before: 1.30 s, the whole call).
    assert!(waited < Duration::from_millis(700), "{waited:?}");
    db.finish().await;
}

// ---- 8. PostgreSQL concurrency -----------------------------------------------------------------------

async fn pg_app(srv: &MockServer) -> Option<(TestDb, TestApp)> {
    let db = TestDb::new(Kind::Postgres).await?;
    let t = app_with_stub(&db, srv, 5.0);
    Some((db, t))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn review_pg_twenty_concurrent_executes_of_one_approval() {
    let (srv, ctl) = stub();
    let Some((db, t)) = pg_app(&srv).await else {
        return;
    };
    let d = demo(&t).await;
    set_up(&t, &d).await;
    let a = approved(&t, &d, &d.hollywood, ITEM_DEPOSIT).await;
    ctl.pay_delay_ms.store(400, Ordering::SeqCst);
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let (tt, dd, id) = (fork(&t), d.clone(), a["approval_id"].clone());
        set.spawn(async move { execute(&tt, &dd, &dd.hollywood, ITEM_DEPOSIT, &id).await });
    }
    let mut codes = std::collections::BTreeMap::new();
    while let Some(r) = set.join_next().await {
        let r = r.unwrap();
        let key = format!(
            "{} {}",
            r.status.as_u16(),
            r.body["error_code"].as_str().unwrap_or("-")
        );
        *codes.entry(key).or_insert(0) += 1;
    }
    eprintln!(
        "REVIEW-PG-20 {codes:?}; bank POSTs {}",
        money_posts(&srv).len()
    );
    assert_eq!(money_posts(&srv).len(), 1);
    assert_eq!(codes.get("200 -"), Some(&1));
    assert_eq!(
        approval_row(&t, &a["approval_id"]).await.approval_status,
        ApprovalStatus::Executed
    );
    db.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn review_pg_two_approvals_for_one_plan_item() {
    let (srv, ctl) = stub();
    let Some((db, t)) = pg_app(&srv).await else {
        return;
    };
    let d = demo(&t).await;
    set_up(&t, &d).await;
    let prop = d.hollywood.clone();
    // Two APPROVED approvals for the same plan item and the same plan (another attempt id, so
    // another payload): executed at the same time, one reaches the bank.
    let first = approved(&t, &d, &prop, ITEM_DEPOSIT).await;
    let mut p: JsonMap = serde_json::from_value(first["action_payload"].clone()).unwrap();
    let other_attempt = "abcdef012345";
    let old = p["attempt_id"].as_str().unwrap().to_owned();
    p.insert("attempt_id".into(), json!(other_attempt));
    let desc = p["description"]
        .as_str()
        .unwrap()
        .replace(&old, other_attempt);
    p.insert("description".into(), json!(desc));
    let second = other_approval(&t, &d, ActionType::FinancialCommitment, p, Some("APPROVED")).await;
    ctl.pay_delay_ms.store(400, Ordering::SeqCst);
    let (t1, d1, i1) = (fork(&t), d.clone(), first["approval_id"].clone());
    let (t2, d2, i2) = (fork(&t), d.clone(), second["approval_id"].clone());
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { execute(&t1, &d1, &d1.hollywood, ITEM_DEPOSIT, &i1).await }),
        tokio::spawn(async move { execute(&t2, &d2, &d2.hollywood, ITEM_DEPOSIT, &i2).await }),
    );
    let (r1, r2) = (r1.unwrap(), r2.unwrap());
    eprintln!(
        "REVIEW-PG-2APPROVALS {} {} / {} {}; bank POSTs {}",
        r1.status.as_u16(),
        r1.body["error_code"],
        r2.status.as_u16(),
        r2.body["error_code"],
        money_posts(&srv).len()
    );
    assert_eq!(money_posts(&srv).len(), 1);
    let mut s = [r1.status.as_u16(), r2.status.as_u16()];
    s.sort_unstable();
    assert_eq!(s, [200, 409]);
    // Sequentially, after a changed bill of materials: the new approval is refused at request
    // time, and an earlier approval that matches again is refused at execute time.
    ctl.pay_delay_ms.store(0, Ordering::SeqCst);
    let f1 = approved(&t, &d, &prop, ITEM_FURNISHING).await;
    let r = execute(&t, &d, &prop, ITEM_FURNISHING, &f1["approval_id"]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = request_approval(&t, &d, &prop, ITEM_FURNISHING).await;
    assert_problem(&r, 409, "NESSIE_ALREADY_ATTEMPTED");
    assert_eq!(money_posts(&srv).len(), 2);
    db.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn review_pg_ten_concurrent_set_ups() {
    let (srv, ctl) = stub();
    let Some((db, t)) = pg_app(&srv).await else {
        return;
    };
    let d = demo(&t).await;
    ctl.setup_delay_ms.store(150, Ordering::SeqCst);
    let mut set = tokio::task::JoinSet::new();
    for i in 0..10 {
        let (tt, dd) = (fork(&t), d.clone());
        set.spawn(async move {
            let body = if i % 2 == 0 {
                json!({})
            } else {
                json!({"opening_balance_minor": 5_000})
            };
            tt.post(&sandbox_path(&dd.pid), Some(&dd.tok), &body).await
        });
    }
    let mut statuses = Vec::new();
    let mut accounts = BTreeSet::new();
    while let Some(r) = set.join_next().await {
        let r = r.unwrap();
        statuses.push(r.status.as_u16());
        accounts.insert(r.body["account_id"].to_string());
    }
    statuses.sort_unstable();
    eprintln!(
        "REVIEW-PG-SETUP statuses {statuses:?}, accounts {accounts:?}, POST /customers {}",
        srv.requests()
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/customers")
            .count()
    );
    assert_eq!(statuses.iter().filter(|s| **s == 201).count(), 1);
    assert_eq!(statuses.iter().filter(|s| **s == 200).count(), 9);
    assert_eq!(accounts.len(), 1);
    assert_eq!(records(&t, &d.pid).await.len(), 1);
    db.finish().await;
}

/// A slow execute (1.5 s bank call) of one project alongside that project's other writers: a
/// search, a profile update, a layout generation, an approval decision, another approval request
/// and a plan read. Nothing deadlocks; who waits for the bank call is measured. The locks the
/// execute holds during its bank call are read from `pg_locks`.
///
/// nessie-3: the execute holds nothing during its bank call (asserted below), so these writers
/// now run at the same time as each other instead of queueing behind the execute. Run all at
/// once, the approval decision deadlocks with the search and the profile update (PostgreSQL
/// `40P01`, a 500): the decision takes the project's event lock and then, inserting its event, a
/// key-share lock on the project row, while a search or profile update holds the project row
/// `FOR UPDATE` and waits for the event lock. That cycle is in shared code, not in a Nessie route:
/// it reproduces with no Nessie request at all (nessie-3 report). The old execute hid it by making
/// the search and the update wait for its bank call. So the writers run in two waves, both during
/// the bank call: the decision with the writers that take no project row lock, then the search,
/// the profile update and the approval request.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_pg_a_slow_execute_alongside_the_projects_other_writers() {
    let (srv, ctl) = stub();
    let Some((db, t)) = pg_app(&srv).await else {
        return;
    };
    let schema = db.schema_name().unwrap().to_owned();
    let d = demo(&t).await;
    set_up(&t, &d).await;
    let a = approved(&t, &d, &d.hollywood, ITEM_DEPOSIT).await;
    let pending = request_approval(&t, &d, &d.alhambra, ITEM_DEPOSIT).await;
    let pending = pending.body["approval"].clone();
    ctl.pay_delay_ms.store(1500, Ordering::SeqCst);
    let (t0, d0, id0) = (fork(&t), d.clone(), a["approval_id"].clone());
    let slow = tokio::spawn(async move {
        let s = Instant::now();
        let r = execute(&t0, &d0, &d0.hollywood, ITEM_DEPOSIT, &id0).await;
        (r, s.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(400)).await;

    // What the execute's connection holds now, probed from another connection without waiting.
    let url = pg_url().unwrap();
    let opts = <sqlx::postgres::PgConnectOptions as std::str::FromStr>::from_str(
        &ss_server::config::normalize_database_url(&url),
    )
    .unwrap();
    let mut c = sqlx::ConnectOptions::connect(&opts).await.unwrap();
    let pid: Uuid = d.pid.parse().unwrap();
    let prop: Uuid = d.hollywood.parse().unwrap();
    let approval: Uuid = a["approval_id"].as_str().unwrap().parse().unwrap();
    let probes: Vec<(&str, String, Option<Uuid>)> = vec![
        (
            "approval row FOR UPDATE NOWAIT",
            format!("SELECT 1 FROM {schema}.approvals WHERE id = $1 FOR UPDATE NOWAIT"),
            Some(approval),
        ),
        (
            "project row FOR UPDATE NOWAIT",
            format!("SELECT 1 FROM {schema}.projects WHERE id = $1 FOR UPDATE NOWAIT"),
            Some(pid),
        ),
        (
            "project row FOR KEY SHARE NOWAIT",
            format!("SELECT 1 FROM {schema}.projects WHERE id = $1 FOR KEY SHARE NOWAIT"),
            Some(pid),
        ),
        (
            "plan item lock (try)",
            format!(
                "SELECT pg_try_advisory_xact_lock({})",
                svc::plan_item_lock_key(pid, prop, ITEM_DEPOSIT)
            ),
            None,
        ),
        (
            "event lock (try)",
            format!(
                "SELECT pg_try_advisory_xact_lock({})",
                ss_server::services::events::event_lock_key(Some(pid))
            ),
            None,
        ),
        (
            "sandbox lock (try)",
            format!(
                "SELECT pg_try_advisory_xact_lock({})",
                svc::sandbox_lock_key(pid)
            ),
            None,
        ),
    ];
    let mut held = Vec::new();
    for (name, sql, bind) in probes {
        let out = match bind {
            Some(id) => sqlx::query(&sql)
                .bind(id)
                .fetch_optional(&mut c)
                .await
                .map(|r| format!("acquired (row found: {})", r.is_some())),
            None => sqlx::query_scalar::<_, bool>(&sql)
                .fetch_one(&mut c)
                .await
                .map(|b| {
                    if b {
                        "free".to_owned()
                    } else {
                        "HELD".to_owned()
                    }
                }),
        };
        let out = match out {
            Ok(v) => v,
            Err(e) => format!("HELD ({e})"),
        };
        if out.starts_with("HELD") {
            held.push(name);
        }
        eprintln!("REVIEW-PG-LOCKS during the bank call, {name}: {out}");
    }
    let idle: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.state, left(a.query, 90) FROM pg_stat_activity a WHERE a.application_name = $1 AND a.state <> 'idle'",
    )
    .bind(&schema)
    .fetch_all(&mut c)
    .await
    .unwrap();
    for r in &idle {
        eprintln!("REVIEW-PG-ACTIVITY {r:?}");
    }
    // nessie-3 (F4): during the bank call the execute holds no lock and no open transaction.
    assert!(held.is_empty(), "held during the bank call: {held:?}");
    assert!(
        idle.is_empty(),
        "sessions not idle during the bank call: {idle:?}"
    );

    // The project's other writers, all at once.
    let mut set = tokio::task::JoinSet::new();
    let jobs: Vec<(&str, Method, String, Option<Value>)> = vec![
        (
            "search",
            Method::POST,
            format!("/api/v1/projects/{}/properties/search", d.pid),
            Some(json!({"city": "Los Angeles", "state": "CA"})),
        ),
        (
            "layout generation",
            Method::POST,
            format!("/api/v1/projects/{}/layouts/generate", d.pid),
            Some(json!({"property_id": d.hollywood, "room_requirements": [
                {"room_type": "treatment", "count": 3, "min_area_sqm": 10, "min_width_m": 2.8, "needs_plumbing": true},
                {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
                {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5}
            ], "candidate_count": 1})),
        ),
        (
            "approval decision",
            Method::POST,
            format!(
                "/api/v1/approvals/{}/decision",
                pending["approval_id"].as_str().unwrap()
            ),
            Some(
                json!({"decision": "REJECTED", "action_payload_hash": pending["action_payload_hash"]}),
            ),
        ),
        (
            "approval request (renovation)",
            Method::POST,
            pay_path(&d.pid, &d.hollywood, ITEM_RENOVATION, "approval"),
            None,
        ),
        (
            "plan read",
            Method::GET,
            prop_path(&d.pid, &d.hollywood, "plan"),
            None,
        ),
        ("project list", Method::GET, "/api/v1/projects".into(), None),
    ];
    let profile_body = demo_project_body()["business_profile"].clone();
    let mut jobs = jobs;
    jobs.push((
        "profile update",
        Method::PUT,
        format!("/api/v1/projects/{}/business-profile", d.pid),
        Some(profile_body),
    ));
    let mut results = Vec::new();
    let (second_wave, first_wave): (Vec<_>, Vec<_>) = jobs
        .into_iter()
        // The approval request asks the stub for the account first, and the stub serves one
        // connection at a time: it waits for the stalled payment POST, so it goes last.
        .partition(|(name, ..)| {
            matches!(
                *name,
                "search" | "profile update" | "approval request (renovation)"
            )
        });
    for wave in [first_wave, second_wave] {
        for (name, m, path, body) in wave {
            let (tt, tok) = (fork(&t), d.tok.clone());
            set.spawn(async move {
                let s = Instant::now();
                let r = tt.call(m, &path, Some(&tok), body.as_ref()).await;
                (
                    name,
                    r.status.as_u16(),
                    r.text.chars().take(160).collect::<String>()
                        + r.body["task"]["error_code"].as_str().unwrap_or_default(),
                    s.elapsed(),
                )
            });
        }
        while let Some(r) = set.join_next().await {
            results.push(r.unwrap());
        }
    }
    let (r, took) = slow.await.unwrap();
    eprintln!(
        "REVIEW-PG-OVERLAP execute: {} in {took:?}",
        r.status.as_u16()
    );
    results.sort_by(|x, y| x.0.cmp(y.0));
    for (name, status, text, took) in &results {
        eprintln!("REVIEW-PG-OVERLAP {name}: {status} in {took:?} {text}");
    }
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    for (name, status, text, _) in &results {
        assert!(*status < 500, "{name}: {status} {text}");
    }
    assert_eq!(money_posts(&srv).len(), 1);
    db.finish().await;
}

/// Pool use during slow executes: the test pool has 4 connections. Four executes of one plan item
/// wait on its lock while the first is at the bank; an unrelated read waits for a connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn review_pg_waiting_executes_hold_pool_connections() {
    let (srv, ctl) = stub();
    let Some((db, t)) = pg_app(&srv).await else {
        return;
    };
    let d = demo(&t).await;
    set_up(&t, &d).await;
    let a = approved(&t, &d, &d.hollywood, ITEM_DEPOSIT).await;
    ctl.pay_delay_ms.store(2000, Ordering::SeqCst);
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let (tt, dd, id) = (fork(&t), d.clone(), a["approval_id"].clone());
        set.spawn(async move { execute(&tt, &dd, &dd.hollywood, ITEM_DEPOSIT, &id).await });
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = Instant::now();
    let r = t.get("/api/v1/projects", Some(&d.tok)).await;
    let waited = s.elapsed();
    while set.join_next().await.is_some() {}
    eprintln!(
        "REVIEW-PG-POOL an unrelated GET /projects during 4 executes of one item took {waited:?} ({})",
        r.status.as_u16()
    );
    // nessie-3 (F3): the waiting executes are refused at once (409, the claim is committed) and
    // the one at the bank holds no connection (before: 1.52 s, the whole call).
    assert_eq!(r.status, StatusCode::OK);
    assert!(waited < Duration::from_millis(1000), "{waited:?}");
    db.finish().await;
}

// ---- 9. Payloads for the Python payload_hash parity check ------------------------------------------

fn fill(v: &mut Value) {
    match v {
        Value::String(s) => {
            if let Some(n) = s.strip_prefix("<id:").and_then(|r| r.strip_suffix('>')) {
                let n: u64 = n.parse().unwrap();
                *s = format!("00000000-0000-4000-8000-{n:012}");
            } else if s == "<timestamp>" {
                *s = "2026-11-14T00:00:00Z".into();
            }
        }
        Value::Array(a) => a.iter_mut().for_each(fill),
        Value::Object(o) => o.values_mut().for_each(fill),
        _ => {}
    }
}

/// Every plan item of every golden scenario, both banks, with and without a bill of materials,
/// rounds 0 to 2 of the server's attempt id (`attempt_id_for(base_payload_hash(..), round)`),
/// hashed with the server's own `payload_hash` (what `require_approved` compares). Written to
/// `$REVIEW_PARITY_OUT` for `check_parity.py`; skipped when that is unset.
#[test]
fn review_dump_payloads_for_python_parity() {
    let Ok(out) = std::env::var("REVIEW_PARITY_OUT") else {
        eprintln!("SKIPPED: REVIEW_PARITY_OUT is not set");
        return;
    };
    let golden = read_json("apps/backend/tests-golden/domain/part2_pipeline.json");
    let mut rows = Vec::new();
    for c in golden["cases"].as_array().unwrap() {
        for (_addr, p) in c["output"]["properties"].as_object().unwrap() {
            for (_h, s) in p["scenarios"].as_object().unwrap() {
                let mut s = s.clone();
                fill(&mut s);
                let sc = CostScenario::from_json_value(s).unwrap();
                for sim in [Simulation::FixtureBank, Simulation::NessieSandbox] {
                    let ids: [&str; 5] = match sim {
                        Simulation::FixtureBank => [
                            "fixture_acct_0002",
                            "fixture_merch_0003",
                            "fixture_merch_0004",
                            "fixture_merch_0005",
                            "fixture_bill_0007",
                        ],
                        Simulation::NessieSandbox => [
                            "5f8d0d55b54764421b7156c3",
                            "5f8d0d55b54764421b7156c4",
                            "5f8d0d55b54764421b7156c5",
                            "5f8d0d55b54764421b7156c6",
                            "5f8d0d55b54764421b7156c7",
                        ],
                    };
                    let targets = SandboxTargets {
                        account_id: ids[0].into(),
                        landlord_merchant_id: Some(ids[1].into()),
                        contractor_merchant_id: Some(ids[2].into()),
                        supplier_merchant_id: Some(ids[3].into()),
                        occupancy_bill_id: Some(ids[4].into()),
                    };
                    let bom = BomTotals {
                        bom_id: Uuid::from_u128(77),
                        project_id: sc.project_id,
                        property_id: sc.property_id,
                        design_version_id: Uuid::from_u128(78),
                        currency: "USD".into(),
                        estimated_total_minor: 1_377_651,
                        approved_total_minor: None,
                        status: "DRAFT".into(),
                        floor: ss_nessie::plan::BomFloor::NotAnEstimate,
                    };
                    for with_bom in [false, true] {
                        let plan = plan_move_in_payments(&sc, with_bom.then_some(&bom), sim, None)
                            .unwrap();
                        for p in &plan.payments {
                            let probe = plan
                                .intent_for(&p.plan_item_id, &targets, "00000000")
                                .unwrap();
                            let base = base_payload_hash(&probe.approval_payload());
                            for round in 0..3 {
                                let attempt = attempt_id_for(&base, round);
                                let intent = plan
                                    .intent_for(&p.plan_item_id, &targets, &attempt)
                                    .unwrap();
                                let payload = intent.approval_payload();
                                rows.push(json!({
                                    "source": "golden",
                                    "case": c["name"],
                                    "payload": payload,
                                    "hash": payload_hash(&payload),
                                    "base_hash": base,
                                    "round": round,
                                    "attempt_id": attempt,
                                }));
                            }
                        }
                    }
                }
            }
        }
    }
    eprintln!("REVIEW-PARITY {} payloads", rows.len());
    std::fs::write(out, serde_json::to_string(&rows).unwrap()).unwrap();
}

/// The approvals the routes actually stored (both banks, in-memory store), written to
/// `$REVIEW_PARITY_OUT_E2E`; skipped when that is unset.
#[tokio::test]
async fn review_dump_stored_approvals_for_python_parity() {
    let Ok(out) = std::env::var("REVIEW_PARITY_OUT_E2E") else {
        eprintln!("SKIPPED: REVIEW_PARITY_OUT_E2E is not set");
        return;
    };
    let db = TestDb::new(Kind::Memory).await.unwrap();
    let (srv, _ctl) = stub();
    let mut rows = Vec::new();
    for fixture in [true, false] {
        let t = if fixture {
            app_with_fixture(&db)
        } else {
            app_with_stub(&db, &srv, 5.0)
        };
        let d = demo(&t).await;
        let r = t
            .post(
                &sandbox_path(&d.pid),
                Some(&d.tok),
                &json!({"opening_balance_minor": 1_000_000_000i64}),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        for prop in [d.hollywood.clone(), d.alhambra.clone()] {
            for item in [
                ITEM_DEPOSIT,
                ITEM_RENOVATION,
                ITEM_FURNISHING,
                ITEM_OCCUPANCY_BILL,
            ] {
                let a = approved(&t, &d, &prop, item).await;
                let r = execute(&t, &d, &prop, item, &a["approval_id"]).await;
                assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            }
            let a = approved(&t, &d, &prop, ITEM_FIRST_MONTH).await;
            let r = execute(&t, &d, &prop, ITEM_FIRST_MONTH, &a["approval_id"]).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        }
        let ap = t
            .get(
                &format!("/api/v1/projects/{}/approvals", d.pid),
                Some(&d.tok),
            )
            .await;
        for a in ap.body.as_array().unwrap() {
            rows.push(json!({"source": "route", "payload": a["action_payload"], "hash": a["action_payload_hash"]}));
        }
    }
    eprintln!("REVIEW-PARITY-E2E {} stored approvals", rows.len());
    std::fs::write(out, serde_json::to_string(&rows).unwrap()).unwrap();
    db.finish().await;
}
