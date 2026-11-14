//! The live client against an in-process HTTP server: documented paths, query strings and
//! bodies; success, 4xx, 5xx, timeout, redirect and transport failures; retries only for GET;
//! the `/ready` status; and the key never in a URL, error, `Debug` output or log line.

mod common;

use std::net::TcpListener;
use std::time::Duration;

use chrono::Utc;
use common::{
    approved, assert_nessie_conforms, capture_logs, captured_logs, json, nessie_golden, problem,
    MockServer, Recorded, Reply,
};
use serde_json::{json, Value};
use ss_core::IntegrationStatus;
use ss_domain::integrations::base::{ProviderErrorKind, RetryPolicy};
use ss_nessie::approval::PaymentOperation;
use ss_nessie::config::ApiKey;
use ss_nessie::models::*;
use ss_nessie::{
    ApprovalAction, AttemptOutcome, AuthorizationError, LiveNessieBank, NessieBank, NessieError,
    PaymentAuthorization, PaymentIntent, SimulatedOutcome, Simulation, NESSIE_SANDBOX_DISCLAIMER,
};
use uuid::Uuid;

const KEY: &str = "nsk_TEST_live_mock_9f1c2e7a5b3d4c6e8a0b";

fn bank(url: &str, attempts: u32, timeout_s: f64) -> LiveNessieBank {
    capture_logs();
    LiveNessieBank::with_policy(
        url,
        ApiKey::new(KEY).unwrap(),
        RetryPolicy {
            attempts,
            backoff_s: 0.0,
            timeout_s,
        },
    )
}

/// Every request: bearer header with the key, the key nowhere else, a documented path and
/// query names, and a body that conforms to the operation's request schema.
fn check_request(r: &Recorded, operation: &str) {
    let ops = nessie_golden("schemas.json")["operations"].clone();
    let op = &ops[operation];
    assert_eq!(r.method, op["method"], "{operation}");
    let template = op["path"].as_str().unwrap();
    let re_matches = {
        let t: Vec<&str> = template.split('/').collect();
        let p: Vec<&str> = r.path.split('/').collect();
        t.len() == p.len()
            && t.iter()
                .zip(&p)
                .all(|(a, b)| a == b || (a.starts_with('{') && !b.is_empty()))
    };
    assert!(re_matches, "{} does not match {template}", r.path);
    if operation == "health" {
        assert_eq!(r.header("Authorization"), None, "/health is public: no key");
    } else {
        assert_eq!(
            r.header("Authorization"),
            Some(format!("Bearer {KEY}").as_str())
        );
    }
    assert!(!r.path.contains(KEY) && !r.query.contains(KEY));
    let documented: Vec<&str> = op["query"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for pair in r.query.split('&').filter(|s| !s.is_empty()) {
        let name = pair.split('=').next().unwrap();
        assert!(
            documented.contains(&name),
            "{operation}: undocumented query {name}"
        );
    }
    match op["request_schema"].as_str() {
        Some(schema) => {
            assert_eq!(r.header("Content-Type"), Some("application/json"));
            assert_nessie_conforms(schema, &r.json());
        }
        None => assert!(r.body.is_empty(), "{operation} sends no body"),
    }
}

fn assert_key_absent(text: &str) {
    assert!(!text.contains(KEY), "key leaked: {text}");
    assert!(
        !text.contains("9f1c2e7a5b3d"),
        "key fragment leaked: {text}"
    );
}

fn tx_body(id: &str, kind: &str, status: &str, amount: i64) -> Value {
    json!({
        "id": id, "kind": kind, "status": status, "medium": "balance", "amount_cents": amount,
        "payer_id": "acct_1", "settle_at": "2026-11-14T12:00:10Z",
        "created_at": "2026-11-14T12:00:00Z", "updated_at": "2026-11-14T12:00:00Z"
    })
}

fn account_body(id: &str, balance: i64) -> Value {
    json!({
        "id": id, "customer_id": "cust_1", "type": "Checking", "nickname": "n",
        "balance_cents": balance, "pending_debits_cents": 0, "available_cents": balance,
        "account_number": "1", "rewards": 0,
        "created_at": "2026-11-14T12:00:00Z", "updated_at": "2026-11-14T12:00:00Z"
    })
}

fn intent(op: PaymentOperation, amount: i64) -> PaymentIntent {
    let action = match op {
        PaymentOperation::Purchase { .. } => ApprovalAction::Purchase,
        _ => ApprovalAction::FinancialCommitment,
    };
    PaymentIntent {
        simulation: Simulation::NessieSandbox,
        project_id: Uuid::from_u128(1),
        property_id: Uuid::from_u128(2),
        scenario_id: Uuid::from_u128(3),
        plan_item_id: "buildout:renovation".into(),
        attempt_id: "0123abcd4567".into(),
        action,
        operation: op,
        amount_minor: amount,
        currency: "USD".into(),
        description: "SIMULATION (Second Site plan item buildout:renovation, attempt 0123abcd4567): test".into(),
    }
}

fn purchase(amount: i64) -> PaymentIntent {
    intent(
        PaymentOperation::Purchase {
            account_id: "acct_1".into(),
            merchant_id: "merch_1".into(),
        },
        amount,
    )
}

fn auth(i: &PaymentIntent) -> PaymentAuthorization {
    PaymentAuthorization::verify(&approved(i), i.clone(), Utc::now()).unwrap()
}

#[test]
fn reads_send_the_key_only_as_a_bearer_header_and_verify_the_status() {
    let srv = MockServer::start(vec![
        json(
            200,
            json!({"status": "healthy", "service": "nessie", "version": "2.1.3", "commit": "x", "timestamp": 1, "uptime_seconds": 2}),
        ),
        json(200, account_body("acct_1", 5000)),
        json(
            200,
            json!({"items": [account_body("acct_1", 5000)], "next_cursor": "c2"}),
        ),
        json(
            200,
            json!({"items": [tx_body("txn_1", "purchase", "pending", 10)]}),
        ),
        json(
            200,
            json!({"id": "merch_1", "name": "SIMULATED LANDLORD", "created_at": "t", "updated_at": "t"}),
        ),
        json(
            200,
            json!({"id": "bill_1", "account_id": "acct_1", "status": "recurring", "payee": "p", "creation_date": "2026-11-14", "upcoming_payment_date": "2026-12-01", "payment_amount": 6900.0, "created_at": "t", "updated_at": "t"}),
        ),
        json(
            200,
            json!({"id": "cust_1", "first_name": "A", "last_name": "B", "address": {"street_number": "1", "street_name": "s", "city": "c", "state": "VA", "zip": "22101"}, "created_at": "t", "updated_at": "t"}),
        ),
        json(200, tx_body("txn_1", "purchase", "completed", 10)),
    ]);
    let b = bank(&srv.url, 1, 2.0);
    let st = b.status();
    assert_eq!(st.status, IntegrationStatus::Partial);
    assert!(st.note.starts_with("CONFIGURED_UNVERIFIED"));

    let h = b.health().unwrap();
    assert_eq!(h.version, "2.1.3");
    assert_eq!(
        b.status().status,
        IntegrationStatus::Partial,
        "/health proves nothing about the key"
    );

    let a = b.get_account("acct_1").unwrap();
    assert_eq!(a.simulation, Simulation::NessieSandbox);
    assert_eq!(a.disclaimer, NESSIE_SANDBOX_DISCLAIMER);
    assert_eq!(a.record.available_cents, 5000);
    let st = b.status();
    assert_eq!(st.status, IntegrationStatus::Live);
    assert!(st.note.starts_with("VERIFIED: "), "{}", st.note);
    assert!(st.note.contains("mock money"));
    assert!(st.credentials_present);

    b.verify().unwrap();
    let l = b.list_account_transactions("acct_1", 5).unwrap();
    assert_eq!(l.record.len(), 1);
    b.get_merchant("merch_1").unwrap();
    let bill = b.get_bill("bill_1").unwrap();
    assert_eq!(bill.record.payment_amount_minor().unwrap().minor, 690000);
    b.get_customer("cust_1").unwrap();
    let t = b.get_transaction("txn_1").unwrap();
    assert_eq!(t.record.status, TransactionStatus::Completed);

    let reqs = srv.requests();
    let ops = [
        "health",
        "get-account",
        "list-accounts",
        "list-account-transactions",
        "get-merchant",
        "get-bill",
        "get-customer",
        "get-transaction",
    ];
    assert_eq!(reqs.len(), ops.len());
    for (r, op) in reqs.iter().zip(ops) {
        check_request(r, op);
        assert_eq!(r.header("Accept"), Some("application/json"));
    }
    assert_eq!(reqs[1].path, "/accounts/acct_1");
    assert_eq!(reqs[2].query, "limit=1");
    assert_eq!(reqs[3].path, "/accounts/acct_1/transactions");
    assert_eq!(reqs[3].query, "limit=5");
    for line in captured_logs() {
        assert_key_absent(&line);
    }
    assert!(!format!("{b:?}").contains(KEY));
}

#[test]
fn creates_send_exact_documented_bodies() {
    let srv = MockServer::start(vec![
        json(
            201,
            json!({"id": "cust_1", "first_name": "Simulated", "last_name": "Owner", "address": {"street_number": "1", "street_name": "s", "city": "c", "state": "CA", "zip": "90028"}, "created_at": "t", "updated_at": "t"}),
        ),
        json(201, account_body("acct_1", 1_500_000)),
        json(
            201,
            json!({"id": "merch_1", "name": "SIMULATED CONTRACTOR (Second Site stand-in)", "category": "construction", "created_at": "t", "updated_at": "t"}),
        ),
        json(201, tx_body("txn_9", "deposit", "pending", 2500)),
        json(200, tx_body("txn_9", "deposit", "completed", 2500)),
        json(200, tx_body("txn_8", "purchase", "cancelled", 10)),
        Reply::Body(204, "application/json", String::new()),
        Reply::Body(204, "application/json", String::new()),
        Reply::Body(204, "application/json", String::new()),
        Reply::Body(204, "application/json", String::new()),
    ]);
    let b = bank(&srv.url, 1, 2.0);
    let c = b
        .create_customer(&CustomerCreate {
            first_name: "Simulated".into(),
            last_name: "Owner".into(),
            address: Address {
                street_number: "1".into(),
                street_name: "s".into(),
                city: "c".into(),
                state: "CA".into(),
                zip: "90028".into(),
            },
        })
        .unwrap();
    assert_eq!(c.record.id, "cust_1");
    b.create_account(
        "cust_1",
        &AccountCreate {
            account_type: AccountType::Checking,
            nickname: "Opening fund".into(),
            balance_cents: 1_500_000,
            credit_limit_cents: None,
            rewards: None,
        },
    )
    .unwrap();
    b.create_merchant(&ss_nessie::plan::stand_in_merchant(
        ss_nessie::PayeeRole::Contractor,
    ))
    .unwrap();
    b.deposit(
        "acct_1",
        &DepositCreate {
            amount_cents: 2500,
            description: Some("SIMULATION funding".into()),
        },
    )
    .unwrap();
    b.settle_transaction("txn_9").unwrap();
    b.cancel_transaction("txn_8").unwrap();
    b.delete_merchant("merch_1").unwrap();
    b.delete_bill("bill_1").unwrap();
    b.delete_account("acct_1").unwrap();
    b.delete_customer("cust_1").unwrap();

    let reqs = srv.requests();
    let ops = [
        "create-customer",
        "create-account-for-customer",
        "create-merchant",
        "create-deposit",
        "settle-transaction",
        "cancel-transaction",
        "delete-merchant",
        "delete-bill",
        "delete-account",
        "delete-customer",
    ];
    assert_eq!(reqs.len(), ops.len());
    for (r, op) in reqs.iter().zip(ops) {
        check_request(r, op);
    }
    assert_eq!(reqs[1].path, "/customers/cust_1/accounts");
    assert_eq!(
        reqs[1].json(),
        json!({"type": "Checking", "nickname": "Opening fund", "balance_cents": 1500000})
    );
    assert_eq!(
        reqs[3].json(),
        json!({"amount_cents": 2500, "description": "SIMULATION funding"})
    );
    assert_eq!(reqs[4].path, "/transactions/txn_9/settle");
    assert_eq!(reqs[6].method, "DELETE");
    // Invalid input is refused before anything is sent.
    assert!(matches!(
        b.get_account("acct_1?key=x"),
        Err(NessieError::InvalidRequest(_))
    ));
    assert!(matches!(
        b.list_accounts(101, None),
        Err(NessieError::InvalidRequest(_))
    ));
    assert_eq!(srv.requests().len(), ops.len());
}

#[test]
fn executes_the_three_payment_operations() {
    let srv = MockServer::start(vec![
        json(201, {
            let mut t = tx_body("txn_1", "purchase", "pending", 1_200_000);
            t["merchant_id"] = json!("merch_1");
            t
        }),
        json(
            201,
            json!({"id": "bill_1", "account_id": "acct_1", "status": "recurring", "payee": "SIMULATED LANDLORD (Second Site stand-in)", "nickname": "SIMULATION: monthly rent + NNN", "creation_date": "2026-11-14", "recurring_date": 1, "upcoming_payment_date": "2026-12-01", "payment_amount": 6900.5, "created_at": "t", "updated_at": "t"}),
        ),
        json(201, {
            let mut t = tx_body("txn_2", "bill_payment", "completed", 690_050);
            t["bill_id"] = json!("bill_1");
            t
        }),
    ]);
    let b = bank(&srv.url, 3, 2.0);

    let p = purchase(1_200_000);
    let r = b.execute(auth(&p)).unwrap();
    assert_eq!(r.simulation, Simulation::NessieSandbox);
    assert_eq!(r.outcome, SimulatedOutcome::SimulatedPending);
    assert_eq!(r.nessie_transaction_id.as_deref(), Some("txn_1"));
    assert!(
        r.note.contains("pending in the Capital One Nessie sandbox"),
        "{}",
        r.note
    );

    let bill = intent(
        PaymentOperation::CreateRecurringBill {
            account_id: "acct_1".into(),
            payee: "SIMULATED LANDLORD (Second Site stand-in)".into(),
            nickname: "SIMULATION: monthly rent + NNN".into(),
            recurring_day: 1,
        },
        690_050,
    );
    let r = b.execute(auth(&bill)).unwrap();
    assert_eq!(r.outcome, SimulatedOutcome::SimulatedBillRecorded);
    assert_eq!(r.nessie_bill_id.as_deref(), Some("bill_1"));
    assert!(!r.moves_money);

    let pay = intent(
        PaymentOperation::PayBill {
            account_id: "acct_1".into(),
            bill_id: "bill_1".into(),
        },
        690_050,
    );
    let r = b.execute(auth(&pay)).unwrap();
    // Completed only because Nessie said so.
    assert_eq!(r.outcome, SimulatedOutcome::SimulatedCompleted);
    assert_eq!(r.provider_status, "completed");

    let reqs = srv.requests();
    assert_eq!(reqs.len(), 3);
    check_request(&reqs[0], "create-purchase");
    check_request(&reqs[1], "create-bill-for-account");
    check_request(&reqs[2], "pay-bill");
    assert_eq!(reqs[0].path, "/accounts/acct_1/purchases");
    assert_eq!(
        reqs[0].json(),
        json!({"merchant_id": "merch_1", "amount_cents": 1200000, "description": "SIMULATION (Second Site plan item buildout:renovation, attempt 0123abcd4567): test"})
    );
    assert_eq!(reqs[1].path, "/accounts/acct_1/bills");
    assert_eq!(
        reqs[1].body,
        r#"{"status":"recurring","payee":"SIMULATED LANDLORD (Second Site stand-in)","nickname":"SIMULATION: monthly rent + NNN","payment_amount":6900.5,"recurring_date":1}"#
    );
    assert_eq!(reqs[2].path, "/bills/bill_1/payments");
    assert_eq!(reqs[2].json(), json!({"amount_cents": 690050}));
    for line in captured_logs() {
        assert_key_absent(&line);
    }
}

#[test]
fn refusals_failures_and_retries() {
    let srv = MockServer::start(vec![
        // 1. GET 401 -> AUTH, key rejected, not retried.
        problem(
            401,
            "unauthorized",
            "Unauthorized",
            "missing or invalid key",
        ),
        // 2. GET 404 with a problem body -> NOT_FOUND, not retried.
        problem(404, "not_found", "Not Found", "Account acct_x not found"),
        // 3-5. GET 503, 503, 200 -> retried to success.
        json(
            503,
            json!({"type": "https://nessieisreal.com/problems/unavailable", "title": "Service Unavailable", "status": 503}),
        ),
        problem(429, "rate_limited", "Too Many Requests", "slow down"),
        json(200, account_body("acct_1", 1)),
        // 6-8. GET 500 x3 -> UNAVAILABLE after three attempts.
        json(500, json!({})),
        json(500, json!({})),
        json(500, json!({})),
        // 9. POST purchase 422 insufficient_funds -> REFUSED, approval released.
        problem(
            422,
            "insufficient_funds",
            "Unprocessable Entity",
            "Account acct_1 has 500 cents available.",
        ),
        // 10. The same approval again, now 500 -> MAY_EXIST, not retried.
        json(500, json!({})),
        // 11. Success body that is not a transaction -> MAY_EXIST.
        json(201, json!({"ok": true})),
        // 12. Amount differs from the approved amount -> MAY_EXIST.
        json(201, tx_body("txn_3", "purchase", "pending", 999)),
        // 13. A server echoing the key back in a problem detail.
        problem(
            400,
            "bad_request",
            "Bad Request",
            &format!("bad header Bearer {KEY}"),
        ),
        // 14. Redirects are not followed.
        Reply::Redirect(302, "http://example.invalid/elsewhere".into()),
    ]);
    let b = bank(&srv.url, 3, 2.0);

    let e = b.get_account("acct_1").unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::Auth));
    assert_eq!(e.http_status(), Some(401));
    let st = b.status();
    assert_eq!(st.status, IntegrationStatus::Blocked);
    assert!(st.note.starts_with("KEY_REJECTED: "));

    let e = b.get_account("acct_x").unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::NotFound));
    assert_eq!(e.problem_slug(), Some("not_found"));
    assert!(
        e.to_string().contains("HTTP 404 not_found: Not Found"),
        "{e}"
    );
    assert_eq!(
        b.status().status,
        IntegrationStatus::Blocked,
        "a 404 says nothing about the key"
    );

    let a = b.get_account("acct_1").unwrap();
    assert_eq!(a.record.id, "acct_1");
    assert_eq!(b.status().status, IntegrationStatus::Live);

    let e = b.get_account("acct_1").unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::Unavailable));
    assert!(e.retryable());
    let st = b.status();
    assert_eq!(st.status, IntegrationStatus::Partial);
    assert!(st.note.starts_with("LAST_CALL_FAILED: "));
    assert!(st.note.contains("UNAVAILABLE"));

    let p = purchase(800_000);
    let ev = approved(&p);
    let err = b
        .execute(PaymentAuthorization::verify(&ev, p.clone(), Utc::now()).unwrap())
        .unwrap_err();
    assert_eq!(err.outcome, AttemptOutcome::Refused);
    assert_eq!(err.error.problem_slug(), Some("insufficient_funds"));
    assert_eq!(err.error.kind(), Some(ProviderErrorKind::BadResponse));
    assert!(err
        .to_string()
        .contains("Account acct_1 has 500 cents available."));

    let err = b
        .execute(PaymentAuthorization::verify(&ev, p.clone(), Utc::now()).unwrap())
        .unwrap_err();
    assert_eq!(err.outcome, AttemptOutcome::MayExist);
    assert_eq!(err.error.kind(), Some(ProviderErrorKind::Unavailable));
    // A create that may have happened spends the approval in this process.
    let again = b
        .execute(PaymentAuthorization::verify(&ev, p.clone(), Utc::now()).unwrap())
        .unwrap_err();
    assert_eq!(again.outcome, AttemptOutcome::NotSent);
    assert_eq!(
        again.error,
        NessieError::NotAuthorized(AuthorizationError::AlreadyUsed(ev.approval_id))
    );

    let err = b.execute(auth(&purchase(5))).unwrap_err();
    assert_eq!(err.outcome, AttemptOutcome::MayExist);
    assert_eq!(err.error.kind(), Some(ProviderErrorKind::BadResponse));
    let err = b.execute(auth(&purchase(5))).unwrap_err();
    assert_eq!(err.outcome, AttemptOutcome::MayExist);
    assert!(err.to_string().contains("amount_cents 999"), "{err}");

    let e = b.get_account("acct_1").unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::BadResponse));
    assert_key_absent(&e.to_string());
    assert_key_absent(&format!("{e:?}"));
    assert!(e.to_string().contains("Bearer ***"), "{e}");

    let e = b.get_account("acct_1").unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::BadResponse));
    assert_eq!(e.http_status(), Some(302));

    let reqs = srv.requests();
    assert_eq!(
        reqs.len(),
        14,
        "{:#?}",
        reqs.iter().map(|r| &r.path).collect::<Vec<_>>()
    );
    for (i, r) in reqs.iter().enumerate() {
        let op = if (8..=11).contains(&i) {
            "create-purchase"
        } else {
            "get-account"
        };
        check_request(r, op);
    }
    for line in captured_logs() {
        assert_key_absent(&line);
    }
}

#[test]
fn timeouts_and_transport_failures() {
    let srv = MockServer::start(vec![
        Reply::Stall(Duration::from_millis(1500)),
        Reply::Stall(Duration::from_millis(1500)),
    ]);
    let b = bank(&srv.url, 1, 0.3);
    let e = b.get_account("acct_1").unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::Timeout));
    assert_eq!(e.to_string(), "[nessie] TIMEOUT: timed out");
    assert!(b.status().note.contains("TIMEOUT"));
    let err = b.execute(auth(&purchase(5))).unwrap_err();
    assert_eq!(
        err.outcome,
        AttemptOutcome::MayExist,
        "a timed-out create may have happened"
    );
    assert_eq!(err.error.kind(), Some(ProviderErrorKind::Timeout));

    // A closed port: UNAVAILABLE, with neither the URL nor the key in the message.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}");
    let b = bank(&url, 2, 0.5);
    let e = b.get_account("acct_1").unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::Unavailable));
    assert!(!e.to_string().contains(&port.to_string()), "{e}");
    assert!(!e.to_string().contains("127.0.0.1"), "{e}");
    assert_key_absent(&e.to_string());

    // A bad base URL is a clean error, not a panic.
    let b = bank("not a url", 1, 0.5);
    assert_eq!(
        b.get_account("acct_1").unwrap_err().kind(),
        Some(ProviderErrorKind::BadResponse)
    );
    for line in captured_logs() {
        assert_key_absent(&line);
    }
}

#[test]
fn a_fixture_approval_never_reaches_the_sandbox() {
    let srv = MockServer::start(vec![]);
    let b = bank(&srv.url, 1, 0.5);
    let mut i = purchase(5);
    i.simulation = Simulation::FixtureBank;
    let err = b.execute(auth(&i)).unwrap_err();
    assert_eq!(err.outcome, AttemptOutcome::NotSent);
    assert!(matches!(
        err.error,
        NessieError::NotAuthorized(AuthorizationError::WrongBank { .. })
    ));
    assert!(srv.requests().is_empty());
}

#[test]
fn ureq_logs_are_captured_and_masked() {
    let srv = MockServer::start(vec![json(200, account_body("acct_1", 1))]);
    let b = bank(&srv.url, 1, 2.0);
    b.get_account("acct_1").unwrap();
    let logs = captured_logs();
    // The capture works (ureq logs the request prelude at debug level) ...
    assert!(
        logs.iter().any(|l| l.contains("Authorization: ***")),
        "no masked prelude captured: {logs:#?}"
    );
    // ... and nothing carries the key.
    for line in &logs {
        assert_key_absent(line);
    }
}
