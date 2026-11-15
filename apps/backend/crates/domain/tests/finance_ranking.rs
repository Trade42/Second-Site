//! Port of `apps/api/tests/test_finance_ranking.py` (assertion for assertion), plus engine
//! tests for the truth rules the Python suite does not exercise directly: broker-reported
//! evidence, historical permits never becoming MEETS, the evidence validator, rounding.

mod common;

use common::analysis::{analyze_in_memory, demo_profile};
use common::to_value;
use serde_json::{json, Value};
use ss_core::indexmap::IndexMap;
use ss_core::{BusinessExpansionProfile, Contract, DataOrigin, EvidenceClassification};
use ss_domain::contracts::{
    AssessmentStatus, CostKind, InfrastructureAssessment, PropertyDigitalProfile,
    RenovationWorkItem, SourceEvidence,
};
use ss_domain::evidence::validate_property;
use ss_domain::feasibility::FeasibilityEngine;
use ss_domain::finance::{apply_bp, work_item_totals, FinancialCalculationService};
use ss_domain::py;
use ss_domain::ranking::{usd, RankingEngine};
use ss_domain::requirements::templates::requirements_for;
use ss_domain::DomainError;
use uuid::Uuid;

const ALH: &str = "5380 E ALHAMBRA AVE";
const HW: &str = "6922 W HOLLYWOOD BLVD";

#[test]
fn test_cheaper_rent_property_ranks_lower_over_36_months() {
    // Acceptance (agents-finance): lower rent but more renovation -> more expensive over 36
    // months. opening_capital=None isolates the cost tradeoff from the capital constraint.
    let profile = demo_profile(json!({"opening_capital": null}));
    let r = analyze_in_memory(&profile, None);
    let (alh, hw) = (&r.props[ALH], &r.props[HW]);
    let rent = |p: &PropertyDigitalProfile| {
        p.commercial_listing
            .as_ref()
            .unwrap()
            .asking_rent_monthly
            .unwrap()
    };
    assert!(rent(alh) < rent(hw));
    assert!(
        r.scenarios[&alh.property_id].long_term_cost > r.scenarios[&hw.property_id].long_term_cost
    );
    assert_eq!(r.ranking.ranked_properties[0].property_id, hw.property_id);
    assert!(!r.ranking.ranked_properties.iter().any(|x| x.disqualified));
    assert!(r.ranking.explanations[&alh.property_id.to_string()].contains("Lower rent than"));
}

#[test]
fn test_tradeoff_flips_over_a_long_horizon() {
    let profile = demo_profile(json!({"opening_capital": null}));
    let r = analyze_in_memory(&profile, Some(120));
    assert!(
        r.scenarios[&r.props[ALH].property_id].long_term_cost
            < r.scenarios[&r.props[HW].property_id].long_term_cost
    );
}

#[test]
fn test_calculation_is_deterministic() {
    let p = demo_profile(json!({"opening_capital": null}));
    let costs = |p: &BusinessExpansionProfile| {
        let mut v: Vec<i64> = analyze_in_memory(p, None)
            .scenarios
            .values()
            .map(|s| s.long_term_cost)
            .collect();
        v.sort();
        v
    };
    assert_eq!(costs(&p), costs(&p));
}

#[test]
fn test_unknown_infrastructure_stays_unknown() {
    let r = analyze_in_memory(&demo_profile(json!({})), None);
    let elec = |pid: &Uuid| -> InfrastructureAssessment {
        r.assessments[pid]
            .iter()
            .find(|a| a.requirement_id.ends_with("electrical.tenant_service"))
            .unwrap()
            .clone()
    };
    for p in r.props.values() {
        let e = elec(&p.property_id);
        // Historical 800A service != available capacity.
        assert_eq!(e.status, AssessmentStatus::Unknown);
        assert!(!e.verification_questions.is_empty());
    }
    let alh_elec = elec(&r.props[ALH].property_id);
    assert!(alh_elec
        .observed_value
        .unwrap()
        .as_str()
        .unwrap()
        .contains("800A"));
    assert!(!alh_elec.evidence_ids.is_empty());
}

#[test]
fn test_capital_constraint_disqualifies() {
    let r = analyze_in_memory(&demo_profile(json!({"opening_capital": 15000000})), None);
    let alh = &r.props[ALH];
    assert!(r.scenarios[&alh.property_id].upfront_cash_expected > 15000000);
    let x = r
        .ranking
        .ranked_properties
        .iter()
        .find(|x| x.property_id == alh.property_id)
        .unwrap();
    assert!(x.disqualified);
    assert_eq!(
        r.ranking.ranked_properties.last().unwrap().property_id,
        alh.property_id
    );
}

#[test]
fn test_no_double_counting() {
    let profile = demo_profile(json!({}));
    let r = analyze_in_memory(&profile, None);
    let hw = &r.props[HW];
    let s = &r.scenarios[&hw.property_id];
    let one_time: Vec<_> = s
        .line_items
        .iter()
        .filter(|li| li.kind == CostKind::OneTime)
        .collect();
    assert_eq!(
        s.opening_cost_expected,
        one_time.iter().map(|li| li.expected).sum::<i64>()
    );
    let deposit = s
        .line_items
        .iter()
        .find(|li| li.kind == CostKind::RefundableDeposit)
        .unwrap();
    assert_eq!(
        s.long_term_cost,
        s.opening_cost_expected + s.monthly_operating_cost * s.comparison_horizon_months
    );
    // Deposit counted once, not in the long-term cost.
    assert_eq!(
        s.upfront_cash_expected,
        s.opening_cost_expected + deposit.expected
    );
    let reno: i64 = s
        .line_items
        .iter()
        .filter(|li| li.line_item_id.starts_with("work:"))
        .map(|li| li.expected)
        .sum();
    let contingency = s
        .line_items
        .iter()
        .find(|li| li.line_item_id == "contingency:renovation")
        .unwrap();
    // Contingency on renovation only, not rent/FF&E. Python: round(reno * 0.15).
    assert_eq!(
        contingency.expected,
        py::round(reno as f64 * 0.15, 0) as i64
    );
    let monthly: i64 = s
        .line_items
        .iter()
        .filter(|li| li.kind == CostKind::Monthly)
        .map(|li| li.expected)
        .sum();
    assert_eq!(monthly, s.monthly_operating_cost);
    let mut twice = r.work_items[&hw.property_id].clone();
    twice.push(twice[0].clone());
    match FinancialCalculationService.scenario(&profile, hw, &twice, None, None) {
        Err(DomainError::Value(msg)) => {
            assert!(msg.contains("counted twice"), "{msg}");
            assert_eq!(
                msg,
                format!("work item {} counted twice", twice[0].work_item_id)
            );
        }
        other => panic!("expected ValueError, got {other:?}"),
    }
}

#[test]
fn test_money_precision_uses_integer_cents() {
    let p = demo_profile(json!({}));
    let r = analyze_in_memory(&p, None);
    let w = RenovationWorkItem::from_json_value(json!({
        "property_id": Uuid::new_v4(),
        "category": "FINISHES",
        "description": "x",
        "quantity": "139.4",
        "unit": "m2",
        "unit_cost_low": 9001,
        "unit_cost_expected": 15001,
        "unit_cost_high": 25001,
    }))
    .unwrap();
    let s = FinancialCalculationService
        .scenario(&p, &r.props[HW], &[w], None, None)
        .unwrap();
    let line = s
        .line_items
        .iter()
        .find(|li| li.line_item_id.starts_with("work:"))
        .unwrap();
    // 139.4 * 15001 = 2091139.4 -> half-up to cents.
    assert_eq!(line.expected, 2091139);
    // long_term_cost is an integer type (i64) by construction; check the wire form too.
    assert!(to_value(&s)["long_term_cost"].is_i64());
}

// ---------------------------------------------------------------------------------------------
// Beyond the Python suite.

fn uuid(n: u32) -> Uuid {
    Uuid::parse_str(&format!("45224ebd-0000-4000-8000-{n:012x}")).unwrap()
}

fn broker_evidence(
    prop: &PropertyDigitalProfile,
    n: u32,
    kind: &str,
    value: Value,
) -> SourceEvidence {
    SourceEvidence::from_json_value(json!({
        "evidence_id": uuid(n),
        "project_id": prop.project_id,
        "property_id": prop.property_id,
        "provider": "broker_call",
        "data_origin": "FIXTURE_SYNTHETIC",
        "observation_type": kind,
        "extracted_value": value,
        "evidence_classification": "OWNER_REPORTED",
        "limitations": ["SIMULATED CALL: scripted demo answer, not a real conversation."],
    }))
    .unwrap()
}

fn find<'a>(a: &'a [InfrastructureAssessment], suffix: &str) -> &'a InfrastructureAssessment {
    a.iter()
        .find(|x| x.requirement_id.ends_with(suffix))
        .unwrap()
}

#[test]
fn historical_permits_never_meet_capacity_but_broker_reports_are_used_with_limits() {
    let profile = demo_profile(json!({}));
    let r = analyze_in_memory(&profile, None);
    let alh = &r.props[ALH];
    let reqs = requirements_for(&profile);
    let mut ev = r.evidence[&alh.property_id].clone();
    // The permit-only run: electrical is UNKNOWN, never MEETS, and costs an allowance.
    let (a, w) = FeasibilityEngine
        .evaluate(&profile, &reqs, alh, &ev)
        .unwrap();
    assert_eq!(
        find(&a, "electrical.tenant_service").status,
        AssessmentStatus::Unknown
    );
    assert!(a.iter().all(|x| x.status != AssessmentStatus::Meets
        || !x.requirement_id.ends_with("electrical.tenant_service")));
    assert!(w.iter().any(|x| x
        .assumptions
        .contains(&"cost_key=electrical_subpanel_allowance".to_owned())));
    // A broker-reported 200A panel meets 100A, but says it is unverified.
    ev.push(broker_evidence(alh, 1, "tenant_subpanel_amps", json!(200)));
    ev.push(broker_evidence(alh, 2, "in_unit_plumbing", json!(true)));
    ev.push(broker_evidence(alh, 3, "hvac_zonable", json!(false)));
    let (a, w) = FeasibilityEngine
        .evaluate(&profile, &reqs, alh, &ev)
        .unwrap();
    let e = find(&a, "electrical.tenant_service");
    assert_eq!(e.status, AssessmentStatus::Meets);
    assert_eq!(
        e.observed_value.as_ref().unwrap().as_str(),
        Some("200A tenant panel (broker-reported)")
    );
    assert_eq!(e.evidence_ids[0], uuid(1));
    assert!(e
        .limitations
        .last()
        .unwrap()
        .starts_with("Reported capacity is not verified"));
    let sinks = find(&a, "plumbing.treatment_sinks");
    assert_eq!(sinks.status, AssessmentStatus::MayRequireWork);
    assert_eq!(sinks.evidence_ids, vec![uuid(2)]);
    // Broker said "existing plumbing", so the owner preference note does not apply.
    assert!(!sinks
        .limitations
        .iter()
        .any(|l| l.starts_with("Owner prefers")));
    let hvac = find(&a, "hvac.ventilation");
    assert_eq!(
        hvac.observed_value.as_ref().unwrap().as_str(),
        Some("Broker reports HVAC cannot be zoned per room")
    );
    let keys: Vec<&str> = w
        .iter()
        .flat_map(|x| x.assumptions.iter())
        .filter_map(|s| s.strip_prefix("cost_key="))
        .collect();
    assert!(keys.contains(&"electrical_circuits_reported_capacity"));
    assert!(keys.contains(&"sink_extend_from_existing"));
    assert!(keys.contains(&"hvac_new_distribution"));
    // Evidence for another property, or of a non-usable classification, is ignored.
    let mut other = broker_evidence(alh, 4, "tenant_subpanel_amps", json!(400));
    other.property_id = Some(Uuid::new_v4());
    let mut unknown = broker_evidence(alh, 5, "tenant_subpanel_amps", json!(400));
    unknown.evidence_classification = EvidenceClassification::Unknown;
    let base = r.evidence[&alh.property_id].clone();
    let (a, _) = FeasibilityEngine
        .evaluate(&profile, &reqs, alh, &[base, vec![other, unknown]].concat())
        .unwrap();
    assert_eq!(
        find(&a, "electrical.tenant_service").status,
        AssessmentStatus::Unknown
    );
}

#[test]
fn every_engine_output_validates_and_conforms_to_its_schema() {
    let r = analyze_in_memory(&demo_profile(json!({})), None);
    for p in r.props.values() {
        for a in &r.assessments[&p.property_id] {
            a.validate().unwrap();
            common::assert_conforms("InfrastructureAssessment", &to_value(a));
        }
        for w in &r.work_items[&p.property_id] {
            w.validate().unwrap();
            assert!(w.requires_professional_quote);
            common::assert_conforms("RenovationWorkItem", &to_value(w));
        }
        let s = &r.scenarios[&p.property_id];
        common::assert_conforms("CostScenario", &to_value(s));
        for li in &s.line_items {
            common::assert_conforms("CostLineItem", &to_value(li));
        }
        assert!(validate_property(
            p,
            &r.evidence[&p.property_id],
            &r.assessments[&p.property_id]
        )
        .is_empty());
    }
    common::assert_conforms("PropertyRankingResult", &to_value(&r.ranking));
    for h in &r.ranking.hard_constraint_results {
        common::assert_conforms("HardConstraintResult", &to_value(h));
    }
    for x in &r.ranking.ranked_properties {
        common::assert_conforms("RankedProperty", &to_value(x));
    }
}

#[test]
fn evidence_validator_flags_python_problems() {
    let r = analyze_in_memory(&demo_profile(json!({})), None);
    let hw = r.props[HW].clone();
    let ev = r.evidence[&hw.property_id].clone();
    let mut a = r.assessments[&hw.property_id].clone();
    let ghost = uuid(99);
    a[0].evidence_ids.push(ghost);
    // MEETS on historical evidence only, with no limitations.
    let water = a
        .iter_mut()
        .find(|x| x.requirement_id.ends_with("plumbing.water_service"))
        .unwrap();
    assert_eq!(water.status, AssessmentStatus::Meets);
    water.limitations.clear();
    let mut costar = hw.clone();
    let listing = costar.commercial_listing.as_mut().unwrap();
    listing.provider = "CoStar".into();
    listing.data_origin = DataOrigin::FixtureSynthetic;
    let problems = validate_property(&costar, &ev, &a);
    assert_eq!(
        problems,
        vec![
            format!("facial_studio.space.min_area: references unknown evidence ['{ghost}']"),
            "facial_studio.plumbing.water_service: MEETS on historical evidence without stated limitations".to_owned(),
            "listing labeled CoStar but not retrieved live from CoStar".to_owned(),
        ]
    );
    listing_live(&mut costar);
    assert_eq!(validate_property(&costar, &ev, &a).len(), 2);
}

fn listing_live(p: &mut PropertyDigitalProfile) {
    p.commercial_listing.as_mut().unwrap().data_origin = DataOrigin::Live;
}

#[test]
fn rounding_is_half_up_on_cents_and_dollars() {
    // Python: Decimal quantize ROUND_HALF_UP (ties away from zero), never banker's rounding.
    assert_eq!(apply_bp(10, 1500).unwrap(), 2); // 1.5 -> 2
    assert_eq!(apply_bp(30, 1500).unwrap(), 5); // 4.5 -> 5 (banker's would give 4)
    assert_eq!(apply_bp(-30, 1500).unwrap(), -5);
    assert_eq!(apply_bp(29, 1500).unwrap(), 4); // 4.35 -> 4
    assert_eq!(usd(250), "$3");
    assert_eq!(usd(-40), "$-0");
    assert_eq!(usd(123456789), "$1,234,568");
    let w = RenovationWorkItem::from_json_value(json!({
        "property_id": Uuid::new_v4(), "category": "OTHER", "description": "x",
        "quantity": "2.5", "unit": "u",
        "unit_cost_low": 1, "unit_cost_expected": 3, "unit_cost_high": 5,
    }))
    .unwrap();
    // 2.5, 7.5, 12.5 -> 3, 8, 13.
    assert_eq!(work_item_totals(&w).unwrap(), (3, 8, 13));
}

#[test]
fn ranking_rejects_all_zero_priorities_and_bad_versions() {
    let r = analyze_in_memory(&demo_profile(json!({})), None);
    let props: Vec<PropertyDigitalProfile> = r.props.values().cloned().collect();
    let zero = demo_profile(json!({"ranking_priorities": {
        "total_cost": 0.0, "infrastructure_fit": 0.0, "evidence_certainty": 0.0}}));
    match RankingEngine.rank(&zero, &props, &r.scenarios, &r.assessments, 1) {
        Err(DomainError::Value(m)) => assert_eq!(m, "ranking priorities must not all be zero"),
        other => panic!("{other:?}"),
    }
    let p = demo_profile(json!({}));
    match RankingEngine.rank(&p, &props, &r.scenarios, &r.assessments, 0) {
        Err(DomainError::Validation(e)) => {
            assert_eq!(e.title, "PropertyRankingResult");
            assert_eq!(e.errors_json()[0]["loc"], json!(["ranking_version"]));
            assert_eq!(e.errors[0].kind, "greater_than_equal");
        }
        other => panic!("{other:?}"),
    }
    // Properties missing from both maps rank with no scenario and no assessments.
    let empty = RankingEngine
        .rank(&p, &props, &IndexMap::new(), &IndexMap::new(), 3)
        .unwrap();
    assert!(empty
        .ranked_properties
        .iter()
        .all(|x| x.long_term_cost.is_none()));
    assert!(empty.cost_scenario_ids.is_empty());
}

#[test]
fn zero_rooms_complete_unlike_pydantic() {
    // required_rooms all zero: Python builds an enclosed-rooms work item of quantity 0 and
    // Pydantic rejects it. Rust deliberately gives no partitions work item and an UNKNOWN
    // assessment instead (see FeasibilityEngine::evaluate and tests/empty_program.rs).
    let profile = demo_profile(json!({"space_requirements": {"required_rooms": {"treatment": 0}}}));
    let r = analyze_in_memory(&demo_profile(json!({})), None);
    let hw = &r.props[HW];
    let (a, w) = FeasibilityEngine
        .evaluate(
            &profile,
            &requirements_for(&profile),
            hw,
            &r.evidence[&hw.property_id],
        )
        .unwrap();
    let rooms = a
        .iter()
        .find(|x| x.requirement_id.ends_with("space.enclosed_rooms"))
        .unwrap();
    assert_eq!(rooms.status, AssessmentStatus::Unknown);
    assert!(w.iter().all(|x| x.category.as_str() != "PARTITIONS"));
    assert!(w.iter().all(|x| !x.quantity.0.is_zero()));
}

#[test]
fn ranking_and_area_boundaries_match_python() {
    // Expected values from the Python engines on the same inputs (independent review). They pin
    // rules no other test separates: limits pass on equality, a zero long-term cost is left out of
    // the cost scores, the uncertainty penalty is capped at 1.0, and an area equal to the
    // requirement meets it.
    let profile = demo_profile(json!({"opening_capital": 11969650, "monthly_rent_limit": 600000}));
    let r = analyze_in_memory(&profile, None);
    let (alh, hw) = (r.props[ALH].property_id, r.props[HW].property_id);
    assert_eq!(r.scenarios[&hw].upfront_cash_expected, 11969650);
    let mut scenarios = r.scenarios.clone();
    scenarios.get_mut(&alh).unwrap().long_term_cost = 0;
    let mut assessments = r.assessments.clone();
    let list = assessments.get_mut(&hw).unwrap();
    let unknown = list
        .iter()
        .find(|a| a.status == AssessmentStatus::Unknown)
        .unwrap()
        .clone();
    list.extend((0..21).map(|i| InfrastructureAssessment {
        requirement_id: format!("extra.{i}"),
        ..unknown.clone()
    }));
    let props: Vec<PropertyDigitalProfile> = r.props.values().cloned().collect();
    let out = RankingEngine
        .rank(&profile, &props, &scenarios, &assessments, 1)
        .unwrap();
    let passed = |pid: Uuid, c: &str| {
        let h = out.hard_constraint_results.iter();
        h.filter(|h| h.property_id == pid && h.constraint == c)
            .map(|h| h.passed)
            .collect::<Vec<_>>()
    };
    assert_eq!(passed(hw, "monthly_rent_limit"), vec![Some(true)]); // $6,000 vs $6,000
    assert_eq!(passed(hw, "opening_capital"), vec![Some(true)]); // $119,697 vs $119,697
    assert_eq!(passed(alh, "opening_capital"), vec![Some(false)]);
    let (a, h) = (alh.to_string(), hw.to_string());
    assert_eq!(out.uncertainty_penalties[&h], 1.0); // 22 UNKNOWN x 0.05, capped
    assert_eq!(out.uncertainty_penalties[&a], 0.1);
    assert_eq!(out.normalized_scores[&a]["total_cost"], 0.0);
    assert_eq!(out.normalized_scores[&h]["total_cost"], 1.0);
    let ranked: Vec<(Uuid, f64, bool, i64)> = out
        .ranked_properties
        .iter()
        .map(|x| {
            (
                x.property_id,
                x.total_score,
                x.disqualified,
                x.unknown_requirement_count,
            )
        })
        .collect();
    assert_eq!(
        ranked,
        vec![(hw, -0.425185, false, 22), (alh, 0.173334, true, 2)]
    );
    // Listing area 148.6 m2 against a required 148.6 m2: MAY_REQUIRE_WORK, not FAILS.
    let p = demo_profile(json!({"space_requirements": {
        "required_rooms": {"treatment": 3, "reception": 1, "storage": 1}, "min_area_sqm": 148.6}}));
    let reqs = requirements_for(&p);
    let (a, _) = FeasibilityEngine
        .evaluate(&p, &reqs, &r.props[ALH], &r.evidence[&alh])
        .unwrap();
    let area = find(&a, "space.min_area");
    assert_eq!(area.status, AssessmentStatus::MayRequireWork);
    assert_eq!(
        area.required_value.as_ref().and_then(|v| v.as_f64()),
        Some(148.6)
    );
}
