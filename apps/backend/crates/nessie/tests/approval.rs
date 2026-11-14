//! The approval gate: every check `PaymentAuthorization::verify` makes at the Nessie boundary.

mod common;

use chrono::Duration;
use common::{approved, t};
use serde_json::json;
use ss_nessie::approval::PaymentOperation;
use ss_nessie::{
    ApprovalAction, AuthorizationError, PaymentAuthorization, PaymentIntent, Simulation,
    FIXTURE_BANK_DISCLAIMER, NESSIE_SANDBOX_DISCLAIMER,
};
use uuid::Uuid;

fn purchase(sim: Simulation) -> PaymentIntent {
    PaymentIntent {
        simulation: sim,
        project_id: Uuid::from_u128(1),
        property_id: Uuid::from_u128(2),
        scenario_id: Uuid::from_u128(3),
        plan_item_id: "movein:security_deposit".into(),
        attempt_id: "0123abcd4567".into(),
        action: ApprovalAction::FinancialCommitment,
        operation: PaymentOperation::Purchase {
            account_id: "acct_1".into(),
            merchant_id: "merch_1".into(),
        },
        amount_minor: 1_200_000,
        currency: "USD".into(),
        description: "SIMULATION (Second Site plan item movein:security_deposit, attempt 0123abcd4567): Refundable security deposit (simulated)"
            .into(),
    }
}

#[test]
fn approved_matching_approval_authorizes() {
    let intent = purchase(Simulation::FixtureBank);
    let ev = approved(&intent);
    let now = t(2026, 11, 14, 12);
    let mut ev = ev;
    ev.expires_at = now + Duration::seconds(1);
    let auth = PaymentAuthorization::verify(&ev, intent.clone(), now).unwrap();
    assert_eq!(auth.approval_id(), ev.approval_id);
    assert_eq!(auth.intent(), &intent);
}

#[test]
fn every_check_refuses() {
    let intent = purchase(Simulation::FixtureBank);
    let now = t(2026, 11, 14, 12);
    let good = {
        let mut e = approved(&intent);
        e.expires_at = now + Duration::hours(1);
        e
    };
    let refuse = |e: &ss_nessie::ApprovalEvidence, i: PaymentIntent| {
        PaymentAuthorization::verify(e, i, now).unwrap_err()
    };

    for status in [
        "PENDING",
        "REJECTED",
        "EXPIRED",
        "CANCELLED",
        "EXECUTED",
        "approved",
    ] {
        let mut e = good.clone();
        e.approval_status = status.into();
        assert_eq!(
            refuse(&e, intent.clone()),
            AuthorizationError::NotApproved(status.into())
        );
    }
    for action in ["PURCHASE", "SEND_MESSAGE", "APPROVE_BOM"] {
        let mut e = good.clone();
        e.action_type = action.into();
        assert!(matches!(
            refuse(&e, intent.clone()),
            AuthorizationError::WrongAction { .. }
        ));
    }
    let mut e = good.clone();
    e.project_id = Uuid::from_u128(99);
    assert_eq!(refuse(&e, intent.clone()), AuthorizationError::WrongProject);

    let mut e = good.clone();
    e.expires_at = now;
    assert_eq!(refuse(&e, intent.clone()), AuthorizationError::Expired);

    // Any change to the approved payload, however small, is a mismatch.
    let mutations: Vec<(&str, serde_json::Value)> = vec![
        ("amount_minor", json!(1_200_001)),
        ("amount_minor", json!(1_200_000.0)),
        ("merchant_id", json!("merch_2")),
        ("account_id", json!("acct_2")),
        ("simulation", json!("CAPITAL_ONE_NESSIE_SANDBOX")),
        ("description", json!("SIMULATION other")),
        ("plan_item_id", json!("buildout:renovation")),
        ("extra", json!(true)),
    ];
    for (k, v) in mutations {
        let mut e = good.clone();
        e.action_payload.insert(k.into(), v);
        assert_eq!(
            refuse(&e, intent.clone()),
            AuthorizationError::PayloadMismatch,
            "{k}"
        );
    }
    let mut e = good.clone();
    e.action_payload.remove("disclaimer");
    assert_eq!(
        refuse(&e, intent.clone()),
        AuthorizationError::PayloadMismatch
    );

    // An intent the bank could never send is refused before the approval is even compared.
    let bad_intents = [
        PaymentIntent {
            currency: "EUR".into(),
            ..intent.clone()
        },
        PaymentIntent {
            amount_minor: 0,
            ..intent.clone()
        },
        PaymentIntent {
            description: "Security deposit".into(),
            ..intent.clone()
        },
        PaymentIntent {
            description: format!("SIMULATION {}", "x".repeat(200)),
            ..intent.clone()
        },
        PaymentIntent {
            operation: PaymentOperation::Purchase {
                account_id: "acct_1/../x".into(),
                merchant_id: "merch_1".into(),
            },
            ..intent.clone()
        },
        PaymentIntent {
            action: ApprovalAction::Purchase,
            operation: PaymentOperation::PayBill {
                account_id: "acct_1".into(),
                bill_id: "bill_1".into(),
            },
            ..intent.clone()
        },
        PaymentIntent {
            operation: PaymentOperation::CreateRecurringBill {
                account_id: "acct_1".into(),
                payee: "p".into(),
                nickname: "n".into(),
                recurring_day: 0,
            },
            ..intent.clone()
        },
    ];
    for bad in bad_intents {
        let e = approved(&bad);
        assert!(
            matches!(
                PaymentAuthorization::verify(&e, bad.clone(), now),
                Err(AuthorizationError::InvalidIntent(_))
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn payload_names_the_simulation_and_every_sent_field() {
    let intent = purchase(Simulation::NessieSandbox);
    let p = intent.approval_payload();
    let mut keys: Vec<&str> = p.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "account_id",
            "amount_minor",
            "attempt_id",
            "currency",
            "description",
            "disclaimer",
            "integration",
            "merchant_id",
            "moves_money",
            "operation",
            "plan_item_id",
            "project_id",
            "property_id",
            "scenario_id",
            "simulation"
        ]
    );
    assert_eq!(p["simulation"], "CAPITAL_ONE_NESSIE_SANDBOX");
    assert_eq!(p["disclaimer"], NESSIE_SANDBOX_DISCLAIMER);
    assert_eq!(p["integration"], "capital_one_nessie_v2");
    assert_eq!(p["operation"], "purchase");
    assert_eq!(p["moves_money"], true);
    assert_eq!(p["amount_minor"], 1_200_000);
    assert!(p["amount_minor"].is_i64());

    let fixture = purchase(Simulation::FixtureBank).approval_payload();
    assert_eq!(fixture["simulation"], "FIXTURE_IN_MEMORY_BANK");
    assert_eq!(fixture["disclaimer"], FIXTURE_BANK_DISCLAIMER);

    let bill = PaymentIntent {
        operation: PaymentOperation::CreateRecurringBill {
            account_id: "acct_1".into(),
            payee: "SIMULATED LANDLORD".into(),
            nickname: "rent".into(),
            recurring_day: 1,
        },
        ..intent.clone()
    };
    let p = bill.approval_payload();
    assert_eq!(p["operation"], "create_recurring_bill");
    assert_eq!(p["moves_money"], false);
    assert_eq!(p["recurring_day"], 1);
    assert!(bill.summary().contains("no money moves"));

    let s = intent.summary();
    assert!(s.starts_with("Simulation only:"), "{s}");
    assert!(s.contains("$12,000.00"));
    assert!(s.contains("Capital One Nessie sandbox (mock money)"));
    assert!(s.contains("Nothing real is paid or committed"));
    assert!(purchase(Simulation::FixtureBank)
        .summary()
        .contains("fixture data"));
}

#[test]
fn disclaimers_say_what_the_money_is() {
    assert!(NESSIE_SANDBOX_DISCLAIMER.contains("Capital One Nessie sandbox"));
    assert!(NESSIE_SANDBOX_DISCLAIMER.contains("mock money"));
    assert!(NESSIE_SANDBOX_DISCLAIMER.contains("not a real bank balance"));
    assert!(FIXTURE_BANK_DISCLAIMER.contains("Fixture data"));
    assert!(FIXTURE_BANK_DISCLAIMER.contains("not the Capital One Nessie sandbox"));
    assert!(FIXTURE_BANK_DISCLAIMER.contains("not a bank"));
    assert_eq!(
        Simulation::NessieSandbox.disclaimer(),
        NESSIE_SANDBOX_DISCLAIMER
    );
    assert_eq!(
        Simulation::FixtureBank.disclaimer(),
        FIXTURE_BANK_DISCLAIMER
    );
    assert_eq!(
        serde_json::to_value(Simulation::FixtureBank).unwrap(),
        "FIXTURE_IN_MEMORY_BANK"
    );
    assert_eq!(
        Simulation::from_wire("CAPITAL_ONE_NESSIE_SANDBOX"),
        Some(Simulation::NessieSandbox)
    );
    assert_eq!(Simulation::from_wire("LIVE"), None);
}
