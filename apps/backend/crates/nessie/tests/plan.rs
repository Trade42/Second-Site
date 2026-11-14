//! The move-in payment plan and the affordability check, on the finance engine's golden
//! `CostScenario` data for the facial-studio fixture project.

mod common;

use std::collections::BTreeSet;

use common::{golden_scenario, read_json, ALHAMBRA, HOLLYWOOD};
use ss_core::uuid::Uuid;
use ss_domain::contracts::{CostKind, CostLineItem, CostScenario};
use ss_nessie::approval::PaymentOperation;
use ss_nessie::models::{Account, AccountType};
use ss_nessie::plan::{
    ITEM_DEPOSIT, ITEM_FIRST_MONTH, ITEM_FURNISHING, ITEM_OCCUPANCY_BILL, ITEM_RENOVATION,
    LANDLORD_BILL_PAYEE, RESERVE_CONTINGENCY,
};
use ss_nessie::{
    check_affordability, plan_move_in_payments, AffordabilityVerdict, ApprovalAction, BankRecord,
    BomTotals, PayeeRole, PlanError, PlannedOperation, SandboxTargets, Simulation,
};

fn sum_kind(s: &CostScenario, kind: CostKind) -> i64 {
    s.line_items
        .iter()
        .filter(|l| l.kind == kind)
        .map(|l| l.expected)
        .sum()
}

fn line(s: &CostScenario, id: &str) -> CostLineItem {
    s.line_items
        .iter()
        .find(|l| l.line_item_id == id)
        .cloned()
        .unwrap_or_else(|| panic!("no line {id}"))
}

fn account(sim: Simulation, kind: AccountType, balance: i64, pending: i64) -> BankRecord<Account> {
    let available = match kind {
        AccountType::CreditCard => 100_000 - balance - pending,
        _ => balance - pending,
    };
    BankRecord::new(
        sim,
        Account {
            id: "acct_1".into(),
            customer_id: Some("cust_1".into()),
            account_type: kind,
            nickname: "Simulation".into(),
            balance_cents: balance,
            pending_debits_cents: Some(pending),
            available_cents: available,
            credit_limit_cents: None,
            account_number: None,
            rewards: None,
            created_at: None,
            updated_at: None,
        },
    )
}

#[test]
fn hollywood_demo_plan_matches_the_scenario_lines() {
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    // The golden scenario as the finance engine computed it.
    assert_eq!(s.upfront_cash_expected, 11_969_650);
    assert_eq!(s.monthly_operating_cost, 690_000);

    let plan = plan_move_in_payments(&s, None, Simulation::NessieSandbox, None).unwrap();
    let ids: Vec<&str> = plan
        .payments
        .iter()
        .map(|p| p.plan_item_id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            ITEM_DEPOSIT,
            ITEM_OCCUPANCY_BILL,
            ITEM_FIRST_MONTH,
            ITEM_RENOVATION,
            ITEM_FURNISHING
        ]
    );
    let seq: Vec<u32> = plan.payments.iter().map(|p| p.sequence).collect();
    assert_eq!(seq, [1, 2, 3, 4, 5]);

    let dep = plan.payment(ITEM_DEPOSIT).unwrap();
    assert_eq!(dep.amount_expected_minor, 1_200_000);
    assert_eq!(dep.payee_role, PayeeRole::Landlord);
    assert_eq!(dep.operation, PlannedOperation::Purchase);
    assert_eq!(dep.approval_action, ApprovalAction::FinancialCommitment);
    assert_eq!(dep.source_line_item_ids, ["deposit:security"]);

    let bill = plan.payment(ITEM_OCCUPANCY_BILL).unwrap();
    assert_eq!(bill.amount_expected_minor, 600_000 + 90_000);
    assert!(!bill.moves_money);
    assert_eq!(bill.recurring_day, Some(1));
    assert_eq!(
        bill.source_line_item_ids,
        ["occupancy:rent", "occupancy:nnn"]
    );
    let first = plan.payment(ITEM_FIRST_MONTH).unwrap();
    assert_eq!(first.amount_expected_minor, 690_000);
    assert_eq!(first.operation, PlannedOperation::PayBill);
    assert_eq!(first.depends_on.as_deref(), Some(ITEM_OCCUPANCY_BILL));
    assert!(first.moves_money);

    let reno = plan.payment(ITEM_RENOVATION).unwrap();
    let work: Vec<CostLineItem> = s
        .line_items
        .iter()
        .filter(|l| l.source_work_item_id.is_some())
        .cloned()
        .collect();
    assert_eq!(work.len(), 7);
    assert_eq!(
        reno.amount_expected_minor,
        work.iter().map(|l| l.expected).sum::<i64>()
    );
    assert_eq!(reno.amount_expected_minor, 8_191_000);
    assert_eq!(
        reno.amount_low_minor,
        work.iter().map(|l| l.low).sum::<i64>()
    );
    assert_eq!(
        reno.amount_high_minor,
        work.iter().map(|l| l.high).sum::<i64>()
    );
    assert_eq!(reno.approval_action, ApprovalAction::Purchase);
    assert_eq!(reno.payee_role, PayeeRole::Contractor);

    let ffe = plan.payment(ITEM_FURNISHING).unwrap();
    assert_eq!(
        ffe.amount_expected_minor,
        line(&s, "ffe:treatment_rooms").expected
    );
    assert_eq!(ffe.amount_expected_minor, 1_350_000);
    assert_eq!(ffe.payee_role, PayeeRole::FurnishingSupplier);

    assert_eq!(plan.reserves.len(), 1);
    assert_eq!(plan.reserves[0].reserve_id, RESERVE_CONTINGENCY);
    assert_eq!(plan.reserves[0].amount_expected_minor, s.contingency);
    assert_eq!(plan.reserves[0].amount_expected_minor, 1_228_650);
    assert!(plan.not_simulated.is_empty());

    assert_eq!(
        plan.move_in_payments_expected_minor,
        1_200_000 + 690_000 + 8_191_000 + 1_350_000
    );
    assert_eq!(plan.reserves_expected_minor, 1_228_650);
    assert_eq!(plan.move_in_cash_expected_minor, 12_659_650);
    // Move-in cash is the scenario's upfront cash plus the first month's occupancy.
    assert_eq!(
        plan.move_in_cash_expected_minor,
        s.upfront_cash_expected + s.monthly_operating_cost
    );
    assert_eq!(
        plan.move_in_cash_high_minor,
        s.opening_cost_high + sum_kind(&s, CostKind::RefundableDeposit) + s.monthly_operating_cost
    );
    assert_eq!(plan.move_in_cash_high_minor, 20_587_750);
    assert_eq!(plan.scenario_upfront_cash_expected_minor, 11_969_650);
    assert_eq!(plan.monthly_recurring_minor, 690_000);
}

/// For every golden case, property and horizon: each line is planned exactly once, and the
/// totals reconcile with the scenario.
#[test]
fn every_golden_scenario_reconciles() {
    let d = read_json("apps/backend/tests-golden/domain/part2_pipeline.json");
    let mut checked = 0;
    for case in d["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        for addr in case["output"]["properties"].as_object().unwrap().keys() {
            for horizon in ["36", "120", "profile"] {
                let s = golden_scenario(name, addr, horizon);
                let plan = plan_move_in_payments(&s, None, Simulation::FixtureBank, None).unwrap();
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
                let all: Vec<String> = s
                    .line_items
                    .iter()
                    .map(|l| l.line_item_id.clone())
                    .collect();
                let seen_set: BTreeSet<&String> = seen.iter().collect();
                assert_eq!(
                    seen.len(),
                    seen_set.len(),
                    "{name}/{addr}/{horizon}: a line counted twice"
                );
                assert_eq!(
                    seen_set,
                    all.iter().collect::<BTreeSet<_>>(),
                    "{name}/{addr}/{horizon}"
                );
                assert_eq!(
                    plan.move_in_cash_expected_minor,
                    s.upfront_cash_expected + s.monthly_operating_cost,
                    "{name}/{addr}/{horizon}"
                );
                assert_eq!(plan.monthly_recurring_minor, s.monthly_operating_cost);
                assert!(plan.payments.iter().all(|p| p.amount_expected_minor > 0));
                // The horizon changes long-term cost only, never the move-in payments.
                let p36 = plan_move_in_payments(
                    &golden_scenario(name, addr, "36"),
                    None,
                    Simulation::FixtureBank,
                    None,
                )
                .unwrap();
                assert_eq!(p36.payments, plan.payments, "{name}/{addr}/{horizon}");
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 6 * 2 * 3);
}

#[test]
fn bill_of_materials_replaces_the_ffe_allowance() {
    let s = golden_scenario("demo", ALHAMBRA, "36");
    let mut bom = BomTotals {
        bom_id: Uuid::from_u128(7),
        project_id: s.project_id,
        property_id: s.property_id,
        design_version_id: Uuid::from_u128(8),
        currency: "USD".into(),
        estimated_total_minor: 987_654,
        approved_total_minor: None,
        status: "DRAFT".into(),
    };
    let plan = plan_move_in_payments(&s, Some(&bom), Simulation::FixtureBank, None).unwrap();
    let f = plan.payment(ITEM_FURNISHING).unwrap();
    assert_eq!(f.amount_expected_minor, 987_654);
    assert!(f.basis.contains("estimated total"));
    assert!(f.source_line_item_ids.is_empty());
    assert_eq!(plan.not_simulated.len(), 1);
    assert_eq!(plan.not_simulated[0].line_item_id, "ffe:treatment_rooms");
    assert!(plan.not_simulated[0].reason.contains("not counted twice"));
    assert_eq!(plan.bom_id, Some(bom.bom_id));
    assert_eq!(plan.design_version_id, Some(bom.design_version_id));
    let ffe = line(&s, "ffe:treatment_rooms").expected;
    assert_eq!(
        plan.move_in_cash_expected_minor,
        s.upfront_cash_expected + s.monthly_operating_cost - ffe + 987_654
    );

    // An APPROVED BOM's approved total wins; an approved total on a non-approved BOM does not.
    bom.approved_total_minor = Some(900_000);
    let p = plan_move_in_payments(&s, Some(&bom), Simulation::FixtureBank, None).unwrap();
    assert_eq!(
        p.payment(ITEM_FURNISHING).unwrap().amount_expected_minor,
        987_654
    );
    bom.status = "APPROVED".into();
    let p = plan_move_in_payments(&s, Some(&bom), Simulation::FixtureBank, None).unwrap();
    let f = p.payment(ITEM_FURNISHING).unwrap();
    assert_eq!(f.amount_expected_minor, 900_000);
    assert_eq!(f.basis, "bill of materials, approved total");

    // A BOM of another property, in another currency, or negative is refused.
    let mut other = bom.clone();
    other.property_id = Uuid::from_u128(9);
    assert!(matches!(
        plan_move_in_payments(&s, Some(&other), Simulation::FixtureBank, None),
        Err(PlanError::BomMismatch(_))
    ));
    let mut eur = bom.clone();
    eur.currency = "EUR".into();
    assert!(matches!(
        plan_move_in_payments(&s, Some(&eur), Simulation::FixtureBank, None),
        Err(PlanError::Currency(_))
    ));
    let mut neg = bom.clone();
    neg.estimated_total_minor = -1;
    assert!(plan_move_in_payments(&s, Some(&neg), Simulation::FixtureBank, None).is_err());
}

#[test]
fn unknown_rent_unknown_lines_and_currency() {
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
    s.line_items.push(CostLineItem {
        line_item_id: "broker:fee".into(),
        category: "BROKER".into(),
        description: "Broker fee".into(),
        kind: CostKind::OneTime,
        low: 100,
        expected: 200,
        high: 300,
        source_work_item_id: None,
        evidence_ids: vec![],
    });
    let plan = plan_move_in_payments(&s, None, Simulation::FixtureBank, Some(15)).unwrap();
    assert!(plan.payment(ITEM_OCCUPANCY_BILL).is_none());
    assert!(plan.payment(ITEM_FIRST_MONTH).is_none());
    assert!(plan
        .assumptions
        .iter()
        .any(|a| a.contains("Monthly occupancy cost is $0")));
    assert!(plan
        .assumptions
        .iter()
        .any(|a| a == "Cost scenario: Asking rent unknown — monthly cost understated"));
    // M1: the $0 MONTHLY lines are listed too (they used to land nowhere).
    assert_eq!(plan.not_simulated.len(), 3);
    assert!(plan.not_simulated[1..]
        .iter()
        .all(|l| l.kind == CostKind::Monthly && l.reason.contains("expected monthly rent and NNN total is $0")));
    assert_eq!(plan.not_simulated[0].line_item_id, "broker:fee");
    assert!(plan.not_simulated[0].reason.contains("no payee is known"));
    assert_eq!(plan.not_simulated_expected_minor, 200);

    let mut eur = golden_scenario("demo", HOLLYWOOD, "36");
    eur.currency = ss_core::CurrencyCode::new("EUR").unwrap();
    assert!(matches!(
        plan_move_in_payments(&eur, None, Simulation::FixtureBank, None),
        Err(PlanError::Currency(_))
    ));
    assert!(plan_move_in_payments(&eur, None, Simulation::FixtureBank, Some(0)).is_err());
}

#[test]
fn intents_follow_the_plan_exactly() {
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    let plan = plan_move_in_payments(&s, None, Simulation::NessieSandbox, Some(5)).unwrap();
    let mut targets = SandboxTargets {
        account_id: "acct_1".into(),
        landlord_merchant_id: Some("merch_landlord".into()),
        contractor_merchant_id: None,
        supplier_merchant_id: Some("merch_supplier".into()),
        occupancy_bill_id: None,
    };
    assert!(matches!(
        plan.intent_for(ITEM_RENOVATION, &targets, "0123abcd4567"),
        Err(PlanError::MissingTarget(_))
    ));
    assert!(matches!(
        plan.intent_for(ITEM_FIRST_MONTH, &targets, "0123abcd4567"),
        Err(PlanError::MissingTarget(_))
    ));
    assert!(matches!(
        plan.intent_for("movein:nothing", &targets, "0123abcd4567"),
        Err(PlanError::UnknownItem(_))
    ));
    targets.contractor_merchant_id = Some("merch_contractor".into());
    targets.occupancy_bill_id = Some("bill_1".into());

    for p in &plan.payments {
        let i = plan.intent_for(&p.plan_item_id, &targets, "0123abcd4567").unwrap();
        assert_eq!(i.amount_minor, p.amount_expected_minor);
        assert_eq!(i.action, p.approval_action);
        assert_eq!(i.simulation, Simulation::NessieSandbox);
        assert_eq!(i.currency, "USD");
        assert!(i.description.starts_with("SIMULATION"));
        assert!(i.description.chars().count() <= 200);
        assert_eq!(i.project_id, s.project_id);
        assert_eq!(i.scenario_id, s.scenario_id);
        match (&i.operation, p.plan_item_id.as_str()) {
            (PaymentOperation::Purchase { merchant_id, .. }, ITEM_DEPOSIT) => {
                assert_eq!(merchant_id, "merch_landlord")
            }
            (PaymentOperation::Purchase { merchant_id, .. }, ITEM_RENOVATION) => {
                assert_eq!(merchant_id, "merch_contractor")
            }
            (PaymentOperation::Purchase { merchant_id, .. }, ITEM_FURNISHING) => {
                assert_eq!(merchant_id, "merch_supplier")
            }
            (
                PaymentOperation::CreateRecurringBill {
                    payee,
                    recurring_day,
                    ..
                },
                ITEM_OCCUPANCY_BILL,
            ) => {
                assert_eq!(payee, LANDLORD_BILL_PAYEE);
                assert_eq!(*recurring_day, 5);
            }
            (
                PaymentOperation::PayBill {
                    account_id,
                    bill_id,
                },
                ITEM_FIRST_MONTH,
            ) => {
                assert_eq!(bill_id, "bill_1");
                assert_eq!(account_id, "acct_1");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    targets.account_id = "acct 1".into();
    assert!(matches!(
        plan.intent_for(ITEM_DEPOSIT, &targets, "0123abcd4567"),
        Err(PlanError::Invalid(_))
    ));
}

#[test]
fn plan_output_is_labelled_as_a_simulation() {
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    for sim in [Simulation::NessieSandbox, Simulation::FixtureBank] {
        let plan = plan_move_in_payments(&s, None, sim, None).unwrap();
        let v = serde_json::to_value(&plan).unwrap();
        assert_eq!(v["simulation"], sim.as_str());
        assert_eq!(v["disclaimer"], sim.disclaimer());
        assert!(plan.assumptions[0].contains("mock money"));
        assert!(plan
            .assumptions
            .iter()
            .any(|a| a.contains("stand-in sandbox merchants")));
        assert!(plan
            .assumptions
            .iter()
            .any(|a| a.starts_with("Not included (cost scenario): ")));
    }
}

#[test]
fn affordability_verdicts() {
    let s = golden_scenario("demo", HOLLYWOOD, "36");
    let sim = Simulation::FixtureBank;
    let plan = plan_move_in_payments(&s, None, sim, None).unwrap();
    let expected = plan.move_in_cash_expected_minor; // 12,659,650
    let high = plan.move_in_cash_high_minor; // 20,587,750

    let r = check_affordability(
        &account(sim, AccountType::Checking, expected - 1, 0),
        &plan,
        &s,
        None,
    )
    .unwrap();
    assert_eq!(r.verdict, AffordabilityVerdict::Short);
    assert_eq!(r.remaining_after_expected_minor, Some(-1));
    assert_eq!(r.months_of_occupancy_covered, None);
    assert!(r.headline.starts_with("Simulation only:"), "{}", r.headline);
    assert!(r.headline.contains("$0.01 short"), "{}", r.headline);

    // Pending holds count against the balance.
    let r = check_affordability(
        &account(sim, AccountType::Checking, expected + 500, 1_000),
        &plan,
        &s,
        None,
    )
    .unwrap();
    assert_eq!(r.sandbox_available_minor, expected - 500);
    assert_eq!(r.verdict, AffordabilityVerdict::Short);

    let r = check_affordability(
        &account(sim, AccountType::Savings, expected, 0),
        &plan,
        &s,
        None,
    )
    .unwrap();
    assert_eq!(r.verdict, AffordabilityVerdict::CoversExpectedOnly);
    assert_eq!(r.remaining_after_expected_minor, Some(0));
    assert_eq!(r.remaining_after_high_minor, Some(expected - high));

    let balance = high + 3 * 690_000 + 5;
    let r = check_affordability(
        &account(sim, AccountType::Checking, balance, 0),
        &plan,
        &s,
        None,
    )
    .unwrap();
    assert_eq!(r.verdict, AffordabilityVerdict::CoversHighEstimate);
    let left = balance - expected;
    assert_eq!(r.remaining_after_expected_minor, Some(left));
    assert_eq!(r.months_of_occupancy_covered, Some(left / 690_000));
    assert!(
        r.headline.contains("months of rent and NNN"),
        "{}",
        r.headline
    );

    // The working-capital reserve is added to what must be covered.
    let r = check_affordability(
        &account(sim, AccountType::Checking, high, 0),
        &plan,
        &s,
        Some(3_000_000),
    )
    .unwrap();
    assert_eq!(r.required_high_minor, high + 3_000_000);
    assert_eq!(r.verdict, AffordabilityVerdict::CoversExpectedOnly);

    let r = check_affordability(
        &account(sim, AccountType::CreditCard, 0, 0),
        &plan,
        &s,
        None,
    )
    .unwrap();
    assert_eq!(r.verdict, AffordabilityVerdict::NotACashAccount);
    assert_eq!(r.remaining_after_expected_minor, None);
    let r = check_affordability(
        &account(sim, AccountType::Unknown("Brokerage".into()), 1 << 40, 0),
        &plan,
        &s,
        None,
    )
    .unwrap();
    assert_eq!(r.verdict, AffordabilityVerdict::Unknown);

    // Honest caveats, always.
    let r =
        check_affordability(&account(sim, AccountType::Checking, 1, 0), &plan, &s, None).unwrap();
    assert_eq!(r.caveats[0], sim.disclaimer());
    assert!(r
        .caveats
        .iter()
        .any(|c| c.contains("not a lending decision or financial advice")));
    assert!(r
        .caveats
        .iter()
        .any(|c| c.contains("only what was put into this mock account")));
    assert!(r
        .caveats
        .iter()
        .any(|c| c.contains("No working-capital reserve was given")));
    assert!(r.caveats.iter().any(|c| c.contains("$119,696.50")));
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["simulation"], "FIXTURE_IN_MEMORY_BANK");
    assert_eq!(v["verdict"], "SANDBOX_BALANCE_SHORT");
    assert_eq!(v["disclaimer"], sim.disclaimer());

    // Mixing banks or scenarios is refused.
    assert!(check_affordability(
        &account(Simulation::NessieSandbox, AccountType::Checking, 1, 0),
        &plan,
        &s,
        None
    )
    .is_err());
    let other = golden_scenario("demo", ALHAMBRA, "36");
    assert!(check_affordability(
        &account(sim, AccountType::Checking, 1, 0),
        &plan,
        &other,
        None
    )
    .is_err());
    assert!(check_affordability(
        &account(sim, AccountType::Checking, 1, 0),
        &plan,
        &s,
        Some(-1)
    )
    .is_err());
}

#[test]
fn capital_case_matches_the_ranking_constraint_direction() {
    // The ranking engine compares upfront cash with the owner's opening capital; the sandbox
    // check uses move-in cash (one more month of occupancy), so it is never more lenient.
    let s = golden_scenario("capital_15m", HOLLYWOOD, "36");
    let plan = plan_move_in_payments(&s, None, Simulation::FixtureBank, None).unwrap();
    assert!(plan.move_in_cash_expected_minor > s.upfront_cash_expected);
    let r = check_affordability(
        &account(
            Simulation::FixtureBank,
            AccountType::Checking,
            s.upfront_cash_expected,
            0,
        ),
        &plan,
        &s,
        None,
    )
    .unwrap();
    assert_eq!(r.verdict, AffordabilityVerdict::Short);
}
