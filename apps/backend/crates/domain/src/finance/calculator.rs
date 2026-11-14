//! Port of `apps/api/app/domains/finance/calculator.py`: `FinancialCalculationService`,
//! deterministic integer-cent arithmetic. No floats in money.
//!
//! Double-counting guards (as in Python):
//! - each work item contributes exactly one line (keyed by `work_item_id`); duplicates are
//!   rejected;
//! - contingency applies to the renovation subtotal only (never to rent, deposits or FF&E);
//! - the refundable security deposit counts toward upfront cash, NOT toward long-term cost;
//! - monthly costs (rent + NNN) appear once as MONTHLY lines; long-term cost multiplies them once.
//!
//! Rounding: Python rounds in exactly two places, both `Decimal.quantize(Decimal("1"),
//! ROUND_HALF_UP)` (ties away from zero): a work item's `quantity * unit_cost`
//! ([`work_item_totals`]) and the contingency's `amount * bp / 10000` ([`apply_bp`]). Every other
//! operation is integer addition or multiplication, done here with overflow checks because
//! Python's integers are unbounded.

use std::collections::HashSet;

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use ss_core::{new_id, utcnow, BusinessExpansionProfile, Contract, CurrencyCode};
use uuid::Uuid;

use super::cost_catalog::{CONTINGENCY_RATE_BP, FF_E_PER_TREATMENT_ROOM};
use crate::contracts::{
    CostKind, CostLineItem, CostScenario, PropertyDigitalProfile, RenovationWorkItem,
};
use crate::error::DomainError;

fn overflow(what: &str) -> DomainError {
    DomainError::Overflow(what.to_owned())
}

pub(crate) fn add(a: i64, b: i64, what: &str) -> Result<i64, DomainError> {
    a.checked_add(b).ok_or_else(|| overflow(what))
}

pub(crate) fn mul(a: i64, b: i64, what: &str) -> Result<i64, DomainError> {
    a.checked_mul(b).ok_or_else(|| overflow(what))
}

fn sum<'a>(mut values: impl Iterator<Item = &'a i64>, what: &str) -> Result<i64, DomainError> {
    values.try_fold(0i64, |acc, v| add(acc, *v, what))
}

/// `_round_cents`: `int(d.quantize(Decimal("1"), rounding=ROUND_HALF_UP))`.
pub fn round_cents(d: Decimal) -> Result<i64, DomainError> {
    d.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
        .to_i64()
        .ok_or_else(|| overflow("rounded cents"))
}

/// `work_item_totals`: `(low, expected, high)` = `quantity * unit cost`, each rounded half-up to
/// whole cents.
pub fn work_item_totals(w: &RenovationWorkItem) -> Result<(i64, i64, i64), DomainError> {
    let q = w.quantity.0;
    let total = |unit_cost: i64| {
        q.checked_mul(Decimal::from(unit_cost))
            .ok_or_else(|| overflow("work item total"))
            .and_then(round_cents)
    };
    Ok((
        total(w.unit_cost_low)?,
        total(w.unit_cost_expected)?,
        total(w.unit_cost_high)?,
    ))
}

/// `apply_bp`: `amount * bp / 10000` rounded half-up (away from zero) to whole cents. Computed
/// exactly in 128-bit integers, which equals Python's exact `Decimal` arithmetic here.
pub fn apply_bp(amount: i64, bp: i64) -> Result<i64, DomainError> {
    let x = i128::from(amount) * i128::from(bp);
    let (q, r) = (x / 10000, x % 10000);
    let rounded = if r.abs() * 2 >= 10000 {
        q + r.signum()
    } else {
        q
    };
    i64::try_from(rounded).map_err(|_| overflow("basis-point amount"))
}

/// `FinancialCalculationService`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FinancialCalculationService;

impl FinancialCalculationService {
    pub fn new() -> Self {
        Self
    }

    /// `FinancialCalculationService.scenario`. `horizon_months` of `None` or `0` uses the
    /// profile's `comparison_horizon_months` (Python's `horizon_months or ...`).
    ///
    /// Errors: [`DomainError::Value`] when a work item appears twice (Python's `ValueError`,
    /// same message); [`DomainError::Validation`] when the resulting `CostScenario` is invalid
    /// (for example a non-positive horizon); [`DomainError::Overflow`] when a sum leaves `i64`.
    pub fn scenario(
        &self,
        profile: &BusinessExpansionProfile,
        prop: &PropertyDigitalProfile,
        work_items: &[RenovationWorkItem],
        horizon_months: Option<i64>,
        design_version_id: Option<Uuid>,
    ) -> Result<CostScenario, DomainError> {
        let horizon = match horizon_months {
            Some(h) if h != 0 => h,
            _ => profile.ranking_priorities.comparison_horizon_months,
        };
        let listing = prop.commercial_listing.as_ref();
        let mut lines: Vec<CostLineItem> = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        let mut reno = [0i64; 3];
        let mut evidence: Vec<Uuid> = Vec::new();
        for w in work_items {
            if !seen.insert(w.work_item_id) {
                return Err(DomainError::Value(format!(
                    "work item {} counted twice",
                    w.work_item_id
                )));
            }
            let (lo, ex, hi) = work_item_totals(w)?;
            reno = [
                add(reno[0], lo, "renovation subtotal")?,
                add(reno[1], ex, "renovation subtotal")?,
                add(reno[2], hi, "renovation subtotal")?,
            ];
            evidence.extend_from_slice(&w.evidence_ids);
            lines.push(CostLineItem {
                line_item_id: format!("work:{}", w.work_item_id),
                category: w.category.as_str().to_owned(),
                description: w.description.clone(),
                kind: CostKind::OneTime,
                low: lo,
                expected: ex,
                high: hi,
                source_work_item_id: Some(w.work_item_id),
                evidence_ids: w.evidence_ids.clone(),
            });
        }
        let cont = [
            apply_bp(reno[0], CONTINGENCY_RATE_BP)?,
            apply_bp(reno[1], CONTINGENCY_RATE_BP)?,
            apply_bp(reno[2], CONTINGENCY_RATE_BP)?,
        ];
        lines.push(CostLineItem {
            line_item_id: "contingency:renovation".into(),
            category: "CONTINGENCY".into(),
            // Python: f"Contingency {CONTINGENCY_RATE_BP / 100:.0f}% of renovation subtotal".
            description: format!(
                "Contingency {:.0}% of renovation subtotal",
                CONTINGENCY_RATE_BP as f64 / 100.0
            ),
            kind: CostKind::OneTime,
            low: cont[0],
            expected: cont[1],
            high: cont[2],
            source_work_item_id: None,
            evidence_ids: Vec::new(),
        });
        let rooms = profile
            .space_requirements
            .required_rooms
            .get("treatment")
            .copied()
            .unwrap_or(0);
        let ffe = mul(rooms, FF_E_PER_TREATMENT_ROOM, "FF&E allowance")?;
        if ffe != 0 {
            lines.push(CostLineItem {
                line_item_id: "ffe:treatment_rooms".into(),
                category: "FF&E".into(),
                description: format!("Furniture/equipment allowance x{rooms} treatment rooms"),
                kind: CostKind::OneTime,
                low: ffe,
                expected: ffe,
                high: ffe,
                source_work_item_id: None,
                evidence_ids: Vec::new(),
            });
        }
        let rent = listing.and_then(|l| l.asking_rent_monthly);
        let nnn = listing.and_then(|l| l.nnn_charges_monthly).unwrap_or(0);
        let excluded = vec![
            "Tenant improvement allowance (not provided)".to_owned(),
            "Free rent concessions".to_owned(),
            "Broker fees".to_owned(),
            "Business operating costs other than occupancy (payroll, supplies)".to_owned(),
        ];
        let mut assumptions = vec![
            format!("Comparison horizon {horizon} months"),
            "Rent and NNN held flat (no escalations)".to_owned(),
            "Unit costs are illustrative planning ranges, not quotes".to_owned(),
        ];
        let nnn_is_estimate = listing.is_some_and(|l| l.nnn_is_estimate);
        if nnn_is_estimate {
            assumptions.push(
                "NNN/CAM not quoted by the listing; derived estimate used (confirm with broker)"
                    .to_owned(),
            );
        } else if listing.is_some_and(|l| l.nnn_charges_monthly.is_none()) {
            assumptions
                .push("NNN/CAM charges unknown — monthly cost may be understated".to_owned());
        }
        let rent = match rent {
            Some(r) => r,
            None => {
                assumptions.push("Asking rent unknown — monthly cost understated".to_owned());
                0
            }
        };
        lines.push(CostLineItem {
            line_item_id: "occupancy:rent".into(),
            category: "RENT".into(),
            description: "Base rent".into(),
            kind: CostKind::Monthly,
            low: rent,
            expected: rent,
            high: rent,
            source_work_item_id: None,
            evidence_ids: Vec::new(),
        });
        lines.push(CostLineItem {
            line_item_id: "occupancy:nnn".into(),
            category: "NNN".into(),
            description: format!(
                "NNN / CAM charges{}",
                if nnn_is_estimate {
                    " (estimated; not quoted by the listing)"
                } else {
                    ""
                }
            ),
            kind: CostKind::Monthly,
            low: nnn,
            expected: nnn,
            high: nnn,
            source_work_item_id: None,
            evidence_ids: Vec::new(),
        });
        let deposit = match listing.and_then(|l| l.security_deposit) {
            Some(d) if d != 0 => d,
            _ => 0,
        };
        if deposit != 0 {
            lines.push(CostLineItem {
                line_item_id: "deposit:security".into(),
                category: "DEPOSIT".into(),
                description:
                    "Refundable security deposit (upfront cash; excluded from long-term cost)"
                        .into(),
                kind: CostKind::RefundableDeposit,
                low: deposit,
                expected: deposit,
                high: deposit,
                source_work_item_id: None,
                evidence_ids: Vec::new(),
            });
        }

        let one_time: Vec<&CostLineItem> = lines
            .iter()
            .filter(|li| li.kind == CostKind::OneTime)
            .collect();
        let opening = [
            sum(one_time.iter().map(|li| &li.low), "opening cost")?,
            sum(one_time.iter().map(|li| &li.expected), "opening cost")?,
            sum(one_time.iter().map(|li| &li.high), "opening cost")?,
        ];
        let monthly = sum(
            lines
                .iter()
                .filter(|li| li.kind == CostKind::Monthly)
                .map(|li| &li.expected),
            "monthly cost",
        )?;
        let long_term = add(
            opening[1],
            mul(monthly, horizon, "long-term cost")?,
            "long-term cost",
        )?;
        let mut source_evidence_ids: Vec<Uuid> = Vec::new();
        let mut seen_evidence: HashSet<Uuid> = HashSet::new();
        for e in evidence {
            if seen_evidence.insert(e) {
                source_evidence_ids.push(e);
            }
        }
        let scenario = CostScenario {
            scenario_id: new_id(),
            project_id: prop.project_id,
            property_id: prop.property_id,
            design_version_id,
            currency: CurrencyCode::default(),
            opening_cost_low: opening[0],
            opening_cost_expected: opening[1],
            opening_cost_high: opening[2],
            upfront_cash_expected: add(opening[1], deposit, "upfront cash")?,
            monthly_operating_cost: monthly,
            comparison_horizon_months: horizon,
            long_term_cost: long_term,
            contingency: cont[1],
            line_items: lines,
            assumptions,
            excluded_costs: excluded,
            source_evidence_ids,
            calculated_at: utcnow(),
        };
        scenario.validate()?;
        Ok(scenario)
    }
}
