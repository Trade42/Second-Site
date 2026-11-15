//! A profile with no room program (`required_rooms: {}` or only zero counts) must not fail the
//! feasibility engine. Python fails it (a partitions work item of quantity 0, rejected by
//! Pydantic); Rust gives no partitions work item and says the program is not stated. See
//! `FeasibilityEngine::evaluate` for the rule.

mod common;

use common::analysis::{analyze_in_memory, demo_profile};
use serde_json::json;
use ss_domain::contracts::{AssessmentStatus, ScalarValue, WorkCategory};
use ss_domain::feasibility::engine::{NO_PROGRAM_AREA, NO_PROGRAM_QUESTION, NO_PROGRAM_ROOMS};
use ss_domain::feasibility::FeasibilityEngine;
use ss_domain::finance::calculator::NO_PROGRAM_EXCLUDED;
use ss_domain::requirements::templates::requirements_for;
use ss_domain::DomainError;

fn check_no_program(rooms: serde_json::Value) {
    let profile = demo_profile(json!({"space_requirements": {"required_rooms": rooms}}));
    let r = analyze_in_memory(&profile, None);
    assert!(!r.props.is_empty());
    for p in r.props.values() {
        let a = &r.assessments[&p.property_id];
        let by = |suffix: &str| a.iter().find(|x| x.requirement_id.ends_with(suffix));
        let rooms = by("space.enclosed_rooms").expect("enclosed rooms assessed");
        assert_eq!(rooms.status, AssessmentStatus::Unknown);
        assert_eq!(rooms.uncertainty, NO_PROGRAM_ROOMS);
        assert!(rooms.proposed_work.is_empty());
        assert_eq!(rooms.verification_questions, vec![NO_PROGRAM_QUESTION]);
        let area = by("space.min_area").expect("area assessed");
        assert_eq!(area.status, AssessmentStatus::Unknown);
        assert_eq!(area.uncertainty, NO_PROGRAM_AREA);
        assert_eq!(area.required_value, Some(ScalarValue::Number(0.0)));
        // No sinks requirement without treatment rooms (templates omit it, as in Python).
        assert!(by("plumbing.treatment_sinks").is_none());

        let w = &r.work_items[&p.property_id];
        assert!(w.iter().all(|x| x.category != WorkCategory::Partitions));
        assert!(w.iter().all(|x| x.quantity.0 > rust_decimal::Decimal::ZERO));
        assert!(w.iter().any(|x| x.category == WorkCategory::PermitsFees));

        let s = &r.scenarios[&p.property_id];
        assert_eq!(
            s.excluded_costs.last().map(String::as_str),
            Some(NO_PROGRAM_EXCLUDED)
        );
        assert!(s.line_items.iter().all(|li| li.category != "FF&E"));
        assert!(s.opening_cost_expected > 0);
    }
    assert_eq!(r.ranking.ranked_properties.len(), r.props.len());
}

#[test]
fn empty_room_program_completes() {
    check_no_program(json!({}));
}

#[test]
fn zero_count_room_program_completes() {
    check_no_program(json!({"treatment": 0}));
    check_no_program(json!({"treatment": 0, "office": 0}));
}

#[test]
fn stated_min_area_is_still_assessed_without_rooms() {
    let profile = demo_profile(json!({"space_requirements": {
        "required_rooms": {}, "min_area_sqm": 100.0}}));
    let r = analyze_in_memory(&profile, None);
    for p in r.props.values() {
        let area = r.assessments[&p.property_id]
            .iter()
            .find(|x| x.requirement_id.ends_with("space.min_area"))
            .unwrap();
        assert_ne!(area.uncertainty, NO_PROGRAM_AREA);
        assert_eq!(area.required_value, Some(ScalarValue::Number(100.0)));
        assert_ne!(area.status, AssessmentStatus::Unknown, "{area:?}");
    }
}

#[test]
fn negative_counts_still_fail() {
    // {"storage": 1, "restroom": -1}: total 0 from nonsense counts is not "no program".
    let base = analyze_in_memory(&demo_profile(json!({})), None);
    let hw = base.props.values().next().unwrap();
    for rooms in [
        json!({"storage": 1, "restroom": -1}),
        json!({"treatment": -2}),
    ] {
        let profile = demo_profile(json!({"space_requirements": {"required_rooms": rooms}}));
        match FeasibilityEngine.evaluate(
            &profile,
            &requirements_for(&profile),
            hw,
            &base.evidence[&hw.property_id],
        ) {
            Err(DomainError::Validation(e)) => {
                assert_eq!(e.title, "RenovationWorkItem");
                assert!(e.mentions("Input should be greater than 0"), "{e:?}");
            }
            other => panic!("{rooms}: {other:?}"),
        }
    }
}

#[test]
fn a_real_program_keeps_partitions() {
    let profile = demo_profile(json!({}));
    let r = analyze_in_memory(&profile, None);
    for p in r.props.values() {
        assert!(r.work_items[&p.property_id]
            .iter()
            .any(|x| x.category == WorkCategory::Partitions));
        assert!(!r.scenarios[&p.property_id]
            .excluded_costs
            .iter()
            .any(|x| x == NO_PROGRAM_EXCLUDED));
    }
}
