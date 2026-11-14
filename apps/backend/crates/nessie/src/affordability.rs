//! The affordability view: a sandbox account's available balance against the move-in cash a
//! [`MoveInPaymentPlan`] needs, plus an optional working-capital reserve.
//!
//! What it can say: whether this mock account would cover the planned move-in payments and
//! reserves at the expected and at the high estimate, what would be left, and roughly how many
//! months of rent and NNN that remainder pays. What it cannot say: anything about the owner's
//! real funds, credit, income, other obligations or the timing of payments. A sandbox balance is
//! only what was put into the mock account. A credit card's available credit is borrowing, not
//! cash, so card accounts get `NOT_A_CASH_ACCOUNT` instead of a verdict.

use serde::Serialize;
use ss_domain::contracts::CostScenario;
use uuid::Uuid;

use crate::models::{Account, AccountType};
use crate::money::format_usd;
use crate::plan::{MoveInPaymentPlan, PlanError};
use crate::{BankRecord, Simulation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum AffordabilityVerdict {
    /// Available >= move-in cash at the high estimate (+ reserve).
    #[serde(rename = "SANDBOX_BALANCE_COVERS_HIGH_ESTIMATE")]
    CoversHighEstimate,
    /// Available >= the expected move-in cash (+ reserve), but below the high estimate.
    #[serde(rename = "SANDBOX_BALANCE_COVERS_EXPECTED_ONLY")]
    CoversExpectedOnly,
    /// Available < the expected move-in cash (+ reserve).
    #[serde(rename = "SANDBOX_BALANCE_SHORT")]
    Short,
    /// A credit card: available credit is borrowing, not cash.
    #[serde(rename = "NOT_A_CASH_ACCOUNT")]
    NotACashAccount,
    /// No verdict: the account type is one this crate does not know, or the monthly rent and
    /// NNN are unknown ($0 in the cost scenario), so coverage cannot be claimed.
    #[serde(rename = "UNKNOWN")]
    Unknown,
}

impl AffordabilityVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            AffordabilityVerdict::CoversHighEstimate => "SANDBOX_BALANCE_COVERS_HIGH_ESTIMATE",
            AffordabilityVerdict::CoversExpectedOnly => "SANDBOX_BALANCE_COVERS_EXPECTED_ONLY",
            AffordabilityVerdict::Short => "SANDBOX_BALANCE_SHORT",
            AffordabilityVerdict::NotACashAccount => "NOT_A_CASH_ACCOUNT",
            AffordabilityVerdict::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AffordabilityReport {
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    pub project_id: Uuid,
    pub property_id: Uuid,
    pub scenario_id: Uuid,
    pub account_id: String,
    pub account_type: String,
    pub sandbox_balance_minor: i64,
    pub sandbox_pending_holds_minor: i64,
    /// What the mock account can spend now (balance minus holds).
    pub sandbox_available_minor: i64,
    pub move_in_cash_expected_minor: i64,
    pub move_in_cash_high_minor: i64,
    pub working_capital_reserve_minor: Option<i64>,
    /// max(move-in cash, the scenario's upfront cash) + reserve: never below what the ranking
    /// engine's opening-capital constraint requires.
    pub required_expected_minor: i64,
    pub required_high_minor: i64,
    /// Available - required (negative when short). `None` for a card account.
    pub remaining_after_expected_minor: Option<i64>,
    pub remaining_after_high_minor: Option<i64>,
    /// Rent + NNN per month (expected).
    pub monthly_cost_minor: i64,
    /// Whole months of rent + NNN the remainder (after the expected move-in cash and reserve)
    /// would pay; `None` when nothing remains to count, the monthly cost is $0, or the account
    /// is not a cash account.
    pub months_of_occupancy_covered: Option<i64>,
    pub verdict: AffordabilityVerdict,
    pub headline: String,
    pub caveats: Vec<String>,
}

/// Compares the sandbox account with the plan. `working_capital_reserve` is the owner's
/// `BusinessExpansionProfile.working_capital_reserve` when they gave one.
pub fn check_affordability(
    account: &BankRecord<Account>,
    plan: &MoveInPaymentPlan,
    scenario: &CostScenario,
    working_capital_reserve: Option<i64>,
) -> Result<AffordabilityReport, PlanError> {
    if account.simulation != plan.simulation {
        return Err(PlanError::Invalid(format!(
            "the account comes from the {} bank but the plan targets the {} bank",
            account.simulation, plan.simulation
        )));
    }
    if plan.scenario_id != scenario.scenario_id {
        return Err(PlanError::Invalid(
            "the plan was built from a different cost scenario".into(),
        ));
    }
    if working_capital_reserve.is_some_and(|r| r < 0) {
        return Err(PlanError::Invalid(
            "working_capital_reserve must not be negative".into(),
        ));
    }
    let a = &account.record;
    let reserve = working_capital_reserve.unwrap_or(0);
    let ovf = || PlanError::Overflow("required cash".into());
    // Never more lenient than the ranking engine (upfront_cash_expected <= opening_capital):
    // planned move-in cash falls below the upfront cash when cost lines are not simulated or a
    // bill of materials replaces a larger FF&E allowance.
    let base_expected = plan
        .move_in_cash_expected_minor
        .max(scenario.upfront_cash_expected);
    let base_high = plan.move_in_cash_high_minor.max(base_expected);
    let required_expected = base_expected.checked_add(reserve).ok_or_else(ovf)?;
    let required_high = base_high.checked_add(reserve).ok_or_else(ovf)?;
    let available = a.available_cents;
    let monthly = plan.monthly_recurring_minor;
    // The finance engine writes $0 for an unknown asking rent: an unknown cost can still show a
    // shortfall, but never coverage.
    let occupancy_unknown = monthly <= 0;

    let is_cash = matches!(a.account_type, AccountType::Checking | AccountType::Savings);
    let verdict = match a.account_type {
        AccountType::CreditCard => AffordabilityVerdict::NotACashAccount,
        AccountType::Unknown(_) => AffordabilityVerdict::Unknown,
        _ if available < required_expected => AffordabilityVerdict::Short,
        _ if occupancy_unknown => AffordabilityVerdict::Unknown,
        _ if available >= required_high => AffordabilityVerdict::CoversHighEstimate,
        _ => AffordabilityVerdict::CoversExpectedOnly,
    };
    let remaining_expected = is_cash.then(|| available.saturating_sub(required_expected));
    let remaining_high = is_cash.then(|| available.saturating_sub(required_high));
    let months = match remaining_expected {
        Some(r) if r > 0 && monthly > 0 => Some(r / monthly),
        _ => None,
    };

    let avail = format_usd(available);
    let need = format_usd(required_expected);
    let need_high = format_usd(required_high);
    let what = if working_capital_reserve.is_some() {
        "move-in cash plus the working-capital reserve"
    } else {
        "move-in cash"
    };
    let headline = match verdict {
        AffordabilityVerdict::CoversHighEstimate => format!(
            "Simulation only: the sandbox account's {avail} available (mock money) covers even the high estimate of {what} ({need_high}); at the expected {need} it would leave {}{}.",
            format_usd(remaining_expected.unwrap_or(0)),
            months
                .map(|m| format!(", about {m} months of rent and NNN"))
                .unwrap_or_default()
        ),
        AffordabilityVerdict::CoversExpectedOnly => format!(
            "Simulation only: the sandbox account's {avail} available (mock money) covers the expected {what} of {need}, but is {} short of the high estimate ({need_high}).",
            format_usd(required_high.saturating_sub(available))
        ),
        AffordabilityVerdict::Short => format!(
            "Simulation only: the sandbox account's {avail} available (mock money) is {} short of the expected {what} of {need}.",
            format_usd(required_expected.saturating_sub(available))
        ),
        AffordabilityVerdict::NotACashAccount => "Simulation only: this sandbox account is a credit card. Available credit is borrowing, not cash, so it is not compared with the move-in cost.".to_owned(),
        AffordabilityVerdict::Unknown if is_cash => format!(
            "Simulation only: the sandbox account's {avail} available (mock money) is at least the known part of the {what} ({need}), but the monthly rent and NNN are unknown ($0 in the cost scenario), so no verdict is given."
        ),
        AffordabilityVerdict::Unknown => format!(
            "Simulation only: the sandbox account type {:?} is unknown, so no verdict is given.",
            a.account_type.as_str()
        ),
    };

    let mut caveats = vec![
        account.simulation.disclaimer().to_owned(),
        "The sandbox balance is only what was put into this mock account (its opening balance and deposits). It says nothing about the owner's real funds, credit, income or other obligations, and it is not a lending decision or financial advice.".to_owned(),
        "Pending holds are already subtracted: available = balance minus pending debits.".to_owned(),
        format!(
            "Move-in cash ({}) = planned payments ({}) + reserves ({}). It includes the first month's rent and NNN; the cost scenario's upfront cash figure ({}) does not.",
            format_usd(plan.move_in_cash_expected_minor),
            format_usd(plan.move_in_payments_expected_minor),
            format_usd(plan.reserves_expected_minor),
            format_usd(plan.scenario_upfront_cash_expected_minor)
        ),
        "Costs are planning estimates from the cost scenario (expected, with a high estimate), not quotes.".to_owned(),
        "A balance check cannot show when each payment falls due; a real build-out is paid in stages.".to_owned(),
    ];
    if working_capital_reserve.is_none() {
        caveats.push(
            "No working-capital reserve was given, so the check leaves nothing for running the business after move-in.".into(),
        );
    }
    if plan.not_simulated_expected_minor != 0 {
        caveats.push(format!(
            "{} of cost lines are not simulated as payments (see not_simulated) and are not part of the planned move-in cash.",
            format_usd(plan.not_simulated_expected_minor)
        ));
    }
    if base_expected > plan.move_in_cash_expected_minor {
        caveats.push(format!(
            "The planned move-in cash ({}) is below the cost scenario's upfront cash ({}), so the check uses the upfront cash: it is never more lenient than the ranking's opening-capital check.",
            format_usd(plan.move_in_cash_expected_minor),
            format_usd(scenario.upfront_cash_expected)
        ));
    }
    caveats.extend(
        scenario
            .assumptions
            .iter()
            .map(|s| format!("Cost scenario: {s}")),
    );

    Ok(AffordabilityReport {
        simulation: account.simulation,
        disclaimer: account.simulation.disclaimer(),
        project_id: plan.project_id,
        property_id: plan.property_id,
        scenario_id: plan.scenario_id,
        account_id: a.id.clone(),
        account_type: a.account_type.as_str().to_owned(),
        sandbox_balance_minor: a.balance_cents,
        sandbox_pending_holds_minor: a.pending_debits_cents.unwrap_or(0),
        sandbox_available_minor: available,
        move_in_cash_expected_minor: plan.move_in_cash_expected_minor,
        move_in_cash_high_minor: plan.move_in_cash_high_minor,
        working_capital_reserve_minor: working_capital_reserve,
        required_expected_minor: required_expected,
        required_high_minor: required_high,
        remaining_after_expected_minor: remaining_expected,
        remaining_after_high_minor: remaining_high,
        monthly_cost_minor: monthly,
        months_of_occupancy_covered: months,
        verdict,
        headline,
        caveats,
    })
}
