//! Port of `apps/api/app/contracts/finance.py` (types and validators only; the calculator and
//! ranking engine are part 2).

use std::collections::HashSet;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use ss_core::{
    new_id, string_enum, utcnow, Checker, Contract, CurrencyCode, MinorUnits, NonNegativeMinor,
    UtcDateTime, Validate,
};
use uuid::Uuid;

use super::values::PyDecimal;

string_enum! {
    pub enum WorkCategory {
        Plumbing = "PLUMBING",
        Electrical = "ELECTRICAL",
        Hvac = "HVAC",
        Partitions = "PARTITIONS",
        Finishes = "FINISHES",
        Accessibility = "ACCESSIBILITY",
        PermitsFees = "PERMITS_FEES",
        DesignProfessional = "DESIGN_PROFESSIONAL",
        Other = "OTHER",
    }
}

fn yes() -> bool {
    true
}

pub const UNIT_COSTS_ORDERED: &str = "unit costs must satisfy low <= expected <= high";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenovationWorkItem {
    #[serde(default = "new_id")]
    pub work_item_id: Uuid,
    pub property_id: Uuid,
    pub category: WorkCategory,
    pub description: String,
    /// `> 0`.
    pub quantity: PyDecimal,
    pub unit: String,
    #[serde(default)]
    pub currency: CurrencyCode,
    pub unit_cost_low: NonNegativeMinor,
    pub unit_cost_expected: NonNegativeMinor,
    pub unit_cost_high: NonNegativeMinor,
    #[serde(default = "yes")]
    pub requires_professional_quote: bool,
    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default)]
    pub evidence_ids: Vec<Uuid>,
    #[serde(default)]
    pub source_assessment_id: Option<Uuid>,
}

impl Validate for RenovationWorkItem {
    fn check(&self, c: &mut Checker) {
        let mark = c.error_count();
        c.gt("quantity", self.quantity, PyDecimal::ZERO);
        c.ge("unit_cost_low", self.unit_cost_low, 0);
        c.ge("unit_cost_expected", self.unit_cost_expected, 0);
        c.ge("unit_cost_high", self.unit_cost_high, 0);
        c.model_rules(mark, |c| {
            if !(self.unit_cost_low <= self.unit_cost_expected
                && self.unit_cost_expected <= self.unit_cost_high)
            {
                c.value_error(UNIT_COSTS_ORDERED);
            }
        });
    }
}

impl Contract for RenovationWorkItem {
    const NAME: &'static str = "RenovationWorkItem";
}

string_enum! {
    pub enum CostKind {
        OneTime = "ONE_TIME",
        Monthly = "MONTHLY",
        /// Cash requirement, excluded from long-term cost.
        RefundableDeposit = "REFUNDABLE_DEPOSIT",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostLineItem {
    /// Deterministic key; duplicates are rejected.
    pub line_item_id: String,
    pub category: String,
    pub description: String,
    pub kind: CostKind,
    pub low: MinorUnits,
    pub expected: MinorUnits,
    pub high: MinorUnits,
    #[serde(default)]
    pub source_work_item_id: Option<Uuid>,
    #[serde(default)]
    pub evidence_ids: Vec<Uuid>,
}

impl Validate for CostLineItem {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for CostLineItem {
    const NAME: &'static str = "CostLineItem";
}

pub const DUPLICATE_LINE_ITEMS: &str = "duplicate line_item_id in cost scenario (double counting)";
pub const OPENING_COSTS_ORDERED: &str = "opening costs must satisfy low <= expected <= high";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostScenario {
    #[serde(default = "new_id")]
    pub scenario_id: Uuid,
    pub project_id: Uuid,
    pub property_id: Uuid,
    #[serde(default)]
    pub design_version_id: Option<Uuid>,
    #[serde(default)]
    pub currency: CurrencyCode,
    pub opening_cost_low: NonNegativeMinor,
    pub opening_cost_expected: NonNegativeMinor,
    pub opening_cost_high: NonNegativeMinor,
    /// Opening cost + refundable deposits: cash needed before opening.
    pub upfront_cash_expected: NonNegativeMinor,
    pub monthly_operating_cost: NonNegativeMinor,
    /// `> 0`.
    pub comparison_horizon_months: i64,
    /// opening_cost_expected + monthly_operating_cost * horizon (deposits excluded).
    pub long_term_cost: NonNegativeMinor,
    pub contingency: NonNegativeMinor,
    pub line_items: Vec<CostLineItem>,
    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default)]
    pub excluded_costs: Vec<String>,
    #[serde(default)]
    pub source_evidence_ids: Vec<Uuid>,
    #[serde(default = "utcnow")]
    pub calculated_at: UtcDateTime,
}

impl Validate for CostScenario {
    fn check(&self, c: &mut Checker) {
        let mark = c.error_count();
        c.ge("opening_cost_low", self.opening_cost_low, 0);
        c.ge("opening_cost_expected", self.opening_cost_expected, 0);
        c.ge("opening_cost_high", self.opening_cost_high, 0);
        c.ge("upfront_cash_expected", self.upfront_cash_expected, 0);
        c.ge("monthly_operating_cost", self.monthly_operating_cost, 0);
        c.gt(
            "comparison_horizon_months",
            self.comparison_horizon_months,
            0,
        );
        c.ge("long_term_cost", self.long_term_cost, 0);
        c.ge("contingency", self.contingency, 0);
        c.each("line_items", &self.line_items);
        c.model_rules(mark, |c| {
            let mut seen = HashSet::new();
            if !self
                .line_items
                .iter()
                .all(|li| seen.insert(li.line_item_id.as_str()))
            {
                c.value_error(DUPLICATE_LINE_ITEMS);
            } else if !(self.opening_cost_low <= self.opening_cost_expected
                && self.opening_cost_expected <= self.opening_cost_high)
            {
                c.value_error(OPENING_COSTS_ORDERED);
            }
        });
    }
}

impl Contract for CostScenario {
    const NAME: &'static str = "CostScenario";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardConstraintResult {
    pub property_id: Uuid,
    pub constraint: String,
    /// None when it cannot be determined (unknown). Required on input (may be null).
    #[serde(deserialize_with = "required_nullable")]
    pub passed: Option<bool>,
    pub detail: String,
}

/// Pydantic `field: X | None` without a default is required but may be null.
fn required_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

impl Validate for HardConstraintResult {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for HardConstraintResult {
    const NAME: &'static str = "HardConstraintResult";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankedProperty {
    /// `>= 1`.
    pub rank: i64,
    pub property_id: Uuid,
    pub total_score: f64,
    #[serde(default)]
    pub disqualified: bool,
    #[serde(default)]
    pub long_term_cost: Option<NonNegativeMinor>,
    #[serde(default)]
    pub unknown_requirement_count: i64,
}

impl Validate for RankedProperty {
    fn check(&self, c: &mut Checker) {
        c.ge("rank", self.rank, 1);
        c.ge_opt("long_term_cost", self.long_term_cost, 0);
    }
}

impl Contract for RankedProperty {
    const NAME: &'static str = "RankedProperty";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyRankingResult {
    #[serde(default = "new_id")]
    pub ranking_id: Uuid,
    pub project_id: Uuid,
    /// `>= 1`.
    pub ranking_version: i64,
    pub criteria_weights: IndexMap<String, f64>,
    pub hard_constraint_results: Vec<HardConstraintResult>,
    pub ranked_properties: Vec<RankedProperty>,
    /// property_id -> criterion -> score in [0,1].
    pub normalized_scores: IndexMap<String, IndexMap<String, f64>>,
    /// property_id -> penalty in [0,1].
    pub uncertainty_penalties: IndexMap<String, f64>,
    pub cost_scenario_ids: Vec<Uuid>,
    /// property_id -> plain-language explanation.
    pub explanations: IndexMap<String, String>,
    #[serde(default = "utcnow")]
    pub calculated_at: UtcDateTime,
}

impl Validate for PropertyRankingResult {
    fn check(&self, c: &mut Checker) {
        c.ge("ranking_version", self.ranking_version, 1);
        c.each("hard_constraint_results", &self.hard_constraint_results);
        c.each("ranked_properties", &self.ranked_properties);
    }
}

impl Contract for PropertyRankingResult {
    const NAME: &'static str = "PropertyRankingResult";
}
