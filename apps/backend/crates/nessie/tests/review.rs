//! Independent review (nessie-1-review): adversarial checks of the approval gate, the live
//! client's limits, the fixture bank's arithmetic and the affordability verdict. Each test names
//! the review finding it pins.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use common::{approved, capture_logs, captured_logs, golden_scenario, HOLLYWOOD};
use serde_json::{json, Map, Value};
use ss_core::IntegrationStatus;
use ss_domain::contracts::{CostKind, CostLineItem};
use ss_domain::integrations::base::{ProviderErrorKind, RetryPolicy};
use ss_nessie::approval::PaymentOperation;
use ss_nessie::config::ApiKey;
use ss_nessie::models::*;
use ss_nessie::{
    check_affordability, plan_move_in_payments, AffordabilityVerdict, ApprovalAction,
    AttemptOutcome, AuthorizationError, BankRecord, BomTotals, FixtureBank, LiveNessieBank,
    NessieBank, PaymentAuthorization, PaymentIntent, SandboxTargets, Simulation,
};
use uuid::Uuid;

const KEY: &str = "nsk_REVIEW_4c1d9e2f7a6b5c3d8e0f";

fn live(url: &str, timeout_s: f64) -> LiveNessieBank {
    capture_logs();
    LiveNessieBank::with_policy(
        url,
        ApiKey::new(KEY).unwrap(),
        RetryPolicy {
            attempts: 1,
            backoff_s: 0.0,
            timeout_s,
        },
    )
}

fn account_json(balance: i64) -> String {
    json!({
        "id": "acct_1", "customer_id": "cust_1", "type": "Checking", "nickname": "n",
        "balance_cents": balance, "pending_debits_cents": 0, "available_cents": balance,
        "account_number": "1", "rewards": 0,
        "created_at": "2026-11-14T12:00:00Z", "updated_at": "2026-11-14T12:00:00Z"
    })
    .to_string()
}

/// One connection: read the request head (and body), then send `head` and `body` in `chunks`
/// pieces with `gap` between them.
fn drip_server(status: &str, body: Vec<u8>, chunks: usize, gap: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let status = status.to_owned();
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let l = line.trim_end().to_ascii_lowercase();
            if let Some(v) = l.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
            if l.is_empty() {
                break;
            }
        }
        let mut sink = vec![0u8; len];
        let _ = reader.read_exact(&mut sink);
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let step = body.len().div_ceil(chunks.max(1)).max(1);
        for piece in body.chunks(step) {
            if stream.write_all(piece).is_err() || stream.flush().is_err() {
                return;
            }
            thread::sleep(gap);
        }
    });
    url
}

fn purchase_intent(amount: i64) -> PaymentIntent {
    PaymentIntent {
        simulation: Simulation::NessieSandbox,
        project_id: Uuid::from_u128(1),
        property_id: Uuid::from_u128(2),
        scenario_id: Uuid::from_u128(3),
        plan_item_id: "buildout:renovation".into(),
        attempt_id: "0123abcd4567".into(),
        action: ApprovalAction::Purchase,
        operation: PaymentOperation::Purchase {
            account_id: "acct_1".into(),
            merchant_id: "merch_1".into(),
        },
        amount_minor: amount,
        currency: "USD".into(),
        description: "SIMULATION (Second Site plan item buildout:renovation, attempt 0123abcd4567): review".into(),
    }
}

fn assert_no_key(text: &str) {
    assert!(!text.contains(KEY), "key leaked: {text}");
    assert!(
        !text.contains("4c1d9e2f7a6b"),
        "key fragment leaked: {text}"
    );
}

// ---- R1: a slow body is bounded by the timeout ----------------------------------------------

#[test]
fn review_r1_slow_body_times_out_instead_of_hanging() {
    // Every gap (0.25 s) is shorter than the 0.5 s timeout, so only an overall deadline stops
    // it; the whole drip takes about 3 s.
    let url = drip_server(
        "200 OK",
        account_json(5000).into_bytes(),
        12,
        Duration::from_millis(250),
    );
    let b = live(&url, 0.5);
    let start = Instant::now();
    let r = b.get_account("acct_1");
    let took = start.elapsed();
    assert!(
        took < Duration::from_millis(2000),
        "a dripping body held the call for {took:?}: {r:?}"
    );
    let e = r.unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::Timeout), "{e}");

    // The same for a create: the payment may exist, so the approval stays spent.
    let tx = json!({"id": "txn_1", "kind": "purchase", "status": "pending", "medium": "balance",
        "amount_cents": 5, "settle_at": "t", "created_at": "t", "updated_at": "t"});
    let url = drip_server(
        "201 Created",
        tx.to_string().into_bytes(),
        12,
        Duration::from_millis(250),
    );
    let b = live(&url, 0.5);
    let i = purchase_intent(5);
    let auth = PaymentAuthorization::verify(&approved(&i), i.clone(), Utc::now()).unwrap();
    let start = Instant::now();
    let err = b.execute(auth).unwrap_err();
    assert!(start.elapsed() < Duration::from_millis(2000));
    assert_eq!(err.outcome, AttemptOutcome::MayExist);
    assert_eq!(err.error.kind(), Some(ProviderErrorKind::Timeout));
}

#[test]
fn review_r1_absurd_timeout_configuration_does_not_panic() {
    for t in [1e300, f64::MAX, f64::INFINITY, f64::NAN, -1.0, 0.0] {
        let b = LiveNessieBank::new("http://127.0.0.1:9", ApiKey::new(KEY).unwrap(), t);
        assert_eq!(b.base_url(), "http://127.0.0.1:9");
    }
}

// ---- Huge bodies and odd keys (probes of the documented limits) ------------------------------

#[test]
fn review_huge_success_body_is_refused_and_huge_error_body_is_cut() {
    let big = vec![b' '; (16 * 1024 * 1024) + 4096];
    let url = drip_server("200 OK", big, 1, Duration::from_millis(0));
    let b = live(&url, 5.0);
    let e = b.get_account("acct_1").unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::BadResponse), "{e}");
    assert!(e.to_string().contains("larger than"), "{e}");

    let mut problem = json!({"type": "https://nessieisreal.com/problems/insufficient_funds",
        "title": "Unprocessable Entity", "status": 422})
    .to_string()
    .into_bytes();
    problem.extend(std::iter::repeat_n(b' ', 1024 * 1024));
    let url = drip_server(
        "422 Unprocessable Entity",
        problem,
        1,
        Duration::from_millis(0),
    );
    let b = live(&url, 5.0);
    let i = purchase_intent(5);
    let auth = PaymentAuthorization::verify(&approved(&i), i.clone(), Utc::now()).unwrap();
    let err = b.execute(auth).unwrap_err();
    // A 4xx: refused before anything exists, whatever the size of the body.
    assert_eq!(err.outcome, AttemptOutcome::Refused);
    assert_eq!(err.error.http_status(), Some(422));
}

#[test]
fn review_a_key_with_a_control_character_never_leaks() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    capture_logs();
    let bad = format!("{KEY}\r\nX-Injected: 1");
    let b = LiveNessieBank::with_policy(
        &url,
        ApiKey::new(bad).unwrap(),
        RetryPolicy {
            attempts: 1,
            backoff_s: 0.0,
            timeout_s: 0.5,
        },
    );
    let e = b.get_account("acct_1").unwrap_err();
    assert_no_key(&e.to_string());
    assert_no_key(&format!("{e:?}"));
    assert_no_key(&format!("{b:?}"));
    for line in captured_logs() {
        assert_no_key(&line);
    }
    // Nothing reached the server (the header is refused before connecting).
    listener.set_nonblocking(true).unwrap();
    assert!(listener.accept().is_err(), "a request was sent");
}

// ---- R5: LIVE only after an answer this crate could read ----------------------------------------

#[test]
fn review_r5_an_unreadable_success_does_not_make_the_status_live() {
    let url = drip_server(
        "200 OK",
        b"<html>not the Nessie API</html>".to_vec(),
        1,
        Duration::from_millis(0),
    );
    let b = live(&url, 2.0);
    let e = b.verify().unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::BadResponse));
    let st = b.status();
    assert_ne!(st.status, IntegrationStatus::Live, "{}", st.note);

    let url = drip_server(
        "200 OK",
        account_json(1).into_bytes(),
        1,
        Duration::from_millis(0),
    );
    let b = live(&url, 2.0);
    b.get_account("acct_1").unwrap();
    assert_eq!(b.status().status, IntegrationStatus::Live);
}

// ---- Approval gate probes ------------------------------------------------------------------------

#[test]
fn review_gate_key_order_is_irrelevant_but_any_value_change_refuses() {
    let mut i = purchase_intent(1_200_000);
    i.project_id = Uuid::from_u128(0x00ab_cdef);
    let mut ev = approved(&i);
    // The same payload with its keys in reverse insertion order authorizes.
    let mut reversed = Map::new();
    let mut entries: Vec<(String, Value)> = ev.action_payload.clone().into_iter().collect();
    entries.reverse();
    for (k, v) in entries {
        reversed.insert(k, v);
    }
    ev.action_payload = reversed;
    assert!(PaymentAuthorization::verify(&ev, i.clone(), Utc::now()).is_ok());

    for (k, v) in [
        ("amount_minor", json!("1200000")),
        ("amount_minor", json!(1_200_000_f64)),
        ("moves_money", json!("true")),
        ("project_id", json!(i.project_id.to_string().to_uppercase())),
        ("currency", json!("usd")),
        ("integration", json!("capital_one_nessie_v1")),
    ] {
        let mut e = approved(&i);
        e.action_payload.insert(k.into(), v);
        assert_eq!(
            PaymentAuthorization::verify(&e, i.clone(), Utc::now()).unwrap_err(),
            AuthorizationError::PayloadMismatch,
            "{k}"
        );
    }
    for status in ["APPROVED ", " APPROVED", "Approved", ""] {
        let mut e = approved(&i);
        e.approval_status = status.into();
        assert!(matches!(
            PaymentAuthorization::verify(&e, i.clone(), Utc::now()),
            Err(AuthorizationError::NotApproved(_))
        ));
    }
    // An approval for the deposit cannot pay the renovation, even with the same amount.
    let mut other = i.clone();
    other.plan_item_id = "movein:security_deposit".into();
    assert_eq!(
        PaymentAuthorization::verify(&approved(&other), i.clone(), Utc::now()).unwrap_err(),
        AuthorizationError::PayloadMismatch
    );
}

#[test]
fn review_gate_single_use_per_bank_instance_and_fixture_never_sandbox() {
    let bank = FixtureBank::new();
    let c = bank
        .create_customer(&CustomerCreate {
            first_name: "A".into(),
            last_name: "B".into(),
            address: Address {
                street_number: "1".into(),
                street_name: "s".into(),
                city: "c".into(),
                state: "CA".into(),
                zip: "90028".into(),
            },
        })
        .unwrap();
    let a = bank
        .create_account(
            &c.record.id,
            &AccountCreate {
                account_type: AccountType::Checking,
                nickname: "n".into(),
                balance_cents: 1_000,
                credit_limit_cents: None,
                rewards: None,
            },
        )
        .unwrap();
    let m = bank
        .create_merchant(&ss_nessie::plan::stand_in_merchant(
            ss_nessie::PayeeRole::Contractor,
        ))
        .unwrap();
    let mut i = purchase_intent(10);
    i.simulation = Simulation::FixtureBank;
    i.operation = PaymentOperation::Purchase {
        account_id: a.record.id.clone(),
        merchant_id: m.record.id.clone(),
    };
    let ev = approved(&i);
    let r = bank
        .execute(PaymentAuthorization::verify(&ev, i.clone(), Utc::now()).unwrap())
        .unwrap();
    assert_eq!(r.simulation, Simulation::FixtureBank);
    assert!(r.nessie_transaction_id.unwrap().starts_with("fixture_"));
    let again = bank
        .execute(PaymentAuthorization::verify(&ev, i.clone(), Utc::now()).unwrap())
        .unwrap_err();
    assert_eq!(again.outcome, AttemptOutcome::NotSent);
    assert_eq!(
        bank.list_account_transactions(&a.record.id, 10)
            .unwrap()
            .record
            .len(),
        1
    );
}

// ---- Every output says what it is ------------------------------------------------------------------

#[test]
fn review_every_serialized_output_carries_the_marker() {
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    for sim in [Simulation::NessieSandbox, Simulation::FixtureBank] {
        let plan = plan_move_in_payments(&s, None, sim, None).unwrap();
        let acct = BankRecord::new(
            sim,
            serde_json::from_str::<Account>(&account_json(1)).unwrap(),
        );
        let report = check_affordability(&acct, &plan, &s, None).unwrap();
        for v in [
            serde_json::to_value(&plan).unwrap(),
            serde_json::to_value(&report).unwrap(),
            serde_json::to_value(&acct).unwrap(),
        ] {
            assert_eq!(v["simulation"], sim.as_str());
            assert_eq!(v["disclaimer"], sim.disclaimer());
        }
    }
}

// ---- R4: the fixture bank never wraps or panics on huge amounts -------------------------------------

fn fixture_with(kind: AccountType, balance: i64, limit: Option<i64>) -> (FixtureBank, String) {
    let bank = FixtureBank::new();
    let c = bank
        .create_customer(&CustomerCreate {
            first_name: "A".into(),
            last_name: "B".into(),
            address: Address {
                street_number: "1".into(),
                street_name: "s".into(),
                city: "c".into(),
                state: "CA".into(),
                zip: "90028".into(),
            },
        })
        .unwrap();
    let a = bank
        .create_account(
            &c.record.id,
            &AccountCreate {
                account_type: kind,
                nickname: "n".into(),
                balance_cents: balance,
                credit_limit_cents: limit,
                rewards: None,
            },
        )
        .unwrap();
    (bank, a.record.id)
}

#[test]
fn review_r4_fixture_balance_overflow_is_refused_not_wrapped() {
    // A checking account at the top of the range, then a deposit that would overflow it.
    let (bank, acct) = fixture_with(AccountType::Checking, i64::MAX - 5, None);
    let d = bank
        .deposit(
            &acct,
            &DepositCreate {
                amount_cents: 10,
                description: None,
            },
        )
        .unwrap();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        bank.settle_transaction(&d.record.id)
    }));
    let r = r.expect("settling must not panic");
    assert!(
        r.is_err(),
        "an overflowing settlement must be refused: {r:?}"
    );
    let a = bank.get_account(&acct).unwrap().record;
    assert_eq!(a.balance_cents, i64::MAX - 5, "balance changed or wrapped");
    assert_eq!(
        bank.get_transaction(&d.record.id).unwrap().record.status,
        TransactionStatus::Pending
    );

    // A card with a huge limit, paid down below zero: available must not wrap.
    let (bank, card) = fixture_with(AccountType::CreditCard, 0, Some(i64::MAX));
    let d = bank
        .deposit(
            &card,
            &DepositCreate {
                amount_cents: 10,
                description: None,
            },
        )
        .unwrap();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        bank.settle_transaction(&d.record.id)
    }))
    .expect("settling must not panic");
    let a = bank.get_account(&card).unwrap().record;
    match r {
        Ok(_) => assert!(a.available_cents > 0, "available wrapped: {a:?}"),
        Err(_) => assert_eq!(a.balance_cents, 0),
    }
}

// ---- R2 and R3: affordability is never more lenient, and unknown rent is not coverage -----------

fn checking(sim: Simulation, available: i64) -> BankRecord<Account> {
    BankRecord::new(
        sim,
        serde_json::from_str::<Account>(&account_json(available)).unwrap(),
    )
}

#[test]
fn review_r2_affordability_is_never_more_lenient_than_the_ranking_constraint() {
    let sim = Simulation::FixtureBank;
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    // The ranking engine fails a property when upfront_cash_expected > opening_capital.
    let capital = s.upfront_cash_expected - 1;

    // A bill of materials far below the FF&E allowance lowers the planned move-in cash below
    // the scenario's upfront cash.
    let bom = BomTotals {
        bom_id: Uuid::from_u128(77),
        project_id: s.project_id,
        property_id: s.property_id,
        design_version_id: Uuid::from_u128(78),
        currency: "USD".into(),
        estimated_total_minor: 100,
        approved_total_minor: None,
        status: "DRAFT".into(),
    };
    let plan = plan_move_in_payments(&s, Some(&bom), sim, None).unwrap();
    assert!(plan.move_in_cash_expected_minor < s.upfront_cash_expected);
    let r = check_affordability(&checking(sim, capital), &plan, &s, None).unwrap();
    assert_eq!(
        r.verdict,
        AffordabilityVerdict::Short,
        "ranking fails this capital, so the sandbox check must not pass it: {}",
        r.headline
    );

    // A one-time cost line with no known payee is not simulated, but it is upfront cash.
    let mut s2 = s.clone();
    s2.line_items.push(CostLineItem {
        line_item_id: "broker:fee".into(),
        category: "BROKER".into(),
        description: "Broker fee".into(),
        kind: CostKind::OneTime,
        low: 2_000_000,
        expected: 2_000_000,
        high: 2_000_000,
        source_work_item_id: None,
        evidence_ids: vec![],
    });
    s2.opening_cost_low += 2_000_000;
    s2.opening_cost_expected += 2_000_000;
    s2.opening_cost_high += 2_000_000;
    s2.upfront_cash_expected += 2_000_000;
    let plan2 = plan_move_in_payments(&s2, None, sim, None).unwrap();
    assert!(plan2.move_in_cash_expected_minor < s2.upfront_cash_expected);
    let r = check_affordability(
        &checking(sim, s2.upfront_cash_expected - 1),
        &plan2,
        &s2,
        None,
    )
    .unwrap();
    assert_eq!(r.verdict, AffordabilityVerdict::Short, "{}", r.headline);
    assert!(r.required_expected_minor >= s2.upfront_cash_expected);
    assert!(r.required_high_minor >= r.required_expected_minor);

    // Exhaustively around the boundary for every golden scenario: whenever ranking fails a
    // capital amount, the sandbox check does not say it covers.
    let d = common::read_json("apps/backend/tests-golden/domain/part2_pipeline.json");
    for case in d["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        for addr in case["output"]["properties"].as_object().unwrap().keys() {
            let s = golden_scenario(name, addr, "36");
            let plan = plan_move_in_payments(
                &s,
                Some(&BomTotals {
                    project_id: s.project_id,
                    property_id: s.property_id,
                    ..bom.clone()
                }),
                sim,
                None,
            )
            .unwrap();
            for capital in [0, s.upfront_cash_expected / 2, s.upfront_cash_expected - 1] {
                let r = check_affordability(&checking(sim, capital), &plan, &s, None).unwrap();
                assert!(
                    !matches!(
                        r.verdict,
                        AffordabilityVerdict::CoversExpectedOnly
                            | AffordabilityVerdict::CoversHighEstimate
                    ),
                    "{name}/{addr}: {}",
                    r.headline
                );
            }
        }
    }
}

#[test]
fn review_r3_unknown_rent_is_never_a_coverage_verdict() {
    let sim = Simulation::NessieSandbox;
    let mut s = golden_scenario("demo", HOLLYWOOD, "36");
    for l in s
        .line_items
        .iter_mut()
        .filter(|l| l.kind == CostKind::Monthly)
    {
        l.low = 0;
        l.expected = 0;
        l.high = 0;
    }
    s.monthly_operating_cost = 0;
    s.assumptions
        .push("Asking rent unknown — monthly cost understated".into());
    let plan = plan_move_in_payments(&s, None, sim, None).unwrap();
    let r = check_affordability(&checking(sim, 1 << 40), &plan, &s, None).unwrap();
    assert!(
        !matches!(
            r.verdict,
            AffordabilityVerdict::CoversExpectedOnly | AffordabilityVerdict::CoversHighEstimate
        ),
        "rent is unknown, so coverage cannot be claimed: {}",
        r.headline
    );
    assert_eq!(r.verdict, AffordabilityVerdict::Unknown);
    assert!(r.headline.starts_with("Simulation only:"), "{}", r.headline);
    assert!(r.headline.contains("rent"), "{}", r.headline);
    // Short stays short: an unknown cost can only add to what is needed.
    let r = check_affordability(&checking(sim, 1), &plan, &s, None).unwrap();
    assert_eq!(r.verdict, AffordabilityVerdict::Short);
}

#[test]
fn review_intents_carry_only_plan_amounts() {
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    let plan = plan_move_in_payments(&s, None, Simulation::NessieSandbox, None).unwrap();
    let t = SandboxTargets {
        account_id: "acct_1".into(),
        landlord_merchant_id: Some("merch_l".into()),
        contractor_merchant_id: Some("merch_c".into()),
        supplier_merchant_id: Some("merch_s".into()),
        occupancy_bill_id: Some("bill_1".into()),
    };
    for p in &plan.payments {
        let i = plan.intent_for(&p.plan_item_id, &t, "0123abcd4567").unwrap();
        assert_eq!(i.amount_minor, p.amount_expected_minor);
        assert_eq!(i.action, p.approval_action);
        assert!(i.description.chars().count() <= 200);
        assert_eq!(
            i.approval_payload()["simulation"],
            "CAPITAL_ONE_NESSIE_SANDBOX"
        );
    }
    // An id that would change the request path is refused before any intent exists.
    let bad = SandboxTargets {
        account_id: "acct_1/../../data".into(),
        ..t.clone()
    };
    assert!(plan.intent_for("movein:security_deposit", &bad, "0123abcd4567").is_err());
    let bad = SandboxTargets {
        landlord_merchant_id: Some("merch?x=1".into()),
        ..t
    };
    assert!(plan.intent_for("movein:security_deposit", &bad, "0123abcd4567").is_err());
}
