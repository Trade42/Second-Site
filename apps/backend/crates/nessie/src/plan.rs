//! The move-in payment plan: which simulated payments follow from a property's `CostScenario`
//! (and, when one exists, its bill of materials), and which Nessie object each becomes.
//!
//! Every cost line lands in exactly one place, so nothing is counted twice or dropped silently:
//!
//! | Cost line | Planned as | Nessie call | Approval |
//! |---|---|---|---|
//! | `REFUNDABLE_DEPOSIT` lines (`deposit:security`) | `movein:security_deposit` | purchase at the stand-in landlord merchant | `FINANCIAL_COMMITMENT` |
//! | `MONTHLY` lines (`occupancy:rent`, `occupancy:nnn`) | `movein:recurring_occupancy_bill` | recurring bill to the landlord (no money moves) | `FINANCIAL_COMMITMENT` |
//! | the same `MONTHLY` lines, once | `movein:first_month_occupancy` | payment of that bill | `FINANCIAL_COMMITMENT` |
//! | `ONE_TIME` work-item lines (`work:*`) | `buildout:renovation` | purchase at the stand-in contractor merchant | `PURCHASE` |
//! | `ONE_TIME` FF&E lines (`ffe:*`), or the BOM total instead | `buildout:furnishing` | purchase at the stand-in furnishing supplier | `PURCHASE` |
//! | `ONE_TIME` contingency (`contingency:*`) | a reserve, **not** a payment | none | none |
//! | anything else | `not_simulated`, with the reason | none | none |
//!
//! Amounts are the scenario's `expected` values (low and high are carried for context).
//! When a bill of materials is given, its total (the approved total once the BOM is APPROVED,
//! otherwise the estimate) replaces the FF&E allowance lines, which are then listed as
//! `not_simulated` with that reason.

use serde::Serialize;
use ss_domain::contracts::{CostKind, CostLineItem, CostScenario};
use thiserror::Error;
use uuid::Uuid;

use crate::approval::{ApprovalAction, PaymentIntent, PaymentOperation};
use crate::error::truncate;
use crate::models::{validate_id, MerchantCreate};
use crate::Simulation;

/// Payee on the recurring occupancy bill.
pub const LANDLORD_BILL_PAYEE: &str = "SIMULATED LANDLORD (Second Site stand-in)";
/// Nickname of the recurring occupancy bill.
pub const OCCUPANCY_BILL_NICKNAME: &str = "SIMULATION: monthly rent + NNN";
/// Day of the month the recurring bill is due when the caller does not choose one.
pub const DEFAULT_RECURRING_DAY: u8 = 1;

pub const ITEM_DEPOSIT: &str = "movein:security_deposit";
pub const ITEM_OCCUPANCY_BILL: &str = "movein:recurring_occupancy_bill";
pub const ITEM_FIRST_MONTH: &str = "movein:first_month_occupancy";
pub const ITEM_RENOVATION: &str = "buildout:renovation";
pub const ITEM_FURNISHING: &str = "buildout:furnishing";
pub const RESERVE_CONTINGENCY: &str = "reserve:contingency";

/// Who a planned payment goes to. In the sandbox each role is a stand-in merchant created by
/// [`stand_in_merchant`]; none is the real landlord, contractor or supplier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum PayeeRole {
    #[serde(rename = "LANDLORD")]
    Landlord,
    #[serde(rename = "CONTRACTOR")]
    Contractor,
    #[serde(rename = "FURNISHING_SUPPLIER")]
    FurnishingSupplier,
}

impl PayeeRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            PayeeRole::Landlord => "LANDLORD",
            PayeeRole::Contractor => "CONTRACTOR",
            PayeeRole::FurnishingSupplier => "FURNISHING_SUPPLIER",
        }
    }
}

/// The stand-in merchant for a role (`POST /merchants`). Its name says it is a simulation.
pub fn stand_in_merchant(role: PayeeRole) -> MerchantCreate {
    let (name, category) = match role {
        PayeeRole::Landlord => (LANDLORD_BILL_PAYEE, "real_estate"),
        PayeeRole::Contractor => (
            "SIMULATED CONTRACTOR (Second Site stand-in)",
            "construction",
        ),
        PayeeRole::FurnishingSupplier => (
            "SIMULATED FURNISHING SUPPLIER (Second Site stand-in)",
            "furnishings",
        ),
    };
    MerchantCreate {
        name: name.to_owned(),
        category: Some(category.to_owned()),
        address: None,
        geocode: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum PlannedOperation {
    #[serde(rename = "PURCHASE")]
    Purchase,
    #[serde(rename = "CREATE_RECURRING_BILL")]
    CreateRecurringBill,
    #[serde(rename = "PAY_BILL")]
    PayBill,
}

/// Totals of a bill of materials (`BillOfMaterials` in `apps/api/app/contracts/procurement.py`):
/// pass `bom_id`, `project_id`, `property_id`, `design_version_id`, `currency`,
/// `estimated_total`, `approved_total` and `status` as they are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BomTotals {
    pub bom_id: Uuid,
    pub project_id: Uuid,
    pub property_id: Uuid,
    pub design_version_id: Uuid,
    pub currency: String,
    pub estimated_total_minor: i64,
    pub approved_total_minor: Option<i64>,
    /// `BomStatus` wire value: DRAFT, RECALCULATION_REQUIRED, PENDING_APPROVAL or APPROVED.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedPayment {
    /// Stable id, e.g. `movein:security_deposit`; also the approval payload's `plan_item_id`.
    pub plan_item_id: String,
    /// Execution order, from 1.
    pub sequence: u32,
    pub label: String,
    pub payee_role: PayeeRole,
    pub operation: PlannedOperation,
    pub approval_action: ApprovalAction,
    /// False for the recurring bill: creating it records the obligation, no money moves.
    pub moves_money: bool,
    pub amount_low_minor: i64,
    /// The amount that is simulated.
    pub amount_expected_minor: i64,
    pub amount_high_minor: i64,
    pub recurring_day: Option<u8>,
    /// A plan item that must be executed first (the bill before its payment).
    pub depends_on: Option<String>,
    pub source_line_item_ids: Vec<String>,
    /// Where the amount comes from.
    pub basis: String,
    pub notes: Vec<String>,
}

/// Cash kept back rather than paid to anyone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedReserve {
    pub reserve_id: String,
    pub label: String,
    pub amount_low_minor: i64,
    pub amount_expected_minor: i64,
    pub amount_high_minor: i64,
    pub source_line_item_ids: Vec<String>,
    pub note: String,
}

/// A cost line that is not simulated, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotSimulatedLine {
    pub line_item_id: String,
    pub category: String,
    pub kind: CostKind,
    pub expected_minor: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveInPaymentPlan {
    /// The bank the plan's payments will be simulated in.
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    pub project_id: Uuid,
    pub property_id: Uuid,
    pub scenario_id: Uuid,
    pub design_version_id: Option<Uuid>,
    pub bom_id: Option<Uuid>,
    pub currency: String,
    pub payments: Vec<PlannedPayment>,
    pub reserves: Vec<PlannedReserve>,
    pub not_simulated: Vec<NotSimulatedLine>,
    /// Payments that move money at move-in (everything except creating the recurring bill).
    pub move_in_payments_expected_minor: i64,
    pub move_in_payments_high_minor: i64,
    pub reserves_expected_minor: i64,
    pub reserves_high_minor: i64,
    /// Payments plus reserves: the cash to have at move-in.
    pub move_in_cash_expected_minor: i64,
    pub move_in_cash_high_minor: i64,
    /// The scenario's `upfront_cash_expected` (opening cost + deposits; no first month).
    pub scenario_upfront_cash_expected_minor: i64,
    /// Sum of the scenario's MONTHLY lines (rent + NNN), expected.
    pub monthly_recurring_minor: i64,
    pub not_simulated_expected_minor: i64,
    pub assumptions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanError {
    #[error("Nessie amounts are US cents; {0}")]
    Currency(String),
    #[error("bill of materials does not belong to this scenario: {0}")]
    BomMismatch(String),
    #[error("amount out of range: {0}")]
    Overflow(String),
    #[error("plan has no item {0}")]
    UnknownItem(String),
    #[error("missing sandbox target: {0}")]
    MissingTarget(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}

#[derive(Default)]
struct Group {
    low: i64,
    expected: i64,
    high: i64,
    ids: Vec<String>,
}

impl Group {
    fn add(&mut self, li: &CostLineItem) -> Result<(), PlanError> {
        let ovf = || PlanError::Overflow(li.line_item_id.clone());
        self.low = self.low.checked_add(li.low).ok_or_else(ovf)?;
        self.expected = self.expected.checked_add(li.expected).ok_or_else(ovf)?;
        self.high = self.high.checked_add(li.high).ok_or_else(ovf)?;
        self.ids.push(li.line_item_id.clone());
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

fn checked_sum(values: impl IntoIterator<Item = i64>) -> Result<i64, PlanError> {
    values.into_iter().try_fold(0i64, |acc, v| {
        acc.checked_add(v)
            .ok_or_else(|| PlanError::Overflow("plan total".into()))
    })
}

const BASIS_SCENARIO: &str =
    "cost scenario line items (expected planning estimate; low and high shown for context)";

/// Builds the plan. `recurring_day` defaults to [`DEFAULT_RECURRING_DAY`].
pub fn plan_move_in_payments(
    scenario: &CostScenario,
    bom: Option<&BomTotals>,
    simulation: Simulation,
    recurring_day: Option<u8>,
) -> Result<MoveInPaymentPlan, PlanError> {
    if scenario.currency.as_str() != "USD" {
        return Err(PlanError::Currency(format!(
            "the cost scenario is in {}",
            scenario.currency.as_str()
        )));
    }
    let day = recurring_day.unwrap_or(DEFAULT_RECURRING_DAY);
    if !(1..=31).contains(&day) {
        return Err(PlanError::Invalid("recurring_day must be 1 to 31".into()));
    }
    if let Some(b) = bom {
        if b.currency != "USD" {
            return Err(PlanError::Currency(format!(
                "the bill of materials is in {}",
                b.currency
            )));
        }
        if b.project_id != scenario.project_id || b.property_id != scenario.property_id {
            return Err(PlanError::BomMismatch(format!(
                "BOM {} is for project {} / property {}",
                b.bom_id, b.project_id, b.property_id
            )));
        }
        if b.estimated_total_minor < 0 || b.approved_total_minor.is_some_and(|t| t < 0) {
            return Err(PlanError::Invalid("BOM totals must not be negative".into()));
        }
    }

    let mut deposit = Group::default();
    let mut monthly = Group::default();
    let mut renovation = Group::default();
    let mut ffe = Group::default();
    let mut contingency = Group::default();
    let mut not_simulated = Vec::new();
    let skip = |li: &CostLineItem, reason: &str| NotSimulatedLine {
        line_item_id: li.line_item_id.clone(),
        category: li.category.clone(),
        kind: li.kind,
        expected_minor: li.expected,
        reason: reason.to_owned(),
    };

    for li in &scenario.line_items {
        if li.low < 0 || li.expected < 0 || li.high < 0 {
            not_simulated.push(skip(
                li,
                "negative amount: a credit, not a payment, so it is not simulated",
            ));
            continue;
        }
        let id = li.line_item_id.as_str();
        match li.kind {
            CostKind::RefundableDeposit => deposit.add(li)?,
            CostKind::Monthly => monthly.add(li)?,
            CostKind::OneTime => {
                if li.category == "CONTINGENCY" || id.starts_with("contingency:") {
                    contingency.add(li)?;
                } else if li.category == "FF&E" || id.starts_with("ffe:") {
                    if bom.is_some() {
                        not_simulated.push(skip(
                            li,
                            "replaced by the bill of materials total, so furnishing is not counted twice",
                        ));
                    } else {
                        ffe.add(li)?;
                    }
                } else if li.source_work_item_id.is_some() || id.starts_with("work:") {
                    renovation.add(li)?;
                } else {
                    not_simulated.push(skip(
                        li,
                        "no payee is known for this cost line, so it is not simulated",
                    ));
                }
            }
        }
    }

    let mut payments: Vec<PlannedPayment> = Vec::new();
    let mut notes_zero: Vec<String> = Vec::new();
    let mut push = |p: PlannedPayment| {
        let mut p = p;
        p.sequence = u32::try_from(payments.len() + 1).unwrap_or(u32::MAX);
        payments.push(p);
    };
    let payment = |id: &str,
                   label: &str,
                   role: PayeeRole,
                   op: PlannedOperation,
                   action: ApprovalAction,
                   g: &Group,
                   basis: &str| PlannedPayment {
        plan_item_id: id.to_owned(),
        sequence: 0,
        label: label.to_owned(),
        payee_role: role,
        operation: op,
        approval_action: action,
        moves_money: op != PlannedOperation::CreateRecurringBill,
        amount_low_minor: g.low,
        amount_expected_minor: g.expected,
        amount_high_minor: g.high,
        recurring_day: None,
        depends_on: None,
        source_line_item_ids: g.ids.clone(),
        basis: basis.to_owned(),
        notes: Vec::new(),
    };

    if !deposit.is_empty() && deposit.expected > 0 {
        let mut p = payment(
            ITEM_DEPOSIT,
            "Refundable security deposit (simulated)",
            PayeeRole::Landlord,
            PlannedOperation::Purchase,
            ApprovalAction::FinancialCommitment,
            &deposit,
            BASIS_SCENARIO,
        );
        p.notes.push(
            "Refundable in a real lease; the sandbox records it as a purchase at the stand-in landlord and does not model the refund.".into(),
        );
        push(p);
    } else if !deposit.is_empty() {
        notes_zero
            .push("The security deposit is $0 in the cost scenario; nothing to simulate.".into());
    }

    if !monthly.is_empty() && monthly.expected > 0 {
        let mut bill = payment(
            ITEM_OCCUPANCY_BILL,
            "Recurring monthly rent + NNN bill (simulated)",
            PayeeRole::Landlord,
            PlannedOperation::CreateRecurringBill,
            ApprovalAction::FinancialCommitment,
            &monthly,
            BASIS_SCENARIO,
        );
        bill.recurring_day = Some(day);
        bill.notes.push(
            "Creating the bill records the monthly obligation in the sandbox; no money moves until a payment.".into(),
        );
        push(bill);
        let mut first = payment(
            ITEM_FIRST_MONTH,
            "First month's rent + NNN (simulated)",
            PayeeRole::Landlord,
            PlannedOperation::PayBill,
            ApprovalAction::FinancialCommitment,
            &monthly,
            BASIS_SCENARIO,
        );
        first.depends_on = Some(ITEM_OCCUPANCY_BILL.into());
        first.notes.push(
            "Paid at move-in; the cost scenario's upfront cash figure does not include it.".into(),
        );
        push(first);
    } else if !monthly.is_empty() {
        notes_zero.push(
            "Monthly occupancy cost is $0 in the cost scenario (rent may be unknown); no rent is simulated.".into(),
        );
    }

    if !renovation.is_empty() && renovation.expected > 0 {
        let mut p = payment(
            ITEM_RENOVATION,
            "Build-out: renovation work items (simulated)",
            PayeeRole::Contractor,
            PlannedOperation::Purchase,
            ApprovalAction::Purchase,
            &renovation,
            BASIS_SCENARIO,
        );
        p.notes.push(
            "Unit costs are illustrative planning ranges, not contractor quotes; a real build-out is paid in stages.".into(),
        );
        push(p);
    }

    let furnishing = match bom {
        Some(b) => {
            let (amount, basis) = match (b.status.as_str(), b.approved_total_minor) {
                ("APPROVED", Some(t)) => (t, "bill of materials, approved total"),
                _ => (
                    b.estimated_total_minor,
                    "bill of materials, estimated total (not approved; illustrative prices)",
                ),
            };
            let g = Group {
                low: amount,
                expected: amount,
                high: amount,
                ids: Vec::new(),
            };
            Some((g, basis.to_owned(), true))
        }
        None if !ffe.is_empty() => Some((ffe, BASIS_SCENARIO.to_owned(), false)),
        None => None,
    };
    if let Some((g, basis, from_bom)) = furnishing {
        if g.expected > 0 {
            let mut p = payment(
                ITEM_FURNISHING,
                "Build-out: furnishing and equipment (simulated)",
                PayeeRole::FurnishingSupplier,
                PlannedOperation::Purchase,
                ApprovalAction::Purchase,
                &g,
                &basis,
            );
            if from_bom {
                p.notes.push(
                    "The bill of materials may price items (for example flooring or hand sinks) that renovation work items also cover; check for overlap.".into(),
                );
            }
            push(p);
        } else {
            notes_zero.push("Furnishing total is $0; nothing to simulate.".into());
        }
    }

    let reserves = if contingency.is_empty() {
        Vec::new()
    } else {
        vec![PlannedReserve {
            reserve_id: RESERVE_CONTINGENCY.into(),
            label: "Renovation contingency".into(),
            amount_low_minor: contingency.low,
            amount_expected_minor: contingency.expected,
            amount_high_minor: contingency.high,
            source_line_item_ids: contingency.ids.clone(),
            note: "Held back for overruns. It is not paid to anyone, so it is not simulated as a payment, but it is part of the cash to have at move-in.".into(),
        }]
    };

    let moving: Vec<&PlannedPayment> = payments.iter().filter(|p| p.moves_money).collect();
    let move_in_payments_expected_minor =
        checked_sum(moving.iter().map(|p| p.amount_expected_minor))?;
    let move_in_payments_high_minor = checked_sum(moving.iter().map(|p| p.amount_high_minor))?;
    let reserves_expected_minor = checked_sum(reserves.iter().map(|r| r.amount_expected_minor))?;
    let reserves_high_minor = checked_sum(reserves.iter().map(|r| r.amount_high_minor))?;
    let not_simulated_expected_minor = checked_sum(not_simulated.iter().map(|l| l.expected_minor))?;

    let mut assumptions = vec![
        "Every amount is mock money in a simulation; nothing here is a payment, a paid deposit or a signed commitment.".to_owned(),
        "Amounts are the cost scenario's expected planning estimates, not quotes or invoices; low and high are shown for context.".to_owned(),
        "Move-in cash = the planned payments that move money + reserves. It includes the first month's rent and NNN, which the cost scenario's upfront cash figure excludes.".to_owned(),
        "Each planned payment is simulated only after its own owner approval of the exact payload.".to_owned(),
        "The landlord, contractor and furnishing supplier are stand-in sandbox merchants, not the real parties.".to_owned(),
    ];
    assumptions.extend(notes_zero);
    if scenario.monthly_operating_cost != monthly.expected {
        assumptions.push(format!(
            "The scenario's monthly_operating_cost ({}) differs from the sum of its MONTHLY lines ({}); the lines are used.",
            scenario.monthly_operating_cost, monthly.expected
        ));
    }
    assumptions.extend(
        scenario
            .assumptions
            .iter()
            .map(|a| format!("Cost scenario: {a}")),
    );
    assumptions.extend(
        scenario
            .excluded_costs
            .iter()
            .map(|e| format!("Not included (cost scenario): {e}")),
    );

    Ok(MoveInPaymentPlan {
        simulation,
        disclaimer: simulation.disclaimer(),
        project_id: scenario.project_id,
        property_id: scenario.property_id,
        scenario_id: scenario.scenario_id,
        design_version_id: bom
            .map(|b| b.design_version_id)
            .or(scenario.design_version_id),
        bom_id: bom.map(|b| b.bom_id),
        currency: "USD".into(),
        payments,
        reserves,
        not_simulated,
        move_in_payments_expected_minor,
        move_in_payments_high_minor,
        reserves_expected_minor,
        reserves_high_minor,
        move_in_cash_expected_minor: move_in_payments_expected_minor
            .checked_add(reserves_expected_minor)
            .ok_or_else(|| PlanError::Overflow("move-in cash".into()))?,
        move_in_cash_high_minor: move_in_payments_high_minor
            .checked_add(reserves_high_minor)
            .ok_or_else(|| PlanError::Overflow("move-in cash".into()))?,
        scenario_upfront_cash_expected_minor: scenario.upfront_cash_expected,
        monthly_recurring_minor: monthly.expected,
        not_simulated_expected_minor,
        assumptions,
    })
}

/// The sandbox ids a plan's payments are sent to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxTargets {
    /// The sandbox account that pays (Checking or Savings).
    pub account_id: String,
    pub landlord_merchant_id: Option<String>,
    pub contractor_merchant_id: Option<String>,
    pub supplier_merchant_id: Option<String>,
    /// The recurring occupancy bill, once created (for the first month's payment).
    pub occupancy_bill_id: Option<String>,
}

impl MoveInPaymentPlan {
    pub fn payment(&self, plan_item_id: &str) -> Option<&PlannedPayment> {
        self.payments
            .iter()
            .find(|p| p.plan_item_id == plan_item_id)
    }

    /// The exact intent (and so the approval payload) for one planned payment.
    pub fn intent_for(
        &self,
        plan_item_id: &str,
        targets: &SandboxTargets,
    ) -> Result<PaymentIntent, PlanError> {
        let p = self
            .payment(plan_item_id)
            .ok_or_else(|| PlanError::UnknownItem(plan_item_id.to_owned()))?;
        validate_id("account_id", &targets.account_id).map_err(PlanError::Invalid)?;
        let merchant = |m: &Option<String>, what: &str| {
            m.clone()
                .ok_or_else(|| PlanError::MissingTarget(format!("{what} merchant id")))
        };
        let operation = match p.operation {
            PlannedOperation::Purchase => {
                let merchant_id = match p.payee_role {
                    PayeeRole::Landlord => merchant(&targets.landlord_merchant_id, "landlord")?,
                    PayeeRole::Contractor => {
                        merchant(&targets.contractor_merchant_id, "contractor")?
                    }
                    PayeeRole::FurnishingSupplier => {
                        merchant(&targets.supplier_merchant_id, "furnishing supplier")?
                    }
                };
                PaymentOperation::Purchase {
                    account_id: targets.account_id.clone(),
                    merchant_id,
                }
            }
            PlannedOperation::CreateRecurringBill => PaymentOperation::CreateRecurringBill {
                account_id: targets.account_id.clone(),
                payee: LANDLORD_BILL_PAYEE.into(),
                nickname: OCCUPANCY_BILL_NICKNAME.into(),
                recurring_day: p.recurring_day.unwrap_or(DEFAULT_RECURRING_DAY),
            },
            PlannedOperation::PayBill => PaymentOperation::PayBill {
                bill_id: targets.occupancy_bill_id.clone().ok_or_else(|| {
                    PlanError::MissingTarget(format!(
                        "occupancy bill id (execute {ITEM_OCCUPANCY_BILL} first)"
                    ))
                })?,
            },
        };
        let intent = PaymentIntent {
            simulation: self.simulation,
            project_id: self.project_id,
            property_id: self.property_id,
            scenario_id: self.scenario_id,
            plan_item_id: p.plan_item_id.clone(),
            action: p.approval_action,
            operation,
            amount_minor: p.amount_expected_minor,
            currency: self.currency.clone(),
            description: truncate(&format!("SIMULATION (Second Site plan): {}", p.label), 199),
        };
        intent.validate().map_err(PlanError::Invalid)?;
        Ok(intent)
    }
}
