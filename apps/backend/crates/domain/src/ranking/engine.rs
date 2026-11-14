//! Port of `apps/api/app/domains/ranking/engine.py`: hard constraints + weighted normalized
//! criteria + an explicit uncertainty penalty. Fully deterministic and explainable; no LLM.
//!
//! Float semantics follow CPython: `round(x, 6)` ([`py::round`]), int/int true division
//! ([`py::int_true_div`]) and the built-in `sum()` with Neumaier compensation
//! ([`py::sum_floats`]). Tie-breaking is Python's stable sort on
//! `(disqualified, -total_score, str(property_id))`.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use ss_core::{new_id, utcnow, BusinessExpansionProfile, Contract};
use uuid::Uuid;

use crate::contracts::{
    AssessmentStatus, CostScenario, HardConstraintResult, InfrastructureAssessment,
    PropertyDigitalProfile, PropertyRankingResult, RankedProperty, Severity,
};
use crate::error::DomainError;
use crate::py;
use crate::requirements::templates::requirements_for;

pub const UNCERTAINTY_PENALTY_PER_UNKNOWN: f64 = 0.05;

/// The three criteria, in Python's dict order.
pub const CRITERIA: [&str; 3] = ["total_cost", "infrastructure_fit", "evidence_certainty"];

fn usd_wide(cents: i128) -> String {
    let (q, r) = (cents / 100, cents % 100);
    // Decimal(cents) / 100 quantized half-up (ties away from zero), formatted with `,`.
    let dollars = if r.abs() * 2 >= 100 {
        q + r.signum()
    } else {
        q
    };
    // Python keeps the sign of a negative amount that rounds to zero: `$-0`.
    let sign = if dollars == 0 && cents < 0 { "-" } else { "" };
    format!("${sign}{}", py::group_thousands(dollars))
}

/// `_usd`: whole dollars, rounded half-up like the UI, with thousands separators (`$1,234`).
pub fn usd(cents: i64) -> String {
    usd_wide(i128::from(cents))
}

/// `RankingEngine`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RankingEngine;

impl RankingEngine {
    pub fn new() -> Self {
        Self
    }

    /// `RankingEngine.rank`. `scenarios` and `assessments` are keyed by property id; the order of
    /// `scenarios` gives `cost_scenario_ids` (Python's dict order). Properties missing from either
    /// map are ranked with no scenario / no assessments, as in Python.
    ///
    /// Errors: [`DomainError::Value`] ("ranking priorities must not all be zero") when the
    /// weights sum to `<= 0`; [`DomainError::Validation`] for a `ranking_version < 1`.
    pub fn rank(
        &self,
        profile: &BusinessExpansionProfile,
        properties: &[PropertyDigitalProfile],
        scenarios: &IndexMap<Uuid, CostScenario>,
        assessments: &IndexMap<Uuid, Vec<InfrastructureAssessment>>,
        ranking_version: i64,
    ) -> Result<PropertyRankingResult, DomainError> {
        let pr = &profile.ranking_priorities;
        let raw = [pr.total_cost, pr.infrastructure_fit, pr.evidence_certainty];
        let total_w = py::sum_floats(raw);
        if total_w <= 0.0 {
            return Err(DomainError::Value(
                "ranking priorities must not all be zero".into(),
            ));
        }
        let weights: IndexMap<String, f64> = CRITERIA
            .iter()
            .zip(raw)
            .map(|(k, v)| ((*k).to_owned(), py::round(v / total_w, 6)))
            .collect();
        let hard_reqs: HashSet<String> = requirements_for(profile)
            .into_iter()
            .filter(|r| r.severity == Severity::Hard)
            .map(|r| r.requirement_id)
            .collect();
        let no_assessments: Vec<InfrastructureAssessment> = Vec::new();
        let assessments_of = |pid: &Uuid| assessments.get(pid).unwrap_or(&no_assessments);

        let mut constraints: Vec<HardConstraintResult> = Vec::new();
        let mut disq: HashSet<Uuid> = HashSet::new();
        for p in properties {
            let pid = p.property_id;
            for a in assessments_of(&pid) {
                if hard_reqs.contains(&a.requirement_id) {
                    let passed = match a.status {
                        AssessmentStatus::Fails => Some(false),
                        AssessmentStatus::Unknown => None,
                        _ => Some(true),
                    };
                    constraints.push(HardConstraintResult {
                        property_id: pid,
                        constraint: a.requirement_id.clone(),
                        passed,
                        detail: format!("{}: {}", a.status.as_str(), a.uncertainty),
                    });
                    if passed == Some(false) {
                        disq.insert(pid);
                    }
                }
            }
            let rent = p
                .commercial_listing
                .as_ref()
                .and_then(|l| l.asking_rent_monthly);
            if let (Some(limit), Some(rent)) = (profile.monthly_rent_limit, rent) {
                let ok = rent <= limit;
                constraints.push(HardConstraintResult {
                    property_id: pid,
                    constraint: "monthly_rent_limit".into(),
                    passed: Some(ok),
                    detail: format!("rent {} vs limit {}", usd(rent), usd(limit)),
                });
                if !ok {
                    disq.insert(pid);
                }
            }
            if let (Some(capital), Some(sc)) = (profile.opening_capital, scenarios.get(&pid)) {
                let ok = sc.upfront_cash_expected <= capital;
                constraints.push(HardConstraintResult {
                    property_id: pid,
                    constraint: "opening_capital".into(),
                    passed: Some(ok),
                    detail: format!(
                        "expected upfront cash {} vs capital {}",
                        usd(sc.upfront_cash_expected),
                        usd(capital)
                    ),
                });
                if !ok {
                    disq.insert(pid);
                }
            }
        }

        let costs: HashMap<Uuid, i64> = scenarios
            .iter()
            .filter(|(_, s)| s.long_term_cost > 0)
            .map(|(pid, s)| (*pid, s.long_term_cost))
            .collect();
        let min_cost = costs.values().min().copied();
        let mut scores: IndexMap<String, IndexMap<String, f64>> = IndexMap::new();
        let mut penalties: IndexMap<String, f64> = IndexMap::new();
        let mut totals: HashMap<Uuid, f64> = HashMap::new();
        let mut unknown_counts: HashMap<Uuid, i64> = HashMap::new();
        for p in properties {
            let pid = p.property_id;
            let al = assessments_of(&pid);
            let n = if al.is_empty() { 1 } else { al.len() as i64 };
            let count = |s: AssessmentStatus| al.iter().filter(|a| a.status == s).count() as i64;
            let meets = count(AssessmentStatus::Meets);
            let work = count(AssessmentStatus::MayRequireWork);
            let fails = count(AssessmentStatus::Fails);
            let unknown = count(AssessmentStatus::Unknown);
            unknown_counts.insert(pid, unknown);
            let cost_score = match (min_cost, costs.get(&pid)) {
                (Some(m), Some(c)) if m != 0 => {
                    py::round(py::int_true_div(m, *c).unwrap_or(0.0), 6)
                }
                _ => 0.0,
            };
            let fit = py::round(
                (meets as f64 + 0.6 * work as f64) / n as f64 * if fails != 0 { 0.0 } else { 1.0 },
                6,
            );
            let certainty = py::round(1.0 - py::int_true_div(unknown, n).unwrap_or(0.0), 6);
            let s: IndexMap<String, f64> = CRITERIA
                .iter()
                .zip([cost_score, fit, certainty])
                .map(|(k, v)| ((*k).to_owned(), v))
                .collect();
            let x = unknown as f64 * UNCERTAINTY_PENALTY_PER_UNKNOWN;
            // min(1.0, x) returns 1.0 unless x is smaller.
            let penalty = py::round(if x < 1.0 { x } else { 1.0 }, 6);
            let weighted = py::sum_floats(CRITERIA.iter().map(|k| weights[*k] * s[*k]));
            totals.insert(pid, py::round(weighted - penalty, 6));
            scores.insert(pid.to_string(), s);
            penalties.insert(pid.to_string(), penalty);
        }

        let mut order: Vec<&PropertyDigitalProfile> = properties.iter().collect();
        let total_of =
            |p: &PropertyDigitalProfile| totals.get(&p.property_id).copied().unwrap_or(0.0);
        // Stable, like Python's `sorted`. Scores are finite for valid profiles; a NaN score
        // (only reachable with infinite weights) is compared as equal.
        order.sort_by(|a, b| {
            disq.contains(&a.property_id)
                .cmp(&disq.contains(&b.property_id))
                .then_with(|| {
                    (-total_of(a))
                        .partial_cmp(&-total_of(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.property_id.to_string().cmp(&b.property_id.to_string()))
        });
        let ranked: Vec<RankedProperty> = order
            .iter()
            .enumerate()
            .map(|(i, p)| RankedProperty {
                rank: i as i64 + 1,
                property_id: p.property_id,
                total_score: total_of(p),
                disqualified: disq.contains(&p.property_id),
                long_term_cost: scenarios.get(&p.property_id).map(|s| s.long_term_cost),
                unknown_requirement_count: unknown_counts.get(&p.property_id).copied().unwrap_or(0),
            })
            .collect();
        for r in &ranked {
            r.validate()?;
        }
        let explanations = explain(&order, scenarios, &unknown_counts, &disq);
        let result = PropertyRankingResult {
            ranking_id: new_id(),
            project_id: profile.project_id,
            ranking_version,
            criteria_weights: weights,
            hard_constraint_results: constraints,
            ranked_properties: ranked,
            normalized_scores: scores,
            uncertainty_penalties: penalties,
            cost_scenario_ids: scenarios.values().map(|s| s.scenario_id).collect(),
            explanations,
            calculated_at: utcnow(),
        };
        result.validate()?;
        Ok(result)
    }
}

/// `RankingEngine._explain`: one plain-language explanation per property, in ranked order.
fn explain(
    order: &[&PropertyDigitalProfile],
    scenarios: &IndexMap<Uuid, CostScenario>,
    unknowns: &HashMap<Uuid, i64>,
    disq: &HashSet<Uuid>,
) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    let rents: HashMap<Uuid, Option<i64>> = order
        .iter()
        .map(|p| {
            (
                p.property_id,
                p.commercial_listing
                    .as_ref()
                    .and_then(|l| l.asking_rent_monthly),
            )
        })
        .collect();
    let rent_of = |pid: &Uuid| rents.get(pid).copied().flatten();
    for p in order {
        let pid = p.property_id;
        let mut parts = vec![format!("{}:", p.property_identity.normalized_address)];
        if disq.contains(&pid) {
            parts.push("disqualified by a hard constraint.".to_owned());
        }
        if let Some(sc) = scenarios.get(&pid) {
            parts.push(format!(
                "expected opening cost {} (range {}–{}), {}/mo occupancy, {}-month total {}.",
                usd(sc.opening_cost_expected),
                usd(sc.opening_cost_low),
                usd(sc.opening_cost_high),
                usd(sc.monthly_operating_cost),
                sc.comparison_horizon_months,
                usd(sc.long_term_cost)
            ));
            for q in order {
                let other = scenarios.get(&q.property_id);
                let (Some(other), Some(mine), Some(theirs)) =
                    (other, rent_of(&pid), rent_of(&q.property_id))
                else {
                    continue;
                };
                if q.property_id == pid {
                    continue;
                }
                if mine < theirs && sc.long_term_cost > other.long_term_cost {
                    parts.push(format!(
                        "Lower rent than {} but {} more over {} months because of higher renovation cost.",
                        q.property_identity.street_address,
                        usd_wide(i128::from(sc.long_term_cost) - i128::from(other.long_term_cost)),
                        sc.comparison_horizon_months
                    ));
                }
            }
        }
        if let Some(u) = unknowns.get(&pid).filter(|u| **u != 0) {
            parts.push(format!(
                "{u} requirement(s) remain UNKNOWN and need verification."
            ));
        }
        out.insert(pid.to_string(), parts.join(" "));
    }
    out
}
