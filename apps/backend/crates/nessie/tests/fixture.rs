//! The fixture bank: labels, documented balance behaviour, and the whole move-in plan executed
//! through approvals.

mod common;

use chrono::Utc;
use common::{approved, golden_scenario, HOLLYWOOD};
use ss_core::IntegrationStatus;
use ss_domain::integrations::base::ProviderErrorKind;
use ss_nessie::approval::PaymentOperation;
use ss_nessie::config::{ApiKey, NessieConfig, ProviderMode};
use ss_nessie::models::*;
use ss_nessie::plan::{stand_in_merchant, ITEM_FIRST_MONTH, ITEM_OCCUPANCY_BILL};
use ss_nessie::{
    check_affordability, plan_move_in_payments, select_bank, AffordabilityVerdict, ApprovalAction,
    AttemptOutcome, AuthorizationError, FixtureBank, NessieBank, NessieError, PayeeRole,
    PaymentAuthorization, PaymentIntent, SandboxTargets, SimulatedOutcome, Simulation,
    FIXTURE_BANK_DISCLAIMER,
};
use uuid::Uuid;

fn addr() -> Address {
    Address {
        street_number: "6922".into(),
        street_name: "W Hollywood Blvd".into(),
        city: "Los Angeles".into(),
        state: "CA".into(),
        zip: "90028".into(),
    }
}

fn owner(bank: &FixtureBank, balance: i64, kind: AccountType) -> (String, String) {
    let c = bank
        .create_customer(&CustomerCreate {
            first_name: "Simulated".into(),
            last_name: "Owner".into(),
            address: addr(),
        })
        .unwrap();
    let a = bank
        .create_account(
            &c.record.id,
            &AccountCreate {
                account_type: kind,
                nickname: "Opening fund".into(),
                balance_cents: balance,
                credit_limit_cents: None,
                rewards: None,
            },
        )
        .unwrap();
    (c.record.id, a.record.id)
}

fn merchant(bank: &FixtureBank, role: PayeeRole) -> String {
    bank.create_merchant(&stand_in_merchant(role))
        .unwrap()
        .record
        .id
}

fn purchase_intent(account: &str, merchant: &str, amount: i64) -> PaymentIntent {
    PaymentIntent {
        simulation: Simulation::FixtureBank,
        project_id: Uuid::from_u128(1),
        property_id: Uuid::from_u128(2),
        scenario_id: Uuid::from_u128(3),
        plan_item_id: "buildout:renovation".into(),
        attempt_id: "0123abcd4567".into(),
        action: ApprovalAction::Purchase,
        operation: PaymentOperation::Purchase {
            account_id: account.into(),
            merchant_id: merchant.into(),
        },
        amount_minor: amount,
        currency: "USD".into(),
        description:
            "SIMULATION (Second Site plan item buildout:renovation, attempt 0123abcd4567): test"
                .into(),
    }
}

fn authorize(intent: &PaymentIntent) -> PaymentAuthorization {
    PaymentAuthorization::verify(&approved(intent), intent.clone(), Utc::now()).unwrap()
}

#[test]
fn every_record_is_labelled_fixture_data() {
    let bank = FixtureBank::new();
    assert_eq!(bank.simulation(), Simulation::FixtureBank);
    let c = bank
        .create_customer(&CustomerCreate {
            first_name: "A".into(),
            last_name: "B".into(),
            address: addr(),
        })
        .unwrap();
    assert_eq!(c.simulation, Simulation::FixtureBank);
    assert_eq!(c.disclaimer, FIXTURE_BANK_DISCLAIMER);
    assert!(c.record.id.starts_with("fixture_cust_"));
    let (_, acct) = owner(&bank, 100, AccountType::Checking);
    let a = bank.get_account(&acct).unwrap();
    assert!(a.record.id.starts_with("fixture_acct_"));
    assert!(a
        .record
        .account_number
        .as_deref()
        .unwrap()
        .starts_with("FIXTURE-"));
    let v = serde_json::to_value(&a).unwrap();
    assert_eq!(v["simulation"], "FIXTURE_IN_MEMORY_BANK");
    assert_eq!(v["disclaimer"], FIXTURE_BANK_DISCLAIMER);
    assert_eq!(v["record"]["id"], acct.as_str());

    let st = bank.status();
    assert_eq!(st.provider, "nessie");
    assert_eq!(st.mode, "fixture");
    assert_eq!(st.status, IntegrationStatus::Mock);
    assert!(st.note.starts_with("FIXTURE: "));
    assert!(st.note.contains("not the Capital One Nessie sandbox"));
    assert!(bank.verify().is_ok());

    // Errors name the fixture provider.
    let e = bank.get_account("fixture_acct_9999").unwrap_err();
    assert_eq!(e.kind(), Some(ProviderErrorKind::NotFound));
    assert!(
        e.to_string().starts_with("[nessie_fixture] NOT_FOUND"),
        "{e}"
    );
}

#[test]
fn selection_and_status_follow_the_configuration() {
    let fixture = select_bank(&NessieConfig::default());
    assert_eq!(fixture.simulation(), Simulation::FixtureBank);
    assert_eq!(fixture.status().status, IntegrationStatus::Mock);

    let live_no_key = NessieConfig {
        mode: ProviderMode::Live,
        ..NessieConfig::default()
    };
    let b = select_bank(&live_no_key);
    assert_eq!(b.simulation(), Simulation::FixtureBank);
    let st = b.status();
    assert_eq!(st.status, IntegrationStatus::Blocked);
    assert_eq!(st.mode, "live");
    assert!(!st.credentials_present);
    assert!(st.note.starts_with("NOT_CONFIGURED: "), "{}", st.note);

    let key_but_fixture = NessieConfig {
        api_key: ApiKey::new("nsk_unused"),
        ..NessieConfig::default()
    };
    let b = select_bank(&key_but_fixture);
    assert_eq!(b.simulation(), Simulation::FixtureBank);
    assert!(b.status().credentials_present);
    assert_eq!(b.status().status, IntegrationStatus::Mock);

    // Live with a key: the live client, not yet verified (no network at construction).
    let live = NessieConfig {
        mode: ProviderMode::Live,
        api_key: ApiKey::new("nsk_unused"),
        base_url: "http://127.0.0.1:9".into(),
        ..NessieConfig::default()
    };
    let b = select_bank(&live);
    assert_eq!(b.simulation(), Simulation::NessieSandbox);
    let st = b.status();
    assert_eq!(st.status, IntegrationStatus::Partial);
    assert!(st.note.starts_with("CONFIGURED_UNVERIFIED: "));
    assert!(!format!("{b:?}").contains("nsk_unused"));
}

#[test]
fn holds_settlement_and_cancellation() {
    let bank = FixtureBank::new();
    let (_, acct) = owner(&bank, 10_000, AccountType::Checking);
    let m = merchant(&bank, PayeeRole::Contractor);

    let rec = bank
        .execute(authorize(&purchase_intent(&acct, &m, 2_500)))
        .unwrap();
    assert_eq!(rec.outcome, SimulatedOutcome::SimulatedPending);
    assert_eq!(rec.provider_status, "pending");
    assert!(rec.note.contains("pending"));
    assert!(rec.note.contains("nothing has been paid"), "{}", rec.note);
    let a = bank.get_account(&acct).unwrap().record;
    assert_eq!(
        (a.balance_cents, a.pending_debits_cents, a.available_cents),
        (10_000, Some(2_500), 7_500)
    );

    let tx_id = rec.nessie_transaction_id.clone().unwrap();
    let settled = bank.settle_transaction(&tx_id).unwrap();
    assert_eq!(settled.record.status, TransactionStatus::Completed);
    let a = bank.get_account(&acct).unwrap().record;
    assert_eq!(
        (a.balance_cents, a.pending_debits_cents, a.available_cents),
        (7_500, Some(0), 7_500)
    );
    let refreshed = rec.refreshed(&settled).unwrap();
    assert_eq!(refreshed.outcome, SimulatedOutcome::SimulatedCompleted);
    assert!(refreshed
        .note
        .contains("reported the simulated purchase of $25.00 as completed"));
    assert!(refreshed.note.contains("nothing real was paid"));
    // Settling again changes nothing.
    assert_eq!(
        bank.settle_transaction(&tx_id).unwrap().record,
        settled.record
    );

    let rec2 = bank
        .execute(authorize(&purchase_intent(&acct, &m, 1_000)))
        .unwrap();
    let tx2 = rec2.nessie_transaction_id.clone().unwrap();
    let c = bank.cancel_transaction(&tx2).unwrap();
    assert_eq!(c.record.status, TransactionStatus::Cancelled);
    assert_eq!(
        rec2.refreshed(&c).unwrap().outcome,
        SimulatedOutcome::SimulatedCancelled
    );
    let a = bank.get_account(&acct).unwrap().record;
    assert_eq!((a.balance_cents, a.available_cents), (7_500, 7_500));
    let e = bank.cancel_transaction(&tx2).unwrap_err();
    assert_eq!(e.problem_slug(), Some("not_pending"));

    // A deposit holds nothing and credits on settlement.
    let d = bank
        .deposit(
            &acct,
            &DepositCreate {
                amount_cents: 500,
                description: None,
            },
        )
        .unwrap();
    assert_eq!(d.record.kind, TransactionKind::Deposit);
    assert_eq!(d.record.payee_id.as_deref(), Some(acct.as_str()));
    assert_eq!(
        bank.get_account(&acct).unwrap().record.available_cents,
        7_500
    );
    bank.settle_transaction(&d.record.id).unwrap();
    assert_eq!(bank.get_account(&acct).unwrap().record.balance_cents, 8_000);

    let list = bank.list_account_transactions(&acct, 10).unwrap();
    let kinds: Vec<&str> = list.record.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(kinds, ["deposit", "purchase", "purchase"], "newest first");
    assert_eq!(
        bank.list_account_transactions(&acct, 1)
            .unwrap()
            .record
            .len(),
        1
    );
    assert!(bank.list_account_transactions(&acct, 0).is_err());
    assert!(rec.refreshed(&c).is_err(), "a different transaction");
}

#[test]
fn refusals_release_the_approval_and_create_nothing() {
    let bank = FixtureBank::new();
    let (_, acct) = owner(&bank, 100, AccountType::Checking);
    let m = merchant(&bank, PayeeRole::Landlord);
    let intent = purchase_intent(&acct, &m, 800_000);
    let ev = approved(&intent);

    let err = bank
        .execute(PaymentAuthorization::verify(&ev, intent.clone(), Utc::now()).unwrap())
        .unwrap_err();
    assert_eq!(err.outcome, AttemptOutcome::Refused);
    assert_eq!(err.approval_id, ev.approval_id);
    assert_eq!(err.error.problem_slug(), Some("insufficient_funds"));
    assert_eq!(err.error.http_status(), Some(422));
    assert!(bank
        .list_account_transactions(&acct, 10)
        .unwrap()
        .record
        .is_empty());

    // After funding the mock account, the same approval can be used once.
    let d = bank
        .deposit(
            &acct,
            &DepositCreate {
                amount_cents: 900_000,
                description: None,
            },
        )
        .unwrap();
    bank.settle_transaction(&d.record.id).unwrap();
    let ok = bank
        .execute(PaymentAuthorization::verify(&ev, intent.clone(), Utc::now()).unwrap())
        .unwrap();
    assert_eq!(ok.approval_id, ev.approval_id);
    let again = bank
        .execute(PaymentAuthorization::verify(&ev, intent.clone(), Utc::now()).unwrap())
        .unwrap_err();
    assert_eq!(again.outcome, AttemptOutcome::NotSent);
    assert_eq!(
        again.error,
        NessieError::NotAuthorized(AuthorizationError::AlreadyUsed(ev.approval_id))
    );

    // Unknown merchant or account: 404, nothing created.
    let e = bank
        .execute(authorize(&purchase_intent(&acct, "fixture_merch_9999", 1)))
        .unwrap_err();
    assert_eq!(e.outcome, AttemptOutcome::Refused);
    assert_eq!(e.error.kind(), Some(ProviderErrorKind::NotFound));

    // A sandbox approval does not authorize a fixture payment.
    let mut sandbox = purchase_intent(&acct, &m, 1);
    sandbox.simulation = Simulation::NessieSandbox;
    let e = bank.execute(authorize(&sandbox)).unwrap_err();
    assert_eq!(e.outcome, AttemptOutcome::NotSent);
    assert!(matches!(
        e.error,
        NessieError::NotAuthorized(AuthorizationError::WrongBank { .. })
    ));

    // Credit cards are limited by available credit.
    let (_, card) = owner(&bank, 0, AccountType::CreditCard);
    let a = bank.get_account(&card).unwrap().record;
    assert_eq!(a.credit_limit_cents, Some(100_000));
    assert_eq!(a.available_cents, 100_000);
    let e = bank
        .execute(authorize(&purchase_intent(&card, &m, 100_001)))
        .unwrap_err();
    assert_eq!(e.error.problem_slug(), Some("credit_limit_exceeded"));
    let r = bank
        .execute(authorize(&purchase_intent(&card, &m, 40_000)))
        .unwrap();
    bank.settle_transaction(r.nessie_transaction_id.as_deref().unwrap())
        .unwrap();
    let a = bank.get_account(&card).unwrap().record;
    assert_eq!(
        (a.balance_cents, a.available_cents),
        (40_000, 60_000),
        "card balance is owed"
    );
}

#[test]
fn deleting_cancels_pending_and_cascades() {
    let bank = FixtureBank::new();
    let (cust, acct) = owner(&bank, 10_000, AccountType::Checking);
    let m = merchant(&bank, PayeeRole::Contractor);
    let r = bank
        .execute(authorize(&purchase_intent(&acct, &m, 100)))
        .unwrap();
    let tx = r.nessie_transaction_id.unwrap();
    bank.delete_customer(&cust).unwrap();
    let t = bank.get_transaction(&tx).unwrap().record;
    assert_eq!(t.status, TransactionStatus::Cancelled);
    assert_eq!(t.decision_reason.as_deref(), Some("account_deleted"));
    assert_eq!(
        bank.get_account(&acct).unwrap_err().kind(),
        Some(ProviderErrorKind::NotFound)
    );
    assert!(bank.get_customer(&cust).is_err());
    bank.delete_merchant(&m).unwrap();
    assert!(bank.get_merchant(&m).is_err());
    assert!(bank.delete_merchant(&m).is_err());
    assert!(matches!(
        bank.get_account("x/../y"),
        Err(NessieError::InvalidRequest(_))
    ));
}

/// The whole golden move-in plan for 6922 W Hollywood Blvd, executed against the fixture bank.
#[test]
fn golden_plan_executes_end_to_end() {
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    let bank = FixtureBank::new();
    let (_, acct) = owner(&bank, 15_000_000, AccountType::Checking);
    let mut targets = SandboxTargets {
        account_id: acct.clone(),
        landlord_merchant_id: Some(merchant(&bank, PayeeRole::Landlord)),
        contractor_merchant_id: Some(merchant(&bank, PayeeRole::Contractor)),
        supplier_merchant_id: Some(merchant(&bank, PayeeRole::FurnishingSupplier)),
        occupancy_bill_id: None,
    };
    let plan = plan_move_in_payments(&s, None, Simulation::FixtureBank, None).unwrap();

    let before = check_affordability(&bank.get_account(&acct).unwrap(), &plan, &s, None).unwrap();
    assert_eq!(before.verdict, AffordabilityVerdict::CoversExpectedOnly);
    assert_eq!(
        before.remaining_after_expected_minor,
        Some(15_000_000 - 12_659_650)
    );

    let mut records = Vec::new();
    for p in &plan.payments {
        let intent = plan
            .intent_for(&p.plan_item_id, &targets, "0123abcd4567")
            .unwrap();
        let rec = bank.execute(authorize(&intent)).unwrap();
        assert_eq!(rec.simulation, Simulation::FixtureBank);
        assert_eq!(rec.disclaimer, FIXTURE_BANK_DISCLAIMER);
        assert_eq!(rec.plan_item_id, p.plan_item_id);
        assert_eq!(rec.amount_minor, p.amount_expected_minor);
        if p.plan_item_id == ITEM_OCCUPANCY_BILL {
            assert_eq!(rec.outcome, SimulatedOutcome::SimulatedBillRecorded);
            assert!(!rec.moves_money);
            assert_eq!(rec.provider_status, "recurring");
            assert!(rec.note.contains("No money moved"));
            let bill_id = rec.nessie_bill_id.clone().unwrap();
            let bill = bank.get_bill(&bill_id).unwrap().record;
            assert_eq!(bill.payment_amount_minor().unwrap().minor, 690_000);
            assert_eq!(bill.recurring_date, Some(1));
            targets.occupancy_bill_id = Some(bill_id);
        } else {
            assert_eq!(rec.outcome, SimulatedOutcome::SimulatedPending);
            assert!(rec.nessie_transaction_id.is_some());
        }
        if p.plan_item_id == ITEM_FIRST_MONTH {
            assert_eq!(rec.nessie_bill_id, targets.occupancy_bill_id);
        }
        records.push(rec);
    }
    let a = bank.get_account(&acct).unwrap().record;
    assert_eq!(
        a.pending_debits_cents,
        Some(plan.move_in_payments_expected_minor)
    );
    assert_eq!(a.available_cents, 15_000_000 - 11_431_000);

    for r in &records {
        if let Some(tx) = &r.nessie_transaction_id {
            let settled = bank.settle_transaction(tx).unwrap();
            assert_eq!(
                r.refreshed(&settled).unwrap().outcome,
                SimulatedOutcome::SimulatedCompleted
            );
        }
    }
    let a = bank.get_account(&acct).unwrap().record;
    assert_eq!(a.balance_cents, 15_000_000 - 11_431_000);
    assert_eq!(a.pending_debits_cents, Some(0));
    // The recurring bill stays recurring after its first payment (fixture assumption).
    let bill = bank
        .get_bill(targets.occupancy_bill_id.as_deref().unwrap())
        .unwrap()
        .record;
    assert_eq!(bill.status, BillStatus::Recurring);

    // What is left in the mock account still has to hold the contingency reserve.
    let after = check_affordability(&bank.get_account(&acct).unwrap(), &plan, &s, None).unwrap();
    assert_eq!(after.sandbox_available_minor, 3_569_000);
}

#[test]
fn the_fixture_bank_is_deterministic() {
    let run = || {
        let bank = FixtureBank::new();
        let (_, acct) = owner(&bank, 10_000, AccountType::Savings);
        let m = merchant(&bank, PayeeRole::Contractor);
        let intent = purchase_intent(&acct, &m, 250);
        let mut ev = approved(&intent);
        ev.approval_id = Uuid::from_u128(42);
        let rec = bank
            .execute(PaymentAuthorization::verify(&ev, intent, Utc::now()).unwrap())
            .unwrap();
        let tx = bank
            .get_transaction(rec.nessie_transaction_id.as_deref().unwrap())
            .unwrap();
        (
            serde_json::to_value(&rec).unwrap(),
            serde_json::to_value(&tx).unwrap(),
        )
    };
    let (a, b) = (run(), run());
    assert_eq!(a, b);
    assert_eq!(a.1["record"]["created_at"], "2026-01-01T00:00:04Z");
    assert_eq!(a.1["record"]["id"], "fixture_txn_0004");
}
