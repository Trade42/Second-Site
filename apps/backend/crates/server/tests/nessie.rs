//! Capital One Nessie sandbox routes (`http::routes::nessie`, `services::nessie`), on both stores:
//! the whole flow on the fixture project after a search, a layout and its bill of materials;
//! approvals that are pending, rejected, approved and spent; a changed plan; isolation and the
//! request-check order; every answer labelled for the fixture bank and for a stub that claims to
//! be the Capital One sandbox; bank refusals, timeouts, 5xx and mismatched answers; and, on
//! PostgreSQL only, concurrent executes and set-ups and the Rust-owned table.
//!
//! No test calls the real Nessie API: the fixture bank is in process, and the "sandbox" is a
//! loopback stub (`common::mock_http`).

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::mock_http::{Canned, MockServer, Recorded};
use serde_json::{json, Value};
use ss_domain::integrations::base::RetryPolicy;
use ss_nessie::config::{ApiKey, ProviderMode};
use ss_nessie::plan::{
    ITEM_DEPOSIT, ITEM_FIRST_MONTH, ITEM_FURNISHING, ITEM_OCCUPANCY_BILL, ITEM_RENOVATION,
};
use ss_nessie::{LiveNessieBank, PaymentAuthorization, FIXTURE_BANK_DISCLAIMER};
use ss_server::config::Settings;
use ss_server::contracts::{ActionType, ApprovalStatus, JsonMap};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::nessie::{
    self as svc, approval_evidence, attempt_id_for, install, plan_item_lock_key, sandbox_lock_key,
    NessieState, NESSIE_RECORDS,
};
use ss_server::store::memory::MemoryStore;
use ss_server::store::repo::APPROVALS;
use ss_server::store::{
    schema, Backend, Filter, Query, Row, Savepoint, Store, StoreError, StoreResult, TableDef, Tx,
    UpdateOutcome,
};
use uuid::Uuid;

use common::*;

const HOLLYWOOD: &str = "6922 W HOLLYWOOD BLVD";
const ALHAMBRA: &str = "5380 E ALHAMBRA AVE";
const FIXTURE: &str = "FIXTURE_IN_MEMORY_BANK";
const SANDBOX: &str = "CAPITAL_ONE_NESSIE_SANDBOX";

// ---- Set-up helpers ------------------------------------------------------------------------------

#[derive(Clone)]
struct Demo {
    tok: String,
    pid: String,
    hollywood: String,
    alhambra: String,
}

/// A test app whose Nessie bank is a fresh fixture bank (whatever the environment says).
fn app_with_fixture(db: &TestDb) -> TestApp {
    let t = TestApp::new(db, &[]);
    install(&t.app, NessieState::fixture());
    t
}

/// Dev token, the demo project with its profile, and the fixture search (two properties, each
/// with a live cost scenario).
async fn demo(t: &TestApp) -> Demo {
    let (tok, _) = t.dev_token().await;
    let pid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    let by: HashMap<String, String> = props
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["property_identity"]["street_address"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                p["property_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    Demo {
        tok,
        pid,
        hollywood: by[HOLLYWOOD].clone(),
        alhambra: by[ALHAMBRA].clone(),
    }
}

fn program(treatments: i64) -> Value {
    json!([
        {"room_type": "treatment", "count": treatments, "min_area_sqm": 10, "min_width_m": 2.8, "needs_plumbing": true},
        {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
        {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5},
    ])
}

/// Generates a layout for the property and its bill of materials; returns the bill.
async fn layout_with_bom(t: &TestApp, d: &Demo, property: &str, treatments: i64) -> Value {
    let r = t
        .post(
            &format!("/api/v1/projects/{}/layouts/generate", d.pid),
            Some(&d.tok),
            &json!({"property_id": property, "room_requirements": program(treatments), "candidate_count": 1}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let v = r.body["task"]["output_reference"]["design_version_ids"][0]
        .as_str()
        .unwrap()
        .to_owned();
    let bom = t
        .get(
            &format!("/api/v1/projects/{}/layouts/{v}/bom", d.pid),
            Some(&d.tok),
        )
        .await;
    assert_eq!(bom.status, StatusCode::OK, "{}", bom.text);
    bom.body
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

/// A success answer: the label at the top level, as the bank in use says.
fn labelled(r: &Resp, sim: &str) {
    assert!(r.status.is_success(), "{} {}", r.status, r.text);
    assert_eq!(r.body["simulation"], sim, "{}", r.text);
    let disclaimer = r.body["disclaimer"].as_str().unwrap_or_default();
    assert!(
        disclaimer.contains("Mock money") || disclaimer.contains("mock money"),
        "{}",
        r.text
    );
    if sim == FIXTURE {
        assert_eq!(disclaimer, FIXTURE_BANK_DISCLAIMER);
    } else {
        assert_eq!(disclaimer, ss_nessie::NESSIE_SANDBOX_DISCLAIMER);
    }
}

/// A bank-outcome error: a `ProblemDetail` whose details carry the label.
fn labelled_problem(r: &Resp, status: u16, code: &str, sim: &str) {
    assert_problem(r, status, code);
    assert_eq!(r.body["details"]["simulation"], sim, "{}", r.text);
    assert!(r.body["details"]["disclaimer"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase()
        .contains("mock money"));
}

async fn request_approval(t: &TestApp, d: &Demo, prop: &str, item: &str) -> Resp {
    t.call(
        axum::http::Method::POST,
        &pay_path(&d.pid, prop, item, "approval"),
        Some(&d.tok),
        None,
    )
    .await
}

async fn decide(t: &TestApp, tok: &str, approval: &Value, decision: &str) -> Resp {
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
    r
}

async fn execute(t: &TestApp, d: &Demo, prop: &str, item: &str, approval_id: &Value) -> Resp {
    t.post(
        &pay_path(&d.pid, prop, item, "execute"),
        Some(&d.tok),
        &json!({"approval_id": approval_id}),
    )
    .await
}

/// Request, approve and execute one plan item; returns the execute answer.
async fn approve_and_execute(t: &TestApp, d: &Demo, prop: &str, item: &str) -> Resp {
    let a = request_approval(t, d, prop, item).await;
    assert!(a.status.is_success(), "{}", a.text);
    decide(t, &d.tok, &a.body["approval"], "APPROVED").await;
    execute(t, d, prop, item, &a.body["approval"]["approval_id"]).await
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

async fn approval_status(t: &TestApp, id: &Value) -> ApprovalStatus {
    let mut tx = t.app.store.begin().await.unwrap();
    let a = APPROVALS
        .require(&mut *tx, id.as_str().unwrap().parse().unwrap())
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    a.approval_status
}

/// Transactions the bank in use holds for the account (fixture bank or stub).
fn bank_transactions(t: &TestApp, account: &str) -> usize {
    t.app
        .nessie()
        .bank()
        .unwrap()
        .list_account_transactions(account, 1000)
        .unwrap()
        .record
        .len()
}

// ---- The whole flow --------------------------------------------------------------------------------

both_backends!(the_whole_flow_on_the_fixture_project, |db| {
    let t = app_with_fixture(db);
    let d = demo(&t).await;
    let bom = layout_with_bom(&t, &d, &d.hollywood, 3).await;
    let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());

    // Nothing is set up yet.
    let r = t.get(&sandbox_path(pid), Some(tok)).await;
    assert_problem(&r, 404, "NESSIE_SANDBOX_NOT_SET_UP");

    // Set-up: the profile's opening capital, labelled as the owner's figure in mock money.
    let r = t.post(&sandbox_path(pid), Some(tok), &json!({})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    labelled(&r, FIXTURE);
    assert_eq!(r.body["created"], true);
    assert_eq!(r.body["sandbox_status"], "READY");
    assert_eq!(r.body["opening_balance_minor"], 18_000_000);
    assert_eq!(
        r.body["opening_balance_source"],
        "BUSINESS_PROFILE_OPENING_CAPITAL"
    );
    let note = r.body["opening_balance_note"].as_str().unwrap();
    assert!(
        note.contains("the opening capital the owner reported"),
        "{note}"
    );
    assert!(note.contains("mock money"), "{note}");
    assert_eq!(r.body["account"]["simulation"], FIXTURE);
    assert_eq!(r.body["account"]["record"]["available_cents"], 18_000_000);
    let account = r.body["account_id"].as_str().unwrap().to_owned();
    assert!(account.starts_with("fixture_"), "{account}");

    // Once per project: a second set-up returns the same sandbox and creates nothing.
    let again = t
        .post(
            &sandbox_path(pid),
            Some(tok),
            &json!({"opening_balance_minor": 5}),
        )
        .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text);
    assert_eq!(again.body["created"], false);
    assert_eq!(again.body["account_id"], account.as_str());
    let r = t.get(&sandbox_path(pid), Some(tok)).await;
    labelled(&r, FIXTURE);
    assert_eq!(r.body["account_id"], account.as_str());

    // The plan: the property's live scenario and its latest bill of materials, said explicitly.
    let plan = t.get(&prop_path(pid, prop, "plan"), Some(tok)).await;
    labelled(&plan, FIXTURE);
    assert_eq!(plan.body["plan"]["simulation"], FIXTURE);
    assert_eq!(plan.body["inputs"]["live_scenarios"], 1);
    assert!(plan.body["inputs"]["scenario_rule"]
        .as_str()
        .unwrap()
        .contains("latest by created_at"));
    assert_eq!(
        plan.body["inputs"]["bill_of_materials"]["bom_id"],
        bom["bom_id"]
    );
    assert_eq!(plan.body["plan"]["bom_id"], bom["bom_id"]);
    let furnishing = plan.body["plan"]["payments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["plan_item_id"] == ITEM_FURNISHING)
        .unwrap()
        .clone();
    assert_eq!(furnishing["amount_expected_minor"], bom["estimated_total"]);
    let items: Vec<&str> = plan.body["plan"]["payments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["plan_item_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        items,
        [
            ITEM_DEPOSIT,
            ITEM_OCCUPANCY_BILL,
            ITEM_FIRST_MONTH,
            ITEM_RENOVATION,
            ITEM_FURNISHING
        ]
    );

    // Affordability: the sandbox account against the plan, with the profile's reserve.
    let aff = t
        .get(&prop_path(pid, prop, "affordability"), Some(tok))
        .await;
    labelled(&aff, FIXTURE);
    assert_eq!(aff.body["report"]["account_id"], account.as_str());
    assert_eq!(
        aff.body["working_capital_reserve_source"],
        "BUSINESS_PROFILE"
    );
    assert_eq!(
        aff.body["report"]["working_capital_reserve_minor"],
        3_000_000
    );
    assert!(aff.body["report"]["headline"]
        .as_str()
        .unwrap()
        .starts_with("Simulation only:"));

    // Approval for the deposit: a CONSEQUENTIAL FINANCIAL_COMMITMENT whose payload names the
    // plan item and the attempt; asking again returns the same open approval.
    let a = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.text);
    labelled(&a, FIXTURE);
    assert_conforms("ApprovalRequest", &a.body["approval"]);
    let approval = a.body["approval"].clone();
    assert_eq!(approval["action_type"], "FINANCIAL_COMMITMENT");
    assert_eq!(approval["risk_category"], "CONSEQUENTIAL");
    assert_eq!(approval["approval_status"], "PENDING");
    let payload = &approval["action_payload"];
    assert_eq!(payload["plan_item_id"], ITEM_DEPOSIT);
    assert_eq!(payload["attempt_id"], a.body["attempt_id"]);
    assert_eq!(payload["account_id"], account.as_str());
    assert_eq!(payload["simulation"], FIXTURE);
    let description = payload["description"].as_str().unwrap();
    assert!(
        description.contains(ITEM_DEPOSIT)
            && description.contains(a.body["attempt_id"].as_str().unwrap())
    );
    assert!(approval["summary"]
        .as_str()
        .unwrap()
        .starts_with("Simulation only:"));
    assert!(a.body["next_step"].as_str().unwrap().contains(&format!(
        "/api/v1/approvals/{}/decision",
        approval["approval_id"].as_str().unwrap()
    )));
    let same = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    assert_eq!(same.status, StatusCode::OK, "{}", same.text);
    assert_eq!(same.body["created"], false);
    assert_eq!(
        same.body["approval"]["approval_id"],
        approval["approval_id"]
    );

    // Execute while PENDING: refused, nothing reaches the bank, nothing recorded.
    let r = execute(&t, &d, prop, ITEM_DEPOSIT, &approval["approval_id"]).await;
    assert_problem(&r, 409, "APPROVAL_REQUIRED");
    assert_eq!(bank_transactions(&t, &account), 0);
    assert!(attempt(&t, pid, prop, ITEM_DEPOSIT).await.is_none());

    // Rejected: still refused. A new request then gets a fresh approval (another attempt id).
    decide(&t, tok, &approval, "REJECTED").await;
    let r = execute(&t, &d, prop, ITEM_DEPOSIT, &approval["approval_id"]).await;
    assert_problem(&r, 409, "APPROVAL_REQUIRED");
    assert_eq!(bank_transactions(&t, &account), 0);
    let fresh = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    assert_eq!(fresh.status, StatusCode::CREATED, "{}", fresh.text);
    assert_ne!(
        fresh.body["approval"]["approval_id"],
        approval["approval_id"]
    );
    assert_ne!(fresh.body["attempt_id"], a.body["attempt_id"]);
    let approval = fresh.body["approval"].clone();

    // APPROVED: executed once, recorded as pending (never "completed"), approval spent.
    decide(&t, tok, &approval, "APPROVED").await;
    let r = execute(&t, &d, prop, ITEM_DEPOSIT, &approval["approval_id"]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    labelled(&r, FIXTURE);
    assert_eq!(r.body["attempt_status"], "SENT");
    assert_eq!(r.body["approval_status"], "EXECUTED");
    assert_eq!(r.body["record"]["outcome"], "SIMULATED_PENDING");
    assert_eq!(r.body["record"]["amount_minor"], 1_200_000);
    assert!(r.body["record"]["note"]
        .as_str()
        .unwrap()
        .contains("pending"));
    assert!(!r.text.contains("SIMULATED_COMPLETED"));
    assert_eq!(bank_transactions(&t, &account), 1);
    assert_eq!(
        approval_status(&t, &approval["approval_id"]).await,
        ApprovalStatus::Executed
    );
    let stored = attempt(&t, pid, prop, ITEM_DEPOSIT).await.unwrap();
    assert_eq!(stored.attempt_status, "SENT");
    assert_eq!(
        stored.approval_id.to_string(),
        approval["approval_id"].as_str().unwrap()
    );
    assert_eq!(
        stored.nessie_transaction_id.as_deref(),
        r.body["record"]["nessie_transaction_id"].as_str()
    );

    // A second execute with the same approval is refused and never reaches the bank.
    let r = execute(&t, &d, prop, ITEM_DEPOSIT, &approval["approval_id"]).await;
    assert_problem(&r, 409, "NESSIE_ALREADY_ATTEMPTED");
    assert_eq!(bank_transactions(&t, &account), 1);
    // So is a new approval for the same plan item.
    let r = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    assert_problem(&r, 409, "NESSIE_ALREADY_ATTEMPTED");

    // Refresh reads the stored transaction again (no id from the client).
    let r = t
        .call(
            axum::http::Method::POST,
            &pay_path(pid, prop, ITEM_DEPOSIT, "refresh"),
            Some(tok),
            None,
        )
        .await;
    labelled(&r, FIXTURE);
    assert_eq!(
        r.body["record"]["nessie_transaction_id"],
        stored.nessie_transaction_id.unwrap().as_str()
    );
    assert_eq!(r.body["transaction"]["simulation"], FIXTURE);
    assert_eq!(r.body["record"]["outcome"], "SIMULATED_PENDING");

    // The first month pays the recurring bill: refused until the bill item was executed.
    let r = request_approval(&t, &d, prop, ITEM_FIRST_MONTH).await;
    assert_problem(&r, 409, "NESSIE_BILL_REQUIRED");
    let r = approve_and_execute(&t, &d, prop, ITEM_OCCUPANCY_BILL).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["record"]["outcome"], "SIMULATED_BILL_RECORDED");
    assert_eq!(r.body["record"]["moves_money"], false);
    let bill = r.body["record"]["nessie_bill_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let a = request_approval(&t, &d, prop, ITEM_FIRST_MONTH).await;
    assert_eq!(
        a.body["approval"]["action_payload"]["bill_id"],
        bill.as_str()
    );
    assert_eq!(
        a.body["approval"]["action_payload"]["account_id"],
        account.as_str()
    );
    decide(&t, tok, &a.body["approval"], "APPROVED").await;
    let r = execute(
        &t,
        &d,
        prop,
        ITEM_FIRST_MONTH,
        &a.body["approval"]["approval_id"],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["record"]["nessie_bill_id"], bill.as_str());
    let r = t
        .call(
            axum::http::Method::POST,
            &pay_path(pid, prop, ITEM_OCCUPANCY_BILL, "refresh"),
            Some(tok),
            None,
        )
        .await;
    assert_problem(&r, 409, "NESSIE_NOTHING_TO_REFRESH");

    // Holds are subtracted from what affordability sees.
    let aff = t
        .get(&prop_path(pid, prop, "affordability"), Some(tok))
        .await;
    assert_eq!(
        aff.body["report"]["sandbox_pending_holds_minor"],
        1_200_000 + 690_000
    );
});

both_backends!(a_changed_plan_invalidates_an_earlier_approval, |db| {
    let t = app_with_fixture(db);
    let d = demo(&t).await;
    let first = layout_with_bom(&t, &d, &d.hollywood, 3).await;
    let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
    assert_eq!(
        t.post(&sandbox_path(pid), Some(tok), &json!({}))
            .await
            .status,
        StatusCode::CREATED
    );
    let a = request_approval(&t, &d, prop, ITEM_FURNISHING).await;
    assert_eq!(
        a.body["approval"]["action_payload"]["amount_minor"],
        first["estimated_total"]
    );
    decide(&t, tok, &a.body["approval"], "APPROVED").await;

    // A new bill of materials (another layout with fewer treatment rooms) changes the plan.
    let second = layout_with_bom(&t, &d, prop, 2).await;
    assert_ne!(second["estimated_total"], first["estimated_total"]);
    let plan = t.get(&prop_path(pid, prop, "plan"), Some(tok)).await;
    assert_eq!(plan.body["plan"]["bom_id"], second["bom_id"]);
    assert_eq!(plan.body["inputs"]["bills_of_materials_for_property"], 2);

    let r = execute(
        &t,
        &d,
        prop,
        ITEM_FURNISHING,
        &a.body["approval"]["approval_id"],
    )
    .await;
    assert_problem(&r, 409, "APPROVAL_PAYLOAD_MISMATCH");
    assert!(attempt(&t, pid, prop, ITEM_FURNISHING).await.is_none());
    assert_eq!(
        approval_status(&t, &a.body["approval"]["approval_id"]).await,
        ApprovalStatus::Approved
    );
    // The new plan gets its own approval, which executes.
    let r = approve_and_execute(&t, &d, prop, ITEM_FURNISHING).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["record"]["amount_minor"], second["estimated_total"]);
});

both_backends!(every_route_is_scoped_to_the_project_and_owner, |db| {
    let t = app_with_fixture(db);
    let d = demo(&t).await;
    let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
    assert_eq!(
        t.post(&sandbox_path(pid), Some(tok), &json!({}))
            .await
            .status,
        StatusCode::CREATED
    );
    let a = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    let approval_id = a.body["approval"]["approval_id"].clone();

    // Another owner: 404 project not found on every route.
    let other = t.token_for(Uuid::new_v4());
    let o = Some(other.as_str());
    let cases: Vec<(axum::http::Method, String, Option<Value>)> = vec![
        (axum::http::Method::POST, sandbox_path(pid), Some(json!({}))),
        (axum::http::Method::GET, sandbox_path(pid), None),
        (axum::http::Method::GET, prop_path(pid, prop, "plan"), None),
        (
            axum::http::Method::GET,
            prop_path(pid, prop, "affordability"),
            None,
        ),
        (
            axum::http::Method::POST,
            pay_path(pid, prop, ITEM_DEPOSIT, "approval"),
            None,
        ),
        (
            axum::http::Method::POST,
            pay_path(pid, prop, ITEM_DEPOSIT, "execute"),
            Some(json!({"approval_id": approval_id})),
        ),
        (
            axum::http::Method::POST,
            pay_path(pid, prop, ITEM_DEPOSIT, "refresh"),
            None,
        ),
    ];
    for (m, path, body) in &cases {
        let r = t.call(m.clone(), path, o, body.as_ref()).await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "project not found", "{path}");
    }

    // The same owner's other project: its routes do not see this project's property.
    let p2 = t.create_project(tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for what in ["plan", "affordability"] {
        let r = t.get(&prop_path(&p2, prop, what), Some(tok)).await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "property not found");
    }
    for what in ["approval", "refresh"] {
        let r = t
            .call(
                axum::http::Method::POST,
                &pay_path(&p2, prop, ITEM_DEPOSIT, what),
                Some(tok),
                None,
            )
            .await;
        assert_eq!(r.body["message"], "property not found", "{what}");
    }
    let r = t
        .post(
            &pay_path(&p2, prop, ITEM_DEPOSIT, "execute"),
            Some(tok),
            &json!({"approval_id": approval_id}),
        )
        .await;
    assert_eq!(r.body["message"], "property not found");
    // Its own property with this project's approval: the approval is not found there.
    let r = t.get(&sandbox_path(&p2), Some(tok)).await;
    assert_problem(&r, 404, "NESSIE_SANDBOX_NOT_SET_UP");

    // Unknown plan items and a reserve are 404 (not payments of the plan).
    for item in ["movein:nothing", "reserve:contingency"] {
        let r = request_approval(&t, &d, prop, item).await;
        assert_problem(&r, 404, "NOT_FOUND");
        let r = execute(&t, &d, prop, item, &approval_id).await;
        assert_problem(&r, 404, "NOT_FOUND");
    }
    // An unknown property of the project.
    let r = t
        .get(
            &prop_path(pid, &Uuid::new_v4().to_string(), "plan"),
            Some(tok),
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    // An approval id of another project is reported as missing.
    let r = execute(&t, &d, prop, ITEM_DEPOSIT, &json!(Uuid::new_v4())).await;
    assert_problem(&r, 404, "NOT_FOUND");
});

both_backends!(bad_requests_follow_the_check_order, |db| {
    let t = app_with_fixture(db);
    let d = demo(&t).await;
    let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
    let raw = |path: String, token: Option<String>, body: &'static str| {
        let mut b = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(tk) = token {
            b = b.header("authorization", format!("Bearer {tk}"));
        }
        b.body(axum::body::Body::from(body)).unwrap()
    };
    // Invalid JSON is first, even without a token.
    let r = t.send(raw(sandbox_path(pid), None, "{nope")).await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.text);
    assert_eq!(r.body["details"]["errors"][0]["type"], "json_invalid");
    let r = t
        .send(raw(
            pay_path(pid, prop, ITEM_DEPOSIT, "execute"),
            None,
            "{nope",
        ))
        .await;
    assert_eq!(r.status.as_u16(), 422);
    // Then the token, the project id, the owned project.
    let r = t.post(&sandbox_path(pid), None, &json!({})).await;
    assert_problem(&r, 401, "UNAUTHORIZED");
    let r = t
        .post(&sandbox_path("not-a-uuid"), Some(tok), &json!({}))
        .await;
    assert_eq!(r.status.as_u16(), 422);
    let other = t.token_for(Uuid::new_v4());
    let r = t
        .post(
            &sandbox_path(pid),
            Some(&other),
            &json!({"opening_balance_minor": -1}),
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    // Then the endpoint's own path parameters, then the body.
    let r = t
        .post(
            &pay_path(pid, "nope", ITEM_DEPOSIT, "execute"),
            Some(tok),
            &json!({}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.text);
    assert_eq!(
        r.body["details"]["errors"][0]["loc"],
        json!(["path", "property_id"])
    );
    let long = "x".repeat(65);
    let r = t
        .post(
            &pay_path(pid, prop, &long, "execute"),
            Some(tok),
            &json!({}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.text);
    assert_eq!(
        r.body["details"]["errors"][0]["loc"],
        json!(["path", "plan_item_id"])
    );
    for body in [
        json!({}),
        json!({"approval_id": "nope"}),
        json!({"approval_id": Uuid::new_v4(), "transaction_id": "t"}),
        json!([1]),
    ] {
        let r = t
            .post(
                &pay_path(pid, prop, ITEM_DEPOSIT, "execute"),
                Some(tok),
                &body,
            )
            .await;
        assert_eq!(r.status.as_u16(), 422, "{body} {}", r.text);
        assert_eq!(r.body["error_code"], "VALIDATION_ERROR");
    }
    // The body model comes before the records: a foreign property with a bad body is 422.
    let r = t
        .post(
            &pay_path(pid, &Uuid::new_v4().to_string(), ITEM_DEPOSIT, "execute"),
            Some(tok),
            &json!({}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 422);
    // Set-up bodies: negative, absurd and unknown fields are 422; nothing is created.
    for body in [
        json!({"opening_balance_minor": -1}),
        json!({"opening_balance_minor": 100_000_000_001i64}),
        json!({"opening_balance_minor": "10"}),
        json!({"balance": 10}),
    ] {
        let r = t.post(&sandbox_path(pid), Some(tok), &body).await;
        assert_eq!(r.status.as_u16(), 422, "{body} {}", r.text);
        assert_conforms("ProblemDetail", &r.body);
    }
    assert!(records(&t, pid).await.is_empty());
    // An explicit balance is used and said to be the owner's figure; an empty body means the
    // profile's figure.
    let r = t
        .post(
            &sandbox_path(pid),
            Some(tok),
            &json!({"opening_balance_minor": 25_000_000}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.body["opening_balance_source"], "SET_UP_REQUEST");
    assert!(r.body["opening_balance_note"]
        .as_str()
        .unwrap()
        .contains("the figure the owner entered"));
    // The plan and affordability without a sandbox or scenario say why.
    let r = t.get(&prop_path(pid, prop, "plan"), Some(tok)).await;
    labelled(&r, FIXTURE);
});

both_backends!(a_project_without_a_sandbox_or_scenario_is_told_why, |db| {
    let t = app_with_fixture(db);
    let d = demo(&t).await;
    let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.alhambra.as_str());
    let r = t
        .get(&prop_path(pid, prop, "affordability"), Some(tok))
        .await;
    assert_problem(&r, 409, "NESSIE_SANDBOX_REQUIRED");
    let r = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    assert_problem(&r, 409, "NESSIE_SANDBOX_REQUIRED");
    let r = t
        .call(
            axum::http::Method::POST,
            &pay_path(pid, prop, ITEM_DEPOSIT, "refresh"),
            Some(tok),
            None,
        )
        .await;
    assert_problem(&r, 404, "NESSIE_NO_ATTEMPT");
    // A project whose search found nothing has no property with a scenario.
    let (tok2, _) = (tok, ());
    let p2 = t.create_project(tok2, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t.post(&sandbox_path(&p2), Some(tok2), &json!({})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
});

// ---- The fixture bank forgets everything on restart --------------------------------------------------

both_backends!(a_reset_fixture_bank_means_the_sandbox_is_gone, |db| {
    let t = app_with_fixture(db);
    let d = demo(&t).await;
    let (tok, pid) = (d.tok.as_str(), d.pid.as_str());
    let first = t.post(&sandbox_path(pid), Some(tok), &json!({})).await;
    assert_eq!(first.status, StatusCode::CREATED);
    // A restart: a new fixture bank, whose ids collide with the old ones.
    install(&t.app, NessieState::fixture());
    let r = t.get(&sandbox_path(pid), Some(tok)).await;
    assert_problem(&r, 404, "NESSIE_SANDBOX_GONE");
    assert!(r.body["message"]
        .as_str()
        .unwrap()
        .contains("fixture bank was reset"));
    let r = request_approval(&t, &d, &d.hollywood, ITEM_DEPOSIT).await;
    assert_problem(&r, 409, "NESSIE_SANDBOX_GONE");
    // Set-up replaces the row and keeps what it replaced.
    let r = t.post(&sandbox_path(pid), Some(tok), &json!({})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let recs = records(&t, pid).await;
    assert_eq!(recs.len(), 1);
    let sb = recs[0].sandbox.as_ref().unwrap();
    assert_eq!(sb.replaced.len(), 1);
    assert!(sb.replaced[0].reason.contains("reset"));
    assert_eq!(
        t.get(&sandbox_path(pid), Some(tok)).await.status,
        StatusCode::OK
    );
});

// ---- A stub that claims to be the Capital One sandbox ------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Normal,
    Refuse,
    ServerError,
    WrongAmount,
}

#[derive(Clone)]
struct Stub {
    mode: Arc<Mutex<Mode>>,
    /// Delay before answering a POST (ms).
    delay_ms: Arc<AtomicU64>,
    /// Transactions read back as completed.
    complete: Arc<AtomicBool>,
    /// nessie-3: set to true when a purchase POST arrives, before it is answered (the database
    /// "goes down" between the bank's answer and the outcome recording).
    down_on_pay: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

#[derive(Default)]
struct StubState {
    seq: u64,
    accounts: HashMap<String, Value>,
    bills: HashMap<String, Value>,
    txns: HashMap<String, Value>,
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

/// A loopback server answering the Nessie v2 calls the server makes, with `mode` for purchases.
fn stub() -> (MockServer, Stub) {
    let ctl = Stub {
        mode: Arc::new(Mutex::new(Mode::Normal)),
        delay_ms: Arc::new(AtomicU64::new(0)),
        complete: Arc::new(AtomicBool::new(false)),
        down_on_pay: Arc::new(Mutex::new(None)),
    };
    let c = ctl.clone();
    let state = Mutex::new(StubState::default());
    let srv = MockServer::start_with(move |r: &Recorded| {
        let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        let parts: Vec<&str> = r.path.trim_start_matches('/').split('/').collect();
        if r.method == "POST" && r.path.ends_with("/purchases") {
            if let Some(down) = c.down_on_pay.lock().unwrap().as_ref() {
                down.store(true, Ordering::SeqCst);
            }
        }
        if r.method == "POST" {
            std::thread::sleep(Duration::from_millis(c.delay_ms.load(Ordering::SeqCst)));
        }
        let mut st = state.lock().unwrap();
        st.seq += 1;
        let n = st.seq;
        let ts = "2026-11-14T12:00:00Z";
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
                let mode = *c.mode.lock().unwrap();
                match mode {
                    Mode::Refuse => problem(422, "insufficient_funds"),
                    Mode::ServerError => problem(503, "unavailable"),
                    _ => {
                        let amount = body["amount_cents"].as_i64().unwrap_or(0)
                            + i64::from(mode == Mode::WrongAmount);
                        let t = json!({"id": format!("txn_{n}"), "kind": "purchase", "status": "pending", "medium": "balance", "amount_cents": amount, "payer_id": id, "merchant_id": body["merchant_id"], "description": body["description"], "settle_at": ts, "created_at": ts, "updated_at": ts});
                        st.txns.insert(format!("txn_{n}"), t.clone());
                        reply(200, t)
                    }
                }
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
                let t = json!({"id": format!("txn_{n}"), "kind": "bill_payment", "status": "pending", "medium": "balance", "amount_cents": body["amount_cents"], "payer_id": account, "bill_id": bid, "settle_at": ts, "created_at": ts, "updated_at": ts});
                st.txns.insert(format!("txn_{n}"), t.clone());
                reply(200, t)
            }
            ("GET", ["transactions", id]) => match st.txns.get(*id) {
                Some(t) => {
                    let mut t = t.clone();
                    if c.complete.load(Ordering::SeqCst) {
                        t["status"] = json!("completed");
                    }
                    reply(200, t)
                }
                None => problem(404, "transaction-not-found"),
            },
            // The account's transactions, newest first (refresh of a MAY_EXIST purchase).
            ("GET", ["accounts", id, "transactions"]) => {
                let mut items: Vec<(u64, Value)> = st
                    .txns
                    .iter()
                    .filter(|(_, t)| t["payer_id"] == json!(id) || t["payee_id"] == json!(id))
                    .map(|(k, t)| {
                        let n = k.trim_start_matches("txn_").parse::<u64>().unwrap_or(0);
                        (n, t.clone())
                    })
                    .collect();
                items.sort_by(|a, b| b.0.cmp(&a.0));
                reply(
                    200,
                    json!({"items": items.into_iter().map(|(_, t)| t).collect::<Vec<_>>()}),
                )
            }
            _ => problem(404, "not-found"),
        }
    });
    (srv, ctl)
}

/// A test app whose bank is the live client pointed at the stub (it claims to be the sandbox).
fn app_with_stub(db: &TestDb, srv: &MockServer, timeout_s: f64) -> TestApp {
    let t = TestApp::new(db, &[]);
    let bank = LiveNessieBank::with_policy(
        &srv.base,
        ApiKey::new("nsk_TEST_server_stub_5d4c3b2a1f0e").unwrap(),
        RetryPolicy {
            attempts: 1,
            backoff_s: 0.0,
            timeout_s,
        },
    );
    install(
        &t.app,
        NessieState::with_bank(Arc::new(bank), format!("sandbox:{}", srv.base)),
    );
    t
}

fn count(srv: &MockServer, method: &str, suffix: &str) -> usize {
    srv.requests()
        .iter()
        .filter(|r| r.method == method && r.path.ends_with(suffix))
        .count()
}

both_backends!(every_answer_is_labelled_for_both_banks, |db| {
    let (srv, _ctl) = stub();
    for (t, sim) in [
        (app_with_fixture(db), FIXTURE),
        (app_with_stub(db, &srv, 5.0), SANDBOX),
    ] {
        let d = demo(&t).await;
        let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
        let mut answers = vec![t.post(&sandbox_path(pid), Some(tok), &json!({})).await];
        answers.push(t.post(&sandbox_path(pid), Some(tok), &json!({})).await);
        answers.push(t.get(&sandbox_path(pid), Some(tok)).await);
        answers.push(t.get(&prop_path(pid, prop, "plan"), Some(tok)).await);
        answers.push(
            t.get(&prop_path(pid, prop, "affordability"), Some(tok))
                .await,
        );
        answers.push(request_approval(&t, &d, prop, ITEM_RENOVATION).await);
        answers.push(approve_and_execute(&t, &d, prop, ITEM_RENOVATION).await);
        answers.push(
            t.call(
                axum::http::Method::POST,
                &pay_path(pid, prop, ITEM_RENOVATION, "refresh"),
                Some(tok),
                None,
            )
            .await,
        );
        for r in &answers {
            labelled(r, sim);
        }
        // The crate's own outputs inside carry the same label.
        assert_eq!(answers[3].body["plan"]["simulation"], sim);
        assert_eq!(answers[4].body["report"]["simulation"], sim);
        assert_eq!(answers[6].body["record"]["simulation"], sim);
        assert_eq!(
            answers[5].body["approval"]["action_payload"]["simulation"],
            sim
        );
        if sim == SANDBOX {
            assert!(answers[0].body["account_id"]
                .as_str()
                .unwrap()
                .starts_with("acct_"));
        }
    }
});

both_backends!(bank_failures_are_recorded_honestly, |db| {
    let (srv, ctl) = stub();
    let t = app_with_stub(db, &srv, 0.6);
    let d = demo(&t).await;
    let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
    assert_eq!(
        t.post(&sandbox_path(pid), Some(tok), &json!({}))
            .await
            .status,
        StatusCode::CREATED
    );
    let purchases = |s: &MockServer| count(s, "POST", "/purchases");

    // Refused by the bank: 422 with its slug, recorded REFUSED, the approval not spent.
    *ctl.mode.lock().unwrap() = Mode::Refuse;
    let a = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    decide(&t, tok, &a.body["approval"], "APPROVED").await;
    let id = a.body["approval"]["approval_id"].clone();
    let r = execute(&t, &d, prop, ITEM_DEPOSIT, &id).await;
    labelled_problem(&r, 422, "NESSIE_PAYMENT_REFUSED", SANDBOX);
    assert_eq!(r.body["details"]["problem_slug"], "insufficient_funds");
    assert_eq!(r.body["details"]["approval_status"], "APPROVED");
    assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Approved);
    assert_eq!(
        attempt(&t, pid, prop, ITEM_DEPOSIT)
            .await
            .unwrap()
            .attempt_status,
        "REFUSED"
    );
    assert_eq!(purchases(&srv), 1);
    // Fixed: the same approval executes, and the refusal stays in the record's history.
    *ctl.mode.lock().unwrap() = Mode::Normal;
    let r = execute(&t, &d, prop, ITEM_DEPOSIT, &id).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let stored = attempt(&t, pid, prop, ITEM_DEPOSIT).await.unwrap();
    assert_eq!(stored.attempt_status, "SENT");
    assert_eq!(stored.earlier_refusals.len(), 1);
    assert_eq!(
        stored.earlier_refusals[0].failure.problem_slug.as_deref(),
        Some("insufficient_funds")
    );
    assert_eq!(purchases(&srv), 2);
    // Refresh reports completion only because the bank said so.
    ctl.complete.store(true, Ordering::SeqCst);
    let r = t
        .call(
            axum::http::Method::POST,
            &pay_path(pid, prop, ITEM_DEPOSIT, "refresh"),
            Some(tok),
            None,
        )
        .await;
    labelled(&r, SANDBOX);
    assert_eq!(r.body["record"]["outcome"], "SIMULATED_COMPLETED");
    assert_eq!(
        attempt(&t, pid, prop, ITEM_DEPOSIT)
            .await
            .unwrap()
            .outcome
            .as_deref(),
        Some("SIMULATED_COMPLETED")
    );

    // Timeout, 5xx, and an answer with another amount: 502, MAY_EXIST, the approval spent, and
    // a second execute never reaches the bank.
    for (item, mode, delay) in [
        (ITEM_RENOVATION, Mode::Normal, 1500u64),
        (ITEM_FURNISHING, Mode::ServerError, 0),
    ] {
        *ctl.mode.lock().unwrap() = mode;
        ctl.delay_ms.store(delay, Ordering::SeqCst);
        let a = request_approval(&t, &d, prop, item).await;
        assert!(a.status.is_success(), "{item}: {}", a.text);
        decide(&t, tok, &a.body["approval"], "APPROVED").await;
        let id = a.body["approval"]["approval_id"].clone();
        let before = purchases(&srv);
        let r = execute(&t, &d, prop, item, &id).await;
        labelled_problem(&r, 502, "NESSIE_PAYMENT_MAY_EXIST", SANDBOX);
        assert_eq!(r.body["retryable"], false);
        assert!(r.body["message"]
            .as_str()
            .unwrap()
            .contains("Check the sandbox account's transactions"));
        assert_eq!(r.body["details"]["approval_status"], "EXECUTED");
        assert_eq!(
            approval_status(&t, &id).await,
            ApprovalStatus::Executed,
            "{item}"
        );
        let stored = attempt(&t, pid, prop, item).await.unwrap();
        assert_eq!(stored.attempt_status, "MAY_EXIST", "{item}");
        ctl.delay_ms.store(0, Ordering::SeqCst);
        // Let the stalled stub answer before counting.
        tokio::time::sleep(Duration::from_millis(delay)).await;
        assert_eq!(purchases(&srv), before + 1, "{item}");
        let r = execute(&t, &d, prop, item, &id).await;
        // nessie-3 F7: the refusal carries the label; an unknown outcome tells the owner to
        // check the account.
        labelled_problem(&r, 409, "NESSIE_ALREADY_ATTEMPTED", SANDBOX);
        assert_eq!(r.body["details"]["attempt_status"], "MAY_EXIST");
        assert!(r.body["message"]
            .as_str()
            .unwrap()
            .contains("may have been made"));
        let r = t
            .call(
                axum::http::Method::POST,
                &pay_path(pid, prop, item, "refresh"),
                Some(tok),
                None,
            )
            .await;
        if mode == Mode::Normal {
            // nessie-3: the timed-out purchase did reach the stub, and its description names the
            // attempt, so refresh finds it among the account's transactions and settles it.
            labelled(&r, SANDBOX);
            assert_eq!(r.body["attempt_status"], "SENT", "{}", r.text);
            assert_eq!(r.body["record"]["outcome"], "SIMULATED_PENDING");
            let stored = attempt(&t, pid, prop, item).await.unwrap();
            assert_eq!(stored.attempt_status, "SENT");
            assert_eq!(
                attempt_json(&t, pid, prop, item).await["outcome_source"],
                svc::SOURCE_REFRESH_FOUND
            );
            assert_eq!(
                stored.nessie_transaction_id,
                r.body["record"]["nessie_transaction_id"]
                    .as_str()
                    .map(str::to_owned)
            );
            assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Executed);
        } else {
            // A 503: nothing was created, and the bank cannot tell more; it stays MAY_EXIST.
            assert_problem(&r, 409, "NESSIE_NOTHING_TO_REFRESH");
            assert_eq!(r.body["details"]["found"], 0, "{}", r.text);
            assert_eq!(
                attempt(&t, pid, prop, item).await.unwrap().attempt_status,
                "MAY_EXIST"
            );
        }
        assert_eq!(purchases(&srv), before + 1, "{item}: never sent twice");
    }

    // An answer about another amount is MAY_EXIST too (a fresh property keeps the items apart).
    *ctl.mode.lock().unwrap() = Mode::WrongAmount;
    let other = d.alhambra.as_str();
    let a = request_approval(&t, &d, other, ITEM_DEPOSIT).await;
    decide(&t, tok, &a.body["approval"], "APPROVED").await;
    let r = execute(
        &t,
        &d,
        other,
        ITEM_DEPOSIT,
        &a.body["approval"]["approval_id"],
    )
    .await;
    labelled_problem(&r, 502, "NESSIE_PAYMENT_MAY_EXIST", SANDBOX);
    assert!(r.body["message"].as_str().unwrap().contains("amount_cents"));
    // The stub never saw the key anywhere but the header.
    for rq in srv.requests() {
        assert_eq!(
            rq.headers.get("authorization").map(String::as_str),
            Some("Bearer nsk_TEST_server_stub_5d4c3b2a1f0e")
        );
        assert!(!rq.path.contains("nsk_"));
        assert!(!String::from_utf8_lossy(&rq.body).contains("nsk_"));
    }
});

/// Review fix F1 (nessie-2 review): the client closes the connection during the bank call. The
/// execute still commits its outcome, so a later execute of the same approval, also after a
/// restart (an empty in-process ledger), never reaches the bank again.
#[tokio::test]
async fn a_closed_connection_during_the_bank_call_still_records_the_payment() {
    use tokio::io::AsyncWriteExt;
    let db = TestDb::new(Kind::Memory).await.expect("memory");
    let (srv, ctl) = stub();
    let t = app_with_stub(&db, &srv, 5.0);
    let d = demo(&t).await;
    let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
    t.post(&sandbox_path(pid), Some(tok), &json!({})).await;
    let a = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    decide(&t, tok, &a.body["approval"], "APPROVED").await;
    let id = a.body["approval"]["approval_id"].clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = t.router.clone();
    tokio::spawn(async move { axum::serve(listener, router).await });
    ctl.delay_ms.store(1000, Ordering::SeqCst);
    let body = json!({"approval_id": id}).to_string();
    let path = pay_path(pid, prop, ITEM_DEPOSIT, "execute");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {tok}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len());
    s.write_all(format!("{head}{body}").as_bytes())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(s);
    tokio::time::sleep(Duration::from_millis(2000)).await;
    ctl.delay_ms.store(0, Ordering::SeqCst);
    assert_eq!(count(&srv, "POST", "/purchases"), 1);
    assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Executed);
    let stored = attempt(&t, pid, prop, ITEM_DEPOSIT).await.unwrap();
    assert_eq!(stored.attempt_status, "SENT");
    // A restart: the same stub behind a fresh client, so the crate's ledger is empty.
    let key = ApiKey::new("nsk_TEST_server_stub_5d4c3b2a1f0e").unwrap();
    let policy = RetryPolicy {
        attempts: 1,
        backoff_s: 0.0,
        timeout_s: 5.0,
    };
    let bank = LiveNessieBank::with_policy(&srv.base, key, policy);
    let bank_ref = format!("sandbox:{}", srv.base);
    install(&t.app, NessieState::with_bank(Arc::new(bank), bank_ref));
    let r = execute(&t, &d, prop, ITEM_DEPOSIT, &id).await;
    assert_problem(&r, 409, "NESSIE_ALREADY_ATTEMPTED");
    assert_eq!(count(&srv, "POST", "/purchases"), 1);
    db.finish().await;
}

both_backends!(the_ready_entry_follows_the_bank, |db| {
    let (srv, _ctl) = stub();
    let t = app_with_fixture(db);
    let r = t.get("/ready", None).await;
    let entry = r.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "nessie")
        .unwrap()
        .clone();
    assert_eq!(entry["status"], "MOCK");
    assert!(entry["note"].as_str().unwrap().starts_with("FIXTURE"));
    assert_conforms("ReadyResponse", &r.body);
    let t = app_with_stub(db, &srv, 5.0);
    let entry = |r: &Resp| {
        r.body["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["provider"] == "nessie")
            .unwrap()
            .clone()
    };
    let r = t.get("/ready", None).await;
    assert_eq!(
        entry(&r)["status"],
        "PARTIAL",
        "configured, not yet verified"
    );
    let d = demo(&t).await;
    t.post(&sandbox_path(&d.pid), Some(&d.tok), &json!({}))
        .await;
    let r = t.get("/ready", None).await;
    assert_eq!(
        entry(&r)["status"],
        "LIVE",
        "verified by an authenticated call"
    );
    assert!(!r.text.contains("nsk_"));
    // Invalid settings: BLOCKED and every route 503.
    install(
        &t.app,
        NessieState::misconfigured(
            "NESSIE_PROVIDER_MODE must be live, fixture or disabled, not \"sandbox\"",
        ),
    );
    let r = t.get("/ready", None).await;
    assert_eq!(entry(&r)["status"], "BLOCKED");
    let r = t
        .get(&prop_path(&d.pid, &d.hollywood, "plan"), Some(&d.tok))
        .await;
    assert_problem(&r, 503, "NESSIE_MISCONFIGURED");
});

// ---- G1, lock keys, attempt ids -----------------------------------------------------------------------

both_backends!(approval_evidence_comes_only_from_require_approved, |db| {
    let t = app_with_fixture(db);
    let d = demo(&t).await;
    let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
    t.post(&sandbox_path(pid), Some(tok), &json!({})).await;
    let a = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    decide(&t, tok, &a.body["approval"], "APPROVED").await;
    let approval: ss_server::contracts::ApprovalRequest =
        serde_json::from_value(a.body["approval"].clone()).unwrap();
    let mut tx = t.app.store.begin().await.unwrap();
    let approved = t
        .app
        .approvals()
        .require_approved(
            &mut *tx,
            approval.project_id,
            Some(approval.approval_id),
            ActionType::FinancialCommitment,
            &approval.action_payload,
        )
        .await
        .unwrap();
    let ev = approval_evidence(&approved);
    let stored = APPROVALS
        .require(&mut *tx, approval.approval_id)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(ev.approval_id, stored.approval_id);
    assert_eq!(ev.project_id, stored.project_id);
    assert_eq!(ev.action_type, "FINANCIAL_COMMITMENT");
    assert_eq!(ev.approval_status, "APPROVED");
    assert_eq!(ev.action_payload, stored.action_payload);
    assert_eq!(ev.expires_at, stored.expires_at.as_datetime());
    // The evidence authorizes exactly the payment the plan rebuilds, and nothing else.
    let attempt_id = stored.action_payload["attempt_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let plan = t.get(&prop_path(pid, prop, "plan"), Some(tok)).await;
    assert_eq!(
        plan.body["plan"]["scenario_id"],
        stored.action_payload["scenario_id"]
    );
    let other = ss_nessie::PaymentIntent {
        simulation: ss_nessie::Simulation::FixtureBank,
        project_id: approval.project_id,
        property_id: prop.parse().unwrap(),
        scenario_id: stored.action_payload["scenario_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        plan_item_id: ITEM_DEPOSIT.into(),
        attempt_id: attempt_id.clone(),
        action: ss_nessie::ApprovalAction::FinancialCommitment,
        operation: ss_nessie::approval::PaymentOperation::Purchase {
            account_id: stored.action_payload["account_id"].as_str().unwrap().into(),
            merchant_id: stored.action_payload["merchant_id"]
                .as_str()
                .unwrap()
                .into(),
        },
        amount_minor: 1,
        currency: "USD".into(),
        description: stored.action_payload["description"]
            .as_str()
            .unwrap()
            .into(),
    };
    assert_eq!(
        PaymentAuthorization::verify(&ev, other, chrono::Utc::now()).unwrap_err(),
        ss_nessie::AuthorizationError::PayloadMismatch
    );
});

#[test]
fn lock_keys_and_attempt_ids_match_their_python_formulas() {
    let pid: Uuid = "00000000-0000-4000-8000-000000000001".parse().unwrap();
    let prop: Uuid = "00000000-0000-4000-8000-000000000002".parse().unwrap();
    // int.from_bytes(hashlib.sha256(f"second-site:nessie:{pid}:{prop}:{item}".encode()).digest()[:8], "big", signed=True)
    assert_eq!(
        plan_item_lock_key(pid, prop, ITEM_DEPOSIT),
        8017684891582393351
    );
    // the same with f"second-site:nessie_sandbox:{pid}"
    assert_eq!(sandbox_lock_key(pid), 2212894234219038640);
    // hashlib.sha256(b"second-site:nessie-attempt:abc:0").hexdigest()[:12]
    assert_eq!(attempt_id_for("abc", 0), "cada3f508752");
    assert_eq!(attempt_id_for("abc", 1), "69e4409f7fde");
    assert_ne!(
        plan_item_lock_key(pid, prop, ITEM_DEPOSIT),
        plan_item_lock_key(pid, prop, ITEM_RENOVATION)
    );
}

// ---- PostgreSQL only -----------------------------------------------------------------------------------

#[tokio::test]
async fn postgres_two_concurrent_executes_of_one_approval_reach_the_bank_once() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let (srv, ctl) = stub();
    let t = app_with_stub(&db, &srv, 5.0);
    let d = demo(&t).await;
    let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
    t.post(&sandbox_path(pid), Some(tok), &json!({})).await;
    let a = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
    decide(&t, tok, &a.body["approval"], "APPROVED").await;
    let id = a.body["approval"]["approval_id"].clone();
    // The bank answers slowly, so the second request arrives while the first holds the locks.
    ctl.delay_ms.store(400, Ordering::SeqCst);
    let (r1, r2) = tokio::join!(
        execute(&t, &d, prop, ITEM_DEPOSIT, &id),
        execute(&t, &d, prop, ITEM_DEPOSIT, &id)
    );
    let mut statuses = vec![r1.status.as_u16(), r2.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, vec![200, 409], "{} / {}", r1.text, r2.text);
    let loser = if r1.status == StatusCode::OK {
        &r2
    } else {
        &r1
    };
    assert_eq!(
        loser.body["error_code"], "NESSIE_ALREADY_ATTEMPTED",
        "{}",
        loser.text
    );
    assert_eq!(count(&srv, "POST", "/purchases"), 1);
    assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Executed);
    db.finish().await;
}

#[tokio::test]
async fn postgres_two_concurrent_set_ups_create_one_sandbox() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let (srv, ctl) = stub();
    let t = app_with_stub(&db, &srv, 5.0);
    let d = demo(&t).await;
    ctl.delay_ms.store(300, Ordering::SeqCst);
    let (path, body) = (sandbox_path(&d.pid), json!({}));
    let (r1, r2) = tokio::join!(
        t.post(&path, Some(&d.tok), &body),
        t.post(&path, Some(&d.tok), &body)
    );
    let mut statuses = vec![r1.status.as_u16(), r2.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, vec![200, 201], "{} / {}", r1.text, r2.text);
    assert_eq!(r1.body["account_id"], r2.body["account_id"]);
    assert_eq!(count(&srv, "POST", "/customers"), 1);
    assert_eq!(records(&t, &d.pid).await.len(), 1);
    db.finish().await;
}

/// The Rust-owned table on a schema the Rust runner migrated (twice) and on one alembic
/// migrated: created, matching its metadata, `alembic_version` untouched.
#[tokio::test]
async fn postgres_the_rust_owned_table_is_created_outside_alembic() {
    let Some(url) = pg_url() else {
        eprintln!("SKIPPED: TEST_DATABASE_URL is not set");
        return;
    };
    use sqlx::ConnectOptions;
    use std::str::FromStr;
    let check = |pool: sqlx::PgPool| async move {
        let version: String = sqlx::query_scalar("SELECT version_num FROM alembic_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, "0002");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM alembic_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let t = &schema::NESSIE_SIMULATION_RECORDS;
        let cols: Vec<(String, String, String, Option<i32>)> = sqlx::query_as(
            "SELECT column_name::text, data_type::text, is_nullable::text, character_maximum_length::int \
             FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = $1 ORDER BY ordinal_position",
        )
        .bind(t.name)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(cols.len(), t.columns.len());
        for (c, (name, ty, nullable, len)) in t.columns.iter().zip(&cols) {
            assert_eq!(c.name, name);
            assert_eq!(c.ty.pg_data_type(), ty, "{name}");
            assert_eq!(c.nullable, nullable == "YES", "{name}");
            if let ss_server::store::ColType::Varchar(n) = c.ty {
                assert_eq!(Some(n as i32), *len, "{name}");
            }
        }
        let constraints: Vec<String> = sqlx::query_scalar(
            "SELECT conname::text FROM pg_constraint WHERE conrelid = to_regclass($1) ORDER BY 1",
        )
        .bind(t.name)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            constraints,
            [
                "fk_nessie_simulation_records_project_id_projects",
                "pk_nessie_simulation_records",
                "uq_nessie_simulation_records_project_id"
            ]
        );
        let idx: Vec<String> = sqlx::query_scalar(
            "SELECT indexname::text FROM pg_indexes WHERE schemaname = current_schema() AND tablename = $1 ORDER BY 1",
        )
        .bind(t.name)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            idx.contains(&"ix_nessie_simulation_records_approval_id".to_owned()),
            "{idx:?}"
        );
    };

    // The Rust runner, twice.
    let (store, schema_name) = pg_store().await;
    assert_eq!(
        store.migrate().await.unwrap(),
        ss_server::store::postgres::MigrationOutcome::UpToDate
    );
    check(store.pool().clone()).await;
    drop(store);
    drop_schema(&schema_name).await;

    // A schema alembic migrated (its own DDL plus alembic_version), then the Rust runner twice.
    let base = sqlx::postgres::PgConnectOptions::from_str(
        &ss_server::config::normalize_database_url(&url),
    )
    .unwrap();
    let schema_name = format!("rt_{}", Uuid::new_v4().simple());
    let mut admin = base.clone().connect().await.unwrap();
    sqlx::Executor::execute(&mut admin, format!("CREATE SCHEMA {schema_name}").as_str())
        .await
        .unwrap();
    let opts = base.options([("search_path", schema_name.as_str())]);
    let mut conn = opts.clone().connect().await.unwrap();
    let mut alembic = String::from("BEGIN;\nCREATE TABLE alembic_version (\n    version_num VARCHAR(32) NOT NULL, \n    CONSTRAINT alembic_version_pkc PRIMARY KEY (version_num)\n);\n");
    alembic.push_str(include_str!("../migrations/0001_foundation_schema.sql"));
    alembic.push_str("\nINSERT INTO alembic_version (version_num) VALUES ('0001');\n");
    alembic.push_str(include_str!("../migrations/0002_owner_channels.sql"));
    alembic.push_str("\nUPDATE alembic_version SET version_num='0002' WHERE alembic_version.version_num = '0001';\nCOMMIT;\n");
    sqlx::Executor::execute(&mut conn, alembic.as_str())
        .await
        .unwrap();
    let exists: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('nessie_simulation_records')::text")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(exists, None, "alembic does not create it");
    let store = ss_server::store::postgres::PgStore::connect_with(opts.clone(), 2)
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            store.migrate().await.unwrap(),
            ss_server::store::postgres::MigrationOutcome::UpToDate
        );
    }
    check(store.pool().clone()).await;
    // A newer alembic revision this binary does not know: the alembic tables are left alone, the
    // Rust-owned table is still there (created idempotently).
    sqlx::Executor::execute(
        &mut conn,
        "DROP TABLE nessie_simulation_records; UPDATE alembic_version SET version_num = '0099'",
    )
    .await
    .unwrap();
    assert_eq!(
        store.migrate().await.unwrap(),
        ss_server::store::postgres::MigrationOutcome::UnknownRevision("0099".into())
    );
    let version: String = sqlx::query_scalar("SELECT version_num FROM alembic_version")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(version, "0099");
    let exists: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('nessie_simulation_records')::text")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert!(exists.is_some());
    drop(store);
    drop(conn);
    drop_schema(&schema_name).await;
}

both_backends!(
    the_record_table_enforces_one_row_per_key_and_project,
    |db| {
        let t = app_with_fixture(db);
        let d = demo(&t).await;
        t.post(&sandbox_path(&d.pid), Some(&d.tok), &json!({}))
            .await;
        let mut recs = records(&t, &d.pid).await;
        let mut dup = recs.remove(0);
        dup.record_id = Uuid::new_v4();
        let mut tx = t.app.store.begin().await.unwrap();
        let e = NESSIE_RECORDS.add(&mut *tx, &dup).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::CONFLICT, "{e:?}");
        drop(tx);
        // A row for a project that does not exist is refused by the foreign key.
        let mut tx = t.app.store.begin().await.unwrap();
        dup.project_id = Uuid::new_v4();
        assert!(NESSIE_RECORDS.add(&mut *tx, &dup).await.is_err());
        drop(tx);
        let mut tx = t.app.store.begin().await.unwrap();
        let rows = tx
            .select(&schema::NESSIE_SIMULATION_RECORDS, &Query::new())
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(rows.len(), 1);
    }
);

// ---- nessie-3: a committed claim before the bank call (F2), nothing held during it, settings -------

/// A store whose every operation fails while `down` is set: the database going away between the
/// bank's answer and the recording of the outcome (as a crash there would leave it).
struct FlakyStore {
    inner: Arc<dyn Store>,
    down: Arc<AtomicBool>,
}

struct FlakyTx {
    inner: Box<dyn Tx>,
    down: Arc<AtomicBool>,
}

fn injected() -> StoreError {
    StoreError::Database("injected by the test: the database is down".into())
}

impl FlakyTx {
    fn check(&self) -> StoreResult<()> {
        if self.down.load(Ordering::SeqCst) {
            Err(injected())
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl Tx for FlakyTx {
    fn backend(&self) -> Backend {
        self.inner.backend()
    }
    async fn insert(&mut self, table: &'static TableDef, row: Row) -> StoreResult<Row> {
        self.check()?;
        self.inner.insert(table, row).await
    }
    async fn get(&mut self, table: &'static TableDef, id: Uuid) -> StoreResult<Option<Row>> {
        self.check()?;
        self.inner.get(table, id).await
    }
    async fn get_for_update(
        &mut self,
        table: &'static TableDef,
        id: Uuid,
    ) -> StoreResult<Option<Row>> {
        self.check()?;
        self.inner.get_for_update(table, id).await
    }
    async fn select(&mut self, table: &'static TableDef, query: &Query) -> StoreResult<Vec<Row>> {
        self.check()?;
        self.inner.select(table, query).await
    }
    async fn update(
        &mut self,
        table: &'static TableDef,
        id: Uuid,
        set: Row,
        expected_version: Option<i64>,
    ) -> StoreResult<UpdateOutcome> {
        self.check()?;
        self.inner.update(table, id, set, expected_version).await
    }
    async fn update_where(
        &mut self,
        table: &'static TableDef,
        filters: &[Filter],
        set: Row,
    ) -> StoreResult<u64> {
        self.check()?;
        self.inner.update_where(table, filters, set).await
    }
    async fn delete_where(
        &mut self,
        table: &'static TableDef,
        filters: &[Filter],
    ) -> StoreResult<u64> {
        self.check()?;
        self.inner.delete_where(table, filters).await
    }
    async fn advisory_xact_lock(&mut self, key: i64) -> StoreResult<()> {
        self.check()?;
        self.inner.advisory_xact_lock(key).await
    }
    async fn savepoint(&mut self) -> StoreResult<Savepoint> {
        self.check()?;
        self.inner.savepoint().await
    }
    async fn release_savepoint(&mut self, sp: Savepoint) -> StoreResult<()> {
        self.check()?;
        self.inner.release_savepoint(sp).await
    }
    async fn rollback_to_savepoint(&mut self, sp: Savepoint) -> StoreResult<()> {
        self.check()?;
        self.inner.rollback_to_savepoint(sp).await
    }
    async fn commit(self: Box<Self>) -> StoreResult<()> {
        self.check()?;
        self.inner.commit().await
    }
    async fn rollback(self: Box<Self>) -> StoreResult<()> {
        self.inner.rollback().await
    }
}

#[async_trait::async_trait]
impl Store for FlakyStore {
    fn backend(&self) -> Backend {
        self.inner.backend()
    }
    async fn begin(&self) -> StoreResult<Box<dyn Tx>> {
        if self.down.load(Ordering::SeqCst) {
            return Err(injected());
        }
        Ok(Box::new(FlakyTx {
            inner: self.inner.begin().await?,
            down: self.down.clone(),
        }))
    }
    async fn ping(&self) -> StoreResult<()> {
        self.inner.ping().await
    }
}

const STUB_KEY: &str = "nsk_TEST_server_stub_5d4c3b2a1f0e";

/// The live client pointed at the stub, as one server instance, with these claim times.
fn stub_state(srv: &MockServer, timeout_s: f64, timing: svc::ClaimTiming) -> NessieState {
    let bank = LiveNessieBank::with_policy(
        &srv.base,
        ApiKey::new(STUB_KEY).unwrap(),
        RetryPolicy {
            attempts: 1,
            backoff_s: 0.0,
            timeout_s,
        },
    );
    NessieState::with_bank(Arc::new(bank), format!("sandbox:{}", srv.base))
        .with_claim_timing(timing)
}

/// Claim times short enough for a test: another instance's claim is dead
/// `send_window + call_deadline + margin` after it was stored.
fn short_timing(send_ms: u64, deadline_ms: u64, margin_ms: u64) -> svc::ClaimTiming {
    svc::ClaimTiming {
        send_window: Duration::from_millis(send_ms),
        call_deadline: Duration::from_millis(deadline_ms),
        margin: Duration::from_millis(margin_ms),
    }
}

/// A server (another process when two share a store) on `store`, with `state` as its bank.
fn app_on(store: Arc<dyn Store>, kind: Kind, state: NessieState) -> TestApp {
    let app = ss_server::build_app(test_settings(&[]), store);
    let router = ss_server::http::router(app.clone());
    install(&app, state);
    TestApp { app, router, kind }
}

fn fork(t: &TestApp) -> TestApp {
    TestApp {
        app: t.app.clone(),
        router: t.router.clone(),
        kind: t.kind,
    }
}

/// A second APPROVED approval for the same plan item and plan: the approved payload with another
/// attempt id (so another payload), created through the approval service and decided by the owner.
async fn second_approval(t: &TestApp, tok: &str, first: &Value) -> Value {
    let mut p: JsonMap = serde_json::from_value(first["action_payload"].clone()).unwrap();
    let old = p["attempt_id"].as_str().unwrap().to_owned();
    let other = "abcdef012345";
    p.insert("attempt_id".into(), json!(other));
    let desc = p["description"].as_str().unwrap().replace(&old, other);
    p.insert("description".into(), json!(desc));
    let action: ActionType = serde_json::from_value(first["action_type"].clone()).unwrap();
    let mut tx = t.app.store.begin().await.unwrap();
    let spec = ApprovalSpec::new(
        first["project_id"].as_str().unwrap().parse().unwrap(),
        first["authorized_owner_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        "nessie3-test",
        action,
        p,
        "a second approval for the same plan item",
        format!("nessie3:{}", Uuid::new_v4()),
    );
    let a = t.app.approvals().request(&mut *tx, spec).await.unwrap();
    tx.commit().await.unwrap();
    let v = serde_json::to_value(&a).unwrap();
    decide(t, tok, &v, "APPROVED").await;
    v
}

/// 409 `NESSIE_ALREADY_ATTEMPTED` for an attempt in this state, with the label in its details.
fn already(r: &Resp, attempt_status: &str) {
    labelled_problem(r, 409, "NESSIE_ALREADY_ATTEMPTED", SANDBOX);
    assert_eq!(
        r.body["details"]["attempt_status"], attempt_status,
        "{}",
        r.text
    );
}

async fn refresh(t: &TestApp, d: &Demo, prop: &str, item: &str) -> Resp {
    t.call(
        axum::http::Method::POST,
        &pay_path(&d.pid, prop, item, "refresh"),
        Some(&d.tok),
        None,
    )
    .await
}

/// A stored attempt as JSON (fields added by nessie-3 are read by name).
async fn attempt_json(t: &TestApp, pid: &str, prop: &str, item: &str) -> Value {
    serde_json::to_value(attempt(t, pid, prop, item).await.unwrap()).unwrap()
}

async fn wait_past(t: &TestApp, pid: &str, prop: &str, item: &str) {
    let claim = attempt_json(t, pid, prop, item).await["claim"].clone();
    let after: chrono::DateTime<chrono::Utc> = claim["settle_after"]
        .as_str()
        .expect("the claim records settle_after")
        .parse()
        .unwrap();
    let left = (after - chrono::Utc::now()).num_milliseconds().max(0) as u64;
    tokio::time::sleep(Duration::from_millis(left + 300)).await;
}

// The process stops, or the database goes away, after the bank answered and before the outcome
// is recorded. Injected with a store that fails from the moment the stub receives the payment.
both_backends!(
    nessie3_an_outcome_lost_after_the_bank_answered_is_never_paid_twice,
    |db| {
        let (srv, ctl) = stub();
        let down = Arc::new(AtomicBool::new(false));
        let flaky: Arc<dyn Store> = Arc::new(FlakyStore {
            inner: db.store.clone(),
            down: down.clone(),
        });
        let timing = short_timing(500, 2000, 500);
        let t = app_on(flaky, db.kind, stub_state(&srv, 2.0, timing));
        let d = demo(&t).await;
        let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
        assert_eq!(
            t.post(&sandbox_path(pid), Some(tok), &json!({}))
                .await
                .status,
            StatusCode::CREATED
        );
        let purchases = |s: &MockServer| count(s, "POST", "/purchases");
        let lose_the_outcome = |on: bool| {
            *ctl.down_on_pay.lock().unwrap() = if on { Some(down.clone()) } else { None };
            if !on {
                down.store(false, Ordering::SeqCst);
            }
        };

        // 1. This server: the answer says the outcome could not be recorded; the claim stays
        // IN_FLIGHT; this server's own flow has ended, so the next read settles it MAY_EXIST.
        let a = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
        decide(&t, tok, &a.body["approval"], "APPROVED").await;
        let id = a.body["approval"]["approval_id"].clone();
        lose_the_outcome(true);
        let r = execute(&t, &d, prop, ITEM_DEPOSIT, &id).await;
        lose_the_outcome(false);
        eprintln!("NESSIE3-LOST answer: {} {}", r.status.as_u16(), r.text);
        labelled_problem(&r, 502, "NESSIE_PAYMENT_MAY_EXIST", SANDBOX);
        assert_eq!(r.body["details"]["recorded"], false);
        assert_eq!(r.body["details"]["bank_answer"], "SENT");
        let msg = r.body["message"].as_str().unwrap();
        assert!(msg.contains("may have been made"), "{msg}");
        assert!(msg.contains("before trying again"), "{msg}");
        assert_eq!(purchases(&srv), 1);
        assert_eq!(
            attempt(&t, pid, prop, ITEM_DEPOSIT)
                .await
                .unwrap()
                .attempt_status,
            "IN_FLIGHT"
        );
        assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Approved);
        let r = execute(&t, &d, prop, ITEM_DEPOSIT, &id).await;
        already(&r, "MAY_EXIST");
        assert!(r.body["message"]
            .as_str()
            .unwrap()
            .contains("may have been made"));
        assert_eq!(
            approval_status(&t, &id).await,
            ApprovalStatus::Executed,
            "the approval must be spent in the database"
        );
        let stored = attempt_json(&t, pid, prop, ITEM_DEPOSIT).await;
        assert_eq!(stored["attempt_status"], "MAY_EXIST");
        assert_eq!(stored["outcome_source"], "UNRECORDED_CLAIM");
        let second = second_approval(&t, tok, &a.body["approval"]).await;
        let r = execute(&t, &d, prop, ITEM_DEPOSIT, &second["approval_id"]).await;
        already(&r, "MAY_EXIST");
        let r = request_approval(&t, &d, prop, ITEM_DEPOSIT).await;
        already(&r, "MAY_EXIST");
        assert_eq!(purchases(&srv), 1, "never paid twice");

        // 2. The same, then a restart: a fresh client with the same bank identity (another server
        // instance, so only the claim's times tell whether its call may still be running).
        let a = request_approval(&t, &d, prop, ITEM_RENOVATION).await;
        decide(&t, tok, &a.body["approval"], "APPROVED").await;
        let id = a.body["approval"]["approval_id"].clone();
        lose_the_outcome(true);
        let r = execute(&t, &d, prop, ITEM_RENOVATION, &id).await;
        lose_the_outcome(false);
        labelled_problem(&r, 502, "NESSIE_PAYMENT_MAY_EXIST", SANDBOX);
        assert_eq!(purchases(&srv), 2);
        install(&t.app, stub_state(&srv, 2.0, timing));
        // Within its settle_after the call may still be running elsewhere: refused at once.
        let r = execute(&t, &d, prop, ITEM_RENOVATION, &id).await;
        already(&r, "IN_FLIGHT");
        let r = request_approval(&t, &d, prop, ITEM_RENOVATION).await;
        already(&r, "IN_FLIGHT");
        assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Approved);
        wait_past(&t, pid, prop, ITEM_RENOVATION).await;
        let r = execute(&t, &d, prop, ITEM_RENOVATION, &id).await;
        already(&r, "MAY_EXIST");
        assert!(r.body["message"]
            .as_str()
            .unwrap()
            .contains("Check the sandbox account's transactions"));
        assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Executed);
        let r = request_approval(&t, &d, prop, ITEM_RENOVATION).await;
        already(&r, "MAY_EXIST");
        assert_eq!(purchases(&srv), 2, "never paid twice");
        // The bank can tell: the purchase is in the account, and its description names the attempt.
        let r = refresh(&t, &d, prop, ITEM_RENOVATION).await;
        labelled(&r, SANDBOX);
        assert_eq!(r.body["attempt_status"], "SENT", "{}", r.text);
        let stored = attempt_json(&t, pid, prop, ITEM_RENOVATION).await;
        assert_eq!(stored["attempt_status"], "SENT");
        assert_eq!(stored["outcome_source"], "REFRESH_FOUND_TRANSACTION");
        assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Executed);
        assert_eq!(purchases(&srv), 2);
    }
);

// Another server settles a claim whose times say its call is over, but the call was in fact
// still running (the claim times are set shorter than this client's deadline on purpose): the
// late answer with the payment makes the attempt SENT; nothing is sent twice.
both_backends!(
    nessie3_a_late_bank_answer_after_another_server_settled_the_claim,
    |db| {
        let (srv, ctl) = stub();
        let tiny = short_timing(200, 100, 100);
        let a = app_on(db.store.clone(), db.kind, stub_state(&srv, 5.0, tiny));
        let b = app_on(db.store.clone(), db.kind, stub_state(&srv, 5.0, tiny));
        let d = demo(&a).await;
        let (tok, pid, prop) = (d.tok.as_str(), d.pid.as_str(), d.hollywood.as_str());
        a.post(&sandbox_path(pid), Some(tok), &json!({})).await;
        let ap = request_approval(&a, &d, prop, ITEM_DEPOSIT).await;
        decide(&a, tok, &ap.body["approval"], "APPROVED").await;
        let id = ap.body["approval"]["approval_id"].clone();
        ctl.delay_ms.store(1500, Ordering::SeqCst);
        let (a1, d1, id1) = (fork(&a), d.clone(), id.clone());
        let slow = tokio::spawn(async move {
            let prop = d1.hollywood.clone();
            execute(&a1, &d1, &prop, ITEM_DEPOSIT, &id1).await
        });
        tokio::time::sleep(Duration::from_millis(700)).await;
        let r = execute(&b, &d, prop, ITEM_DEPOSIT, &id).await;
        already(&r, "MAY_EXIST");
        assert_eq!(approval_status(&b, &id).await, ApprovalStatus::Executed);
        let r = slow.await.unwrap();
        ctl.delay_ms.store(0, Ordering::SeqCst);
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        assert_eq!(r.body["attempt_status"], "SENT");
        assert_eq!(r.body["approval_status"], "EXECUTED");
        let stored = attempt_json(&a, pid, prop, ITEM_DEPOSIT).await;
        assert_eq!(stored["attempt_status"], "SENT");
        assert_eq!(stored["outcome_source"], "BANK_ANSWER_AFTER_SETTLEMENT");
        assert_eq!(count(&srv, "POST", "/purchases"), 1);
        let r = execute(&b, &d, prop, ITEM_DEPOSIT, &id).await;
        already(&r, "SENT");
        assert_eq!(count(&srv, "POST", "/purchases"), 1);
    }
);

// 20 executes of one approval and one of a second approval for the same plan item, all at once,
// with real separate connections: one payment, and every refusal arrives while the bank call is
// still running.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn nessie3_postgres_executes_of_one_item_are_refused_without_waiting_for_the_bank() {
    let Some(db) = TestDb::new(Kind::Postgres).await else {
        return;
    };
    let (srv, ctl) = stub();
    let t = app_with_stub(&db, &srv, 10.0);
    let d = demo(&t).await;
    let prop = d.hollywood.clone();
    t.post(&sandbox_path(&d.pid), Some(&d.tok), &json!({}))
        .await;
    let first = request_approval(&t, &d, &prop, ITEM_DEPOSIT).await;
    decide(&t, &d.tok, &first.body["approval"], "APPROVED").await;
    let first_id = first.body["approval"]["approval_id"].clone();
    let second = second_approval(&t, &d.tok, &first.body["approval"]).await;
    let bank_ms = 3000u64;
    ctl.delay_ms.store(bank_ms, Ordering::SeqCst);
    let mut set = tokio::task::JoinSet::new();
    for i in 0..21 {
        let id = if i == 20 {
            second["approval_id"].clone()
        } else {
            first_id.clone()
        };
        let (tt, dd, pp) = (fork(&t), d.clone(), prop.clone());
        set.spawn(async move {
            let s = Instant::now();
            let r = execute(&tt, &dd, &pp, ITEM_DEPOSIT, &id).await;
            (r, s.elapsed())
        });
    }
    let mut paid = 0;
    let mut refused = Vec::new();
    while let Some(r) = set.join_next().await {
        let (r, took) = r.unwrap();
        if r.status == StatusCode::OK {
            paid += 1;
            assert!(took >= Duration::from_millis(bank_ms), "{took:?}");
        } else {
            already(&r, "IN_FLIGHT");
            refused.push(took);
        }
    }
    ctl.delay_ms.store(0, Ordering::SeqCst);
    let slowest = refused.iter().max().copied().unwrap_or_default();
    eprintln!(
        "NESSIE3-PG-21 paid {paid}, refused {} (slowest refusal {slowest:?} during a {bank_ms} ms bank call); bank POSTs {}",
        refused.len(),
        count(&srv, "POST", "/purchases")
    );
    assert_eq!(paid, 1);
    assert_eq!(refused.len(), 20);
    assert!(
        slowest < Duration::from_millis(bank_ms - 1000),
        "a refusal waited for the bank call: {slowest:?}"
    );
    assert_eq!(count(&srv, "POST", "/purchases"), 1);
    db.finish().await;
}

// Requests made while a slow bank call is in progress (four executes of one item, as the review's
// pool probe): a project list, a business-profile update of the same project, a plan read and
// another execute are all answered at once.
both_backends!(
    nessie3_other_requests_are_answered_during_a_slow_bank_call,
    |db| {
        let (srv, ctl) = stub();
        let t = app_with_stub(db, &srv, 10.0);
        let d = demo(&t).await;
        let prop = d.hollywood.clone();
        t.post(&sandbox_path(&d.pid), Some(&d.tok), &json!({}))
            .await;
        let a = request_approval(&t, &d, &prop, ITEM_DEPOSIT).await;
        decide(&t, &d.tok, &a.body["approval"], "APPROVED").await;
        let id = a.body["approval"]["approval_id"].clone();
        let bank_ms = 2500u64;
        ctl.delay_ms.store(bank_ms, Ordering::SeqCst);
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let (tt, dd, pp, ii) = (fork(&t), d.clone(), prop.clone(), id.clone());
            set.spawn(async move { execute(&tt, &dd, &pp, ITEM_DEPOSIT, &ii).await });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut took = Vec::new();
        let s = Instant::now();
        let r = t.get("/api/v1/projects", Some(&d.tok)).await;
        took.push(("project list", r.status.as_u16(), s.elapsed()));
        let s = Instant::now();
        let r = t
            .put(
                &format!("/api/v1/projects/{}/business-profile", d.pid),
                Some(&d.tok),
                &demo_project_body()["business_profile"],
            )
            .await;
        took.push(("profile update", r.status.as_u16(), s.elapsed()));
        let s = Instant::now();
        let r = t.get(&prop_path(&d.pid, &prop, "plan"), Some(&d.tok)).await;
        took.push(("plan read", r.status.as_u16(), s.elapsed()));
        let s = Instant::now();
        let r = execute(&t, &d, &prop, ITEM_DEPOSIT, &id).await;
        took.push(("another execute", r.status.as_u16(), s.elapsed()));
        already(&r, "IN_FLIGHT");
        let mut statuses = Vec::new();
        while let Some(r) = set.join_next().await {
            statuses.push(r.unwrap().status.as_u16());
        }
        ctl.delay_ms.store(0, Ordering::SeqCst);
        statuses.sort_unstable();
        for (name, status, d) in &took {
            eprintln!(
                "NESSIE3-DURING {:?} {name}: {status} in {d:?} (bank call {bank_ms} ms)",
                db.kind
            );
        }
        for (name, status, d) in &took {
            assert!(*status < 500, "{name}: {status}");
            assert!(
                *d < Duration::from_millis(1500),
                "{name} waited for the bank call: {d:?}"
            );
        }
        assert_eq!(statuses, vec![200, 409, 409, 409]);
        assert_eq!(count(&srv, "POST", "/purchases"), 1);
    }
);

// A refusal by the bank leaves the approval usable and the plan item payable; while the refused
// call is still running, the item is blocked without waiting; then it is paid exactly once.
both_backends!(
    nessie3_a_refusal_leaves_the_approval_usable_and_the_item_is_paid_once,
    |db| {
        let (srv, ctl) = stub();
        let t = app_with_stub(db, &srv, 10.0);
        let d = demo(&t).await;
        let prop = d.hollywood.clone();
        t.post(&sandbox_path(&d.pid), Some(&d.tok), &json!({}))
            .await;
        let a = request_approval(&t, &d, &prop, ITEM_DEPOSIT).await;
        decide(&t, &d.tok, &a.body["approval"], "APPROVED").await;
        let id = a.body["approval"]["approval_id"].clone();
        *ctl.mode.lock().unwrap() = Mode::Refuse;
        ctl.delay_ms.store(1500, Ordering::SeqCst);
        let (tt, dd, pp, ii) = (fork(&t), d.clone(), prop.clone(), id.clone());
        let slow = tokio::spawn(async move { execute(&tt, &dd, &pp, ITEM_DEPOSIT, &ii).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        let s = Instant::now();
        let r = execute(&t, &d, &prop, ITEM_DEPOSIT, &id).await;
        let waited = s.elapsed();
        already(&r, "IN_FLIGHT");
        assert!(
            waited < Duration::from_millis(900),
            "waited for the bank: {waited:?}"
        );
        // (No approval request here: it asks the stub for the account first, and the stub serves one
        // connection at a time, so it would wait for the stalled POST.)
        let r = slow.await.unwrap();
        labelled_problem(&r, 422, "NESSIE_PAYMENT_REFUSED", SANDBOX);
        assert_eq!(r.body["details"]["approval_status"], "APPROVED");
        assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Approved);
        assert_eq!(
            attempt(&t, &d.pid, &prop, ITEM_DEPOSIT)
                .await
                .unwrap()
                .attempt_status,
            "REFUSED"
        );
        assert_eq!(count(&srv, "POST", "/purchases"), 1);
        *ctl.mode.lock().unwrap() = Mode::Normal;
        ctl.delay_ms.store(0, Ordering::SeqCst);
        let r = execute(&t, &d, &prop, ITEM_DEPOSIT, &id).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        assert_eq!(r.body["record"]["outcome"], "SIMULATED_PENDING");
        assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Executed);
        let stored = attempt(&t, &d.pid, &prop, ITEM_DEPOSIT).await.unwrap();
        assert_eq!(stored.attempt_status, "SENT");
        assert_eq!(stored.earlier_refusals.len(), 1);
        assert_eq!(count(&srv, "POST", "/purchases"), 2);
        let r = execute(&t, &d, &prop, ITEM_DEPOSIT, &id).await;
        already(&r, "SENT");
        assert_eq!(count(&srv, "POST", "/purchases"), 2, "paid once");
    }
);

// Graceful shutdown waits for an execute between its two phases, also when its client left.
#[tokio::test]
async fn nessie3_shutdown_waits_for_an_execute_between_its_phases() {
    let db = TestDb::new(Kind::Memory).await.expect("memory");
    let (srv, ctl) = stub();
    let t = app_with_stub(&db, &srv, 10.0);
    let d = demo(&t).await;
    let prop = d.hollywood.clone();
    t.post(&sandbox_path(&d.pid), Some(&d.tok), &json!({}))
        .await;
    let a = request_approval(&t, &d, &prop, ITEM_DEPOSIT).await;
    decide(&t, &d.tok, &a.body["approval"], "APPROVED").await;
    let id = a.body["approval"]["approval_id"].clone();
    ctl.delay_ms.store(1200, Ordering::SeqCst);
    let cut = tokio::time::timeout(
        Duration::from_millis(300),
        execute(&t, &d, &prop, ITEM_DEPOSIT, &id),
    )
    .await;
    assert!(cut.is_err(), "the client was expected to leave");
    assert_eq!(
        svc::wait_for_flows_within(&t.app, Duration::from_millis(50)).await,
        1,
        "the execute is at the bank"
    );
    let s = Instant::now();
    assert_eq!(
        svc::wait_for_flows_within(&t.app, Duration::from_secs(10)).await,
        0
    );
    eprintln!("NESSIE3-SHUTDOWN waited {:?}", s.elapsed());
    assert!(s.elapsed() < Duration::from_secs(5));
    ctl.delay_ms.store(0, Ordering::SeqCst);
    let stored = attempt(&t, &d.pid, &prop, ITEM_DEPOSIT).await.unwrap();
    assert_eq!(stored.attempt_status, "SENT");
    assert_eq!(approval_status(&t, &id).await, ApprovalStatus::Executed);
    assert_eq!(count(&srv, "POST", "/purchases"), 1);
    db.finish().await;
}

// NESSIE_PROVIDER_MODE, NESSIE_API_KEY and NESSIE_BASE_URL come through the settings loader:
// from the `.env` files, with the same precedence as every other name, the environment first.
// Scratch files only (never the repository's `.env`).
#[tokio::test]
async fn nessie3_the_nessie_settings_come_through_the_settings_loader() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("nessie3-dotenv-{}", Uuid::new_v4().simple()));
    let dir = root.join("a").join("b");
    std::fs::create_dir_all(&dir).unwrap();
    const NEAR_KEY: &str = "nsk_DOTENVCANARY_near_a1b2c3";
    const FAR_KEY: &str = "nsk_DOTENVCANARY_far_d4e5f6";
    // `.env` in the working directory, `../../.env` two levels up.
    std::fs::write(
        dir.join(".env"),
        format!("NESSIE_PROVIDER_MODE=live\nNESSIE_API_KEY={NEAR_KEY}\nNESSIE_BASE_URL=http://near.invalid\nPUBLIC_BASE_URL=http://near.invalid\n"),
    )
    .unwrap();
    std::fs::write(
        root.join(".env"),
        format!("NESSIE_API_KEY={FAR_KEY}\nNESSIE_BASE_URL=http://far.invalid\nPUBLIC_BASE_URL=http://far.invalid\n"),
    )
    .unwrap();
    let env = |extra: &[(&str, &str)]| {
        let mut v = vec![("APP_ENV".to_owned(), "test".to_owned())];
        v.extend(
            extra
                .iter()
                .map(|(k, x)| ((*k).to_owned(), (*x).to_owned())),
        );
        v
    };
    let s = Settings::from_sources(&dir, env(&[])).unwrap();
    // The Nessie names take the file that every other name takes.
    let near = s.public_base_url == "http://near.invalid";
    eprintln!(
        "NESSIE3-SETTINGS the file that wins for every name: {}",
        if near { ".env" } else { "../../.env" }
    );
    let cfg = svc::nessie_config(&s).unwrap();
    assert_eq!(cfg.mode, ProviderMode::Live, "only `.env` sets it");
    let (url, key) = if near {
        ("http://near.invalid", NEAR_KEY)
    } else {
        ("http://far.invalid", FAR_KEY)
    };
    assert_eq!(cfg.base_url, url);
    assert_eq!(cfg.api_key.as_ref().map(|k| k.expose()), Some(key));
    // The environment over both files.
    let s2 = Settings::from_sources(
        &dir,
        env(&[
            ("NESSIE_PROVIDER_MODE", "fixture"),
            ("NESSIE_BASE_URL", "http://env.invalid"),
        ]),
    )
    .unwrap();
    let cfg2 = svc::nessie_config(&s2).unwrap();
    assert_eq!(cfg2.mode, ProviderMode::Fixture);
    assert_eq!(cfg2.base_url, "http://env.invalid");
    // The key is in no Debug output and not in `/ready`.
    assert!(!format!("{s:?}").contains("DOTENVCANARY"));
    assert!(!format!("{cfg:?}").contains("DOTENVCANARY"));
    let app = ss_server::build_app(s, Arc::new(MemoryStore::new()));
    let router = ss_server::http::router(app.clone());
    let t = TestApp {
        app,
        router,
        kind: Kind::Memory,
    };
    assert_eq!(
        t.app.nessie().simulation(),
        ss_nessie::Simulation::NessieSandbox
    );
    let r = t.get("/ready", None).await;
    let entry = r.body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "nessie")
        .unwrap()
        .clone();
    assert_eq!(entry["mode"], "live", "{entry}");
    assert_eq!(entry["credentials_present"], true);
    assert_eq!(entry["status"], "PARTIAL");
    assert!(!r.text.contains("DOTENVCANARY"));
    let _ = std::fs::remove_dir_all(&root);
}
