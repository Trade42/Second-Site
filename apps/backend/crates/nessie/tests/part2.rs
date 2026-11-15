//! The crate changes the server integration (Nessie part 2) relies on:
//! - D3: the plan item id and an attempt id are part of the approved payload and start the
//!   Nessie description;
//! - B1: an answer about another object, kind, merchant, bill, account, payee or payer than the
//!   approved one is MAY_EXIST, never a recorded payment;
//! - B2: only 400, 401, 403, 404, 409, 422 and 429 are refusals; any other 4xx is MAY_EXIST;
//! - M1: a cost group whose expected total is $0 is listed in `not_simulated`, and its high
//!   estimate counts in the high move-in cash.

mod common;

use std::collections::BTreeSet;

use chrono::Utc;
use common::{approved, golden_scenario, json, problem, MockServer, HOLLYWOOD};
use serde_json::{json, Value};
use ss_domain::contracts::CostKind;
use ss_domain::integrations::base::RetryPolicy;
use ss_nessie::approval::{
    description_prefix, new_attempt_id, validate_attempt_id, PaymentOperation,
};
use ss_nessie::config::ApiKey;
use ss_nessie::models::{AccountCreate, AccountType, Address, CustomerCreate};
use ss_nessie::plan::{stand_in_merchant, ITEM_FIRST_MONTH, ITEM_OCCUPANCY_BILL};
use ss_nessie::{
    plan_move_in_payments, ApprovalAction, AttemptOutcome, AuthorizationError, FixtureBank,
    LiveNessieBank, NessieBank, PayeeRole, PaymentAuthorization, PaymentIntent, SandboxTargets,
    Simulation,
};
use uuid::Uuid;

const ATTEMPT: &str = "a1b2c3d4e5f6";

fn targets() -> SandboxTargets {
    SandboxTargets {
        account_id: "acct_1".into(),
        landlord_merchant_id: Some("merch_l".into()),
        contractor_merchant_id: Some("merch_c".into()),
        supplier_merchant_id: Some("merch_s".into()),
        occupancy_bill_id: Some("bill_1".into()),
    }
}

fn auth(i: &PaymentIntent) -> PaymentAuthorization {
    PaymentAuthorization::verify(&approved(i), i.clone(), Utc::now()).unwrap()
}

// ---- D3 -----------------------------------------------------------------------------------------

#[test]
fn d3_every_intent_names_its_plan_item_and_attempt_in_the_payload_and_description() {
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    for sim in [Simulation::FixtureBank, Simulation::NessieSandbox] {
        let plan = plan_move_in_payments(&s, None, sim, None).unwrap();
        assert!(!plan.payments.is_empty());
        for p in &plan.payments {
            let i = plan
                .intent_for(&p.plan_item_id, &targets(), ATTEMPT)
                .unwrap();
            let prefix = description_prefix(&p.plan_item_id, ATTEMPT);
            assert!(i.description.starts_with(&prefix), "{}", i.description);
            assert!(i.description.starts_with("SIMULATION"));
            assert!(i.description.chars().count() <= 200);
            assert!(i.description.ends_with(&p.label), "{}", i.description);
            let payload = i.approval_payload();
            assert_eq!(payload["attempt_id"], ATTEMPT);
            assert_eq!(payload["plan_item_id"], p.plan_item_id.as_str());
            assert_eq!(payload["description"], i.description.as_str());
            assert!(i.summary().contains(ATTEMPT), "{}", i.summary());

            // Another attempt id is another payload: an approval of one cannot send the other.
            let other = plan
                .intent_for(&p.plan_item_id, &targets(), "0f0f0f0f")
                .unwrap();
            assert_ne!(other.approval_payload(), payload);
            assert_eq!(
                PaymentAuthorization::verify(&approved(&i), other, Utc::now()).unwrap_err(),
                AuthorizationError::PayloadMismatch
            );
        }
    }
}

#[test]
fn d3_attempt_ids_are_short_lower_case_hex() {
    for good in ["0123abcd", "a1b2c3d4e5f6", "0123456789abcdef"] {
        assert!(validate_attempt_id(good).is_ok(), "{good}");
    }
    for bad in [
        "",
        "abc",
        "0123abc",
        "0123456789abcdef0",
        "A1B2C3D4",
        "a1b2c3d4 ",
        "a1b2-c3d4",
        "ghijklmn",
        "a1b2c3d4\n",
    ] {
        assert!(validate_attempt_id(bad).is_err(), "{bad:?}");
    }
    let a = new_attempt_id();
    let b = new_attempt_id();
    assert_eq!(a.len(), 12);
    assert!(validate_attempt_id(&a).is_ok(), "{a}");
    assert_ne!(a, b);

    let s = golden_scenario("demo", HOLLYWOOD, "36");
    let plan = plan_move_in_payments(&s, None, Simulation::FixtureBank, None).unwrap();
    let item = &plan.payments[0].plan_item_id;
    for bad in ["", "XYZ", "0123abcd/../x"] {
        assert!(plan.intent_for(item, &targets(), bad).is_err(), "{bad:?}");
    }
}

#[test]
fn d3_an_intent_without_the_prefix_or_with_a_bad_plan_item_is_refused() {
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    let plan = plan_move_in_payments(&s, None, Simulation::FixtureBank, None).unwrap();
    let good = plan
        .intent_for(&plan.payments[0].plan_item_id, &targets(), ATTEMPT)
        .unwrap();
    let refused = |i: PaymentIntent| {
        matches!(
            PaymentAuthorization::verify(&approved(&i), i, Utc::now()),
            Err(AuthorizationError::InvalidIntent(_))
        )
    };
    assert!(refused(PaymentIntent {
        description: "SIMULATION (Second Site plan): deposit".into(),
        ..good.clone()
    }));
    assert!(refused(PaymentIntent {
        attempt_id: "0f0f0f0f".into(),
        ..good.clone()
    }));
    assert!(refused(PaymentIntent {
        attempt_id: "NOT-HEX!".into(),
        ..good.clone()
    }));
    let spaced = "movein:security deposit".to_owned();
    assert!(refused(PaymentIntent {
        description: format!("{}x", description_prefix(&spaced, ATTEMPT)),
        plan_item_id: spaced,
        ..good.clone()
    }));
    let long = "x".repeat(65);
    assert!(refused(PaymentIntent {
        description: format!("{}x", description_prefix(&long, ATTEMPT)),
        plan_item_id: long,
        ..good.clone()
    }));
    // A long label is cut, the prefix never.
    let long_label = PaymentIntent {
        description: format!(
            "{}{}",
            description_prefix(&good.plan_item_id, ATTEMPT),
            "y".repeat(300)
        ),
        ..good.clone()
    };
    assert!(refused(long_label));
}

#[test]
fn d3_the_nessie_transaction_description_names_the_plan_item_and_attempt() {
    let bank = FixtureBank::new();
    let c = bank
        .create_customer(&CustomerCreate {
            first_name: "SIMULATED".into(),
            last_name: "OWNER".into(),
            address: Address {
                street_number: "1".into(),
                street_name: "Main St".into(),
                city: "Los Angeles".into(),
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
                nickname: "SIMULATION".into(),
                balance_cents: 100_000_000,
                credit_limit_cents: None,
                rewards: None,
            },
        )
        .unwrap();
    let mut t = SandboxTargets {
        account_id: a.record.id.clone(),
        ..SandboxTargets::default()
    };
    for (role, slot) in [
        (PayeeRole::Landlord, &mut t.landlord_merchant_id),
        (PayeeRole::Contractor, &mut t.contractor_merchant_id),
        (PayeeRole::FurnishingSupplier, &mut t.supplier_merchant_id),
    ] {
        *slot = Some(
            bank.create_merchant(&stand_in_merchant(role))
                .unwrap()
                .record
                .id,
        );
    }
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    let plan = plan_move_in_payments(&s, None, bank.simulation(), None).unwrap();
    let deposit = &plan.payments[0];
    let i = plan.intent_for(&deposit.plan_item_id, &t, ATTEMPT).unwrap();
    let rec = bank.execute(auth(&i)).unwrap();
    assert_eq!(rec.attempt_id, ATTEMPT);
    let listed = bank.list_account_transactions(&a.record.id, 10).unwrap();
    let tx = listed
        .record
        .iter()
        .find(|x| Some(x.id.as_str()) == rec.nessie_transaction_id.as_deref())
        .unwrap();
    let d = tx.description.as_deref().unwrap();
    assert!(
        d.contains(&deposit.plan_item_id) && d.contains(ATTEMPT),
        "{d}"
    );
}

// ---- B1 -----------------------------------------------------------------------------------------

fn live(url: &str) -> LiveNessieBank {
    LiveNessieBank::with_policy(
        url,
        ApiKey::new("nsk_TEST_part2_5e4d3c2b1a0f").unwrap(),
        RetryPolicy {
            attempts: 1,
            backoff_s: 0.0,
            timeout_s: 5.0,
        },
    )
}

fn sandbox_intent(op: PaymentOperation, amount: i64) -> PaymentIntent {
    let (item, action) = match op {
        PaymentOperation::Purchase { .. } => ("buildout:renovation", ApprovalAction::Purchase),
        PaymentOperation::CreateRecurringBill { .. } => {
            (ITEM_OCCUPANCY_BILL, ApprovalAction::FinancialCommitment)
        }
        PaymentOperation::PayBill { .. } => (ITEM_FIRST_MONTH, ApprovalAction::FinancialCommitment),
    };
    PaymentIntent {
        simulation: Simulation::NessieSandbox,
        project_id: Uuid::from_u128(1),
        property_id: Uuid::from_u128(2),
        scenario_id: Uuid::from_u128(3),
        plan_item_id: item.into(),
        attempt_id: ATTEMPT.into(),
        action,
        operation: op,
        amount_minor: amount,
        currency: "USD".into(),
        description: format!("{}test", description_prefix(item, ATTEMPT)),
    }
}

fn purchase_op() -> PaymentOperation {
    PaymentOperation::Purchase {
        account_id: "acct_1".into(),
        merchant_id: "merch_1".into(),
    }
}

fn pay_bill_op() -> PaymentOperation {
    PaymentOperation::PayBill {
        account_id: "acct_1".into(),
        bill_id: "bill_1".into(),
    }
}

fn bill_op() -> PaymentOperation {
    PaymentOperation::CreateRecurringBill {
        account_id: "acct_1".into(),
        payee: "SIMULATED LANDLORD (Second Site stand-in)".into(),
        nickname: "SIMULATION: monthly rent + NNN".into(),
        recurring_day: 1,
    }
}

fn tx(kind: &str, amount: i64, extra: Value) -> Value {
    let mut v = json!({
        "id": "txn_1", "kind": kind, "status": "pending", "medium": "balance",
        "amount_cents": amount, "settle_at": "2026-11-14T12:00:10Z",
        "created_at": "2026-11-14T12:00:00Z", "updated_at": "2026-11-14T12:00:00Z"
    });
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            o.insert(k.clone(), x.clone());
        }
    }
    v
}

fn bill(account: &str, payee: &str) -> Value {
    json!({
        "id": "bill_1", "account_id": account, "status": "recurring", "payee": payee,
        "creation_date": "2026-11-14", "upcoming_payment_date": "2026-12-01",
        "payment_amount": 6900.0, "created_at": "t", "updated_at": "t"
    })
}

/// Executes `intent` against a server that answers `answer` once.
fn outcome(intent: PaymentIntent, answer: Value) -> Result<(), AttemptOutcome> {
    let srv = MockServer::start(vec![json(200, answer)]);
    let bank = live(&srv.url);
    let r = bank.execute(auth(&intent));
    assert_eq!(srv.requests().len(), 1, "sent exactly once");
    r.map(|rec| {
        assert_eq!(rec.attempt_id, ATTEMPT);
    })
    .map_err(|e| e.outcome)
}

#[test]
fn b1_an_answer_about_another_object_is_may_exist() {
    let p = sandbox_intent(purchase_op(), 1234);
    let good = json!({"payer_id": "acct_1", "merchant_id": "merch_1"});
    assert_eq!(outcome(p.clone(), tx("purchase", 1234, good)), Ok(()));
    for (why, kind, extra) in [
        (
            "other merchant",
            "purchase",
            json!({"payer_id": "acct_1", "merchant_id": "merch_2"}),
        ),
        ("no merchant", "purchase", json!({"payer_id": "acct_1"})),
        (
            "other payer",
            "purchase",
            json!({"payer_id": "acct_2", "merchant_id": "merch_1"}),
        ),
        ("no payer", "purchase", json!({"merchant_id": "merch_1"})),
        (
            "other kind",
            "withdrawal",
            json!({"payer_id": "acct_1", "merchant_id": "merch_1"}),
        ),
        (
            "unknown kind",
            "zzz",
            json!({"payer_id": "acct_1", "merchant_id": "merch_1"}),
        ),
    ] {
        assert_eq!(
            outcome(p.clone(), tx(kind, 1234, extra)),
            Err(AttemptOutcome::MayExist),
            "{why}"
        );
    }

    let pay = sandbox_intent(pay_bill_op(), 690_000);
    let good = json!({"payer_id": "acct_1", "bill_id": "bill_1"});
    assert_eq!(
        outcome(pay.clone(), tx("bill_payment", 690_000, good)),
        Ok(())
    );
    for (why, kind, extra) in [
        (
            "other bill",
            "bill_payment",
            json!({"payer_id": "acct_1", "bill_id": "bill_2"}),
        ),
        ("no bill", "bill_payment", json!({"payer_id": "acct_1"})),
        (
            "other payer",
            "bill_payment",
            json!({"payer_id": "acct_9", "bill_id": "bill_1"}),
        ),
        (
            "purchase kind",
            "purchase",
            json!({"payer_id": "acct_1", "bill_id": "bill_1"}),
        ),
    ] {
        assert_eq!(
            outcome(pay.clone(), tx(kind, 690_000, extra)),
            Err(AttemptOutcome::MayExist),
            "{why}"
        );
    }

    let b = sandbox_intent(bill_op(), 690_000);
    assert_eq!(
        outcome(
            b.clone(),
            bill("acct_1", "SIMULATED LANDLORD (Second Site stand-in)")
        ),
        Ok(())
    );
    assert_eq!(
        outcome(
            b.clone(),
            bill("acct_2", "SIMULATED LANDLORD (Second Site stand-in)")
        ),
        Err(AttemptOutcome::MayExist),
        "other account"
    );
    assert_eq!(
        outcome(b, bill("acct_1", "Someone else")),
        Err(AttemptOutcome::MayExist),
        "other payee"
    );
}

#[test]
fn b1_the_fixture_bank_pays_a_bill_from_its_own_account_so_another_approved_payer_is_may_exist() {
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
    let open = |n: &str| {
        bank.create_account(
            &c.record.id,
            &AccountCreate {
                account_type: AccountType::Checking,
                nickname: n.into(),
                balance_cents: 10_000_000,
                credit_limit_cents: None,
                rewards: None,
            },
        )
        .unwrap()
        .record
        .id
    };
    let (a1, a2) = (open("one"), open("two"));
    let mut bill_intent = sandbox_intent(bill_op(), 690_000);
    bill_intent.simulation = Simulation::FixtureBank;
    bill_intent.operation = PaymentOperation::CreateRecurringBill {
        account_id: a1.clone(),
        payee: "SIMULATED LANDLORD (Second Site stand-in)".into(),
        nickname: "rent".into(),
        recurring_day: 1,
    };
    let bill_id = bank
        .execute(auth(&bill_intent))
        .unwrap()
        .nessie_bill_id
        .unwrap();
    let mut pay = sandbox_intent(pay_bill_op(), 690_000);
    pay.simulation = Simulation::FixtureBank;
    pay.operation = PaymentOperation::PayBill {
        account_id: a2,
        bill_id: bill_id.clone(),
    };
    let err = bank.execute(auth(&pay)).unwrap_err();
    assert_eq!(err.outcome, AttemptOutcome::MayExist, "{err}");
    // It is MAY_EXIST and not "nothing happened": the bank did create the payment.
    let held = bank.list_account_transactions(&a1, 10).unwrap();
    assert!(held
        .record
        .iter()
        .any(|t| t.bill_id.as_deref() == Some(bill_id.as_str())));

    pay.operation = PaymentOperation::PayBill {
        account_id: a1,
        bill_id,
    };
    assert!(bank.execute(auth(&pay)).is_ok());
}

// ---- B2 -----------------------------------------------------------------------------------------

#[test]
fn b2_only_documented_refusals_release_the_approval() {
    let p = sandbox_intent(purchase_op(), 1234);
    for status in [400u16, 401, 403, 404, 409, 422, 429] {
        let srv = MockServer::start(vec![problem(status, "refused", "Refused", "no")]);
        let err = live(&srv.url).execute(auth(&p)).unwrap_err();
        assert_eq!(err.outcome, AttemptOutcome::Refused, "{status}");
        assert_eq!(err.error.http_status(), Some(status));
        assert!(err.error.is_client_refusal());
    }
    for status in [402u16, 405, 408, 410, 413, 418, 425, 451, 499] {
        let srv = MockServer::start(vec![problem(status, "odd", "Odd", "?")]);
        let err = live(&srv.url).execute(auth(&p)).unwrap_err();
        assert_eq!(err.outcome, AttemptOutcome::MayExist, "{status}");
        assert!(!err.error.is_client_refusal(), "{status}");
        assert_eq!(srv.requests().len(), 1, "a POST is never resent ({status})");
    }
}

#[test]
fn b2_a_may_exist_attempt_keeps_the_approval_spent_in_process() {
    let p = sandbox_intent(purchase_op(), 1234);
    let srv = MockServer::start(vec![problem(408, "timeout", "Request Timeout", "proxy")]);
    let bank = live(&srv.url);
    let ev = approved(&p);
    let first = PaymentAuthorization::verify(&ev, p.clone(), Utc::now()).unwrap();
    assert_eq!(
        bank.execute(first).unwrap_err().outcome,
        AttemptOutcome::MayExist
    );
    let again = PaymentAuthorization::verify(&ev, p, Utc::now()).unwrap();
    let err = bank.execute(again).unwrap_err();
    assert_eq!(err.outcome, AttemptOutcome::NotSent);
    assert_eq!(srv.requests().len(), 1);
}

// ---- M1 -----------------------------------------------------------------------------------------

#[test]
fn m1_a_group_with_a_zero_expected_total_is_listed_and_its_high_counted() {
    let base = golden_scenario("demo", HOLLYWOOD, "36");
    let before = plan_move_in_payments(&base, None, Simulation::FixtureBank, None).unwrap();
    assert_eq!(before.zero_expected_high_minor, 0);

    type InGroup = fn(&ss_domain::contracts::CostLineItem) -> bool;
    let groups: [(&str, InGroup, &str); 3] = [
        (
            "security deposit",
            |l| l.kind == CostKind::RefundableDeposit,
            "movein:security_deposit",
        ),
        (
            "renovation",
            |l| l.kind == CostKind::OneTime && l.line_item_id.starts_with("work:"),
            "buildout:renovation",
        ),
        (
            "furnishing",
            |l| l.kind == CostKind::OneTime && l.line_item_id.starts_with("ffe:"),
            "buildout:furnishing",
        ),
    ];
    for (what, in_group, item) in groups {
        let mut s = base.clone();
        let mut ids = BTreeSet::new();
        let mut high = 0i64;
        for l in s.line_items.iter_mut().filter(|l| in_group(l)) {
            l.low = 0;
            l.expected = 0;
            l.high = 77_700;
            high += l.high;
            ids.insert(l.line_item_id.clone());
        }
        assert!(!ids.is_empty(), "{what}");
        let plan = plan_move_in_payments(&s, None, Simulation::FixtureBank, None).unwrap();
        assert!(plan.payment(item).is_none(), "{what}");
        let listed: BTreeSet<String> = plan
            .not_simulated
            .iter()
            .filter(|l| l.reason.contains(&format!("expected {what} total is $0")))
            .map(|l| l.line_item_id.clone())
            .collect();
        assert_eq!(listed, ids, "{what}");
        assert_eq!(plan.zero_expected_high_minor, high, "{what}");
        assert_eq!(
            plan.move_in_cash_high_minor,
            plan.move_in_payments_high_minor + plan.reserves_high_minor + high,
            "{what}"
        );

        // Every line still lands in exactly one place.
        let mut seen: Vec<String> = Vec::new();
        for p in &plan.payments {
            if p.plan_item_id != ITEM_FIRST_MONTH {
                seen.extend(p.source_line_item_ids.iter().cloned());
            }
        }
        for r in &plan.reserves {
            seen.extend(r.source_line_item_ids.iter().cloned());
        }
        seen.extend(plan.not_simulated.iter().map(|l| l.line_item_id.clone()));
        let mut all: Vec<String> = s
            .line_items
            .iter()
            .map(|l| l.line_item_id.clone())
            .collect();
        seen.sort();
        all.sort();
        assert_eq!(seen, all, "{what}");
    }
}
