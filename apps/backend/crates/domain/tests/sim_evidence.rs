//! Synthetic evidence (a simulated call's scripted answer) never moves a listing that is not
//! itself synthetic; for the synthetic demo listing it counts as before. The rule is
//! `ss_domain::feasibility::engine::counts_for`, applied in `FeasibilityEngine::evaluate`.
//! Deliberate difference from Python, which counts evidence whatever its origin.

mod common;

use common::analysis::{analyze_in_memory, demo_profile};
use serde_json::json;
use ss_core::{DataOrigin, EvidenceClassification};
use ss_domain::contracts::{
    InfrastructureAssessment, PropertyDigitalProfile, RenovationWorkItem, SourceEvidence,
};
use ss_domain::feasibility::engine::{counts_for, SIMULATED_NOT_COUNTED};
use ss_domain::feasibility::FeasibilityEngine;
use ss_domain::requirements::templates::requirements_for;
use uuid::Uuid;

/// An answer from a call, as `call_orchestration` stores it: OWNER_REPORTED, with the call's
/// origin (`FIXTURE_SYNTHETIC` for a simulated call, `LIVE` for a real one).
fn answer(template: &SourceEvidence, claim: &str, origin: DataOrigin) -> SourceEvidence {
    SourceEvidence {
        evidence_id: Uuid::new_v4(),
        provider: "broker_call".into(),
        data_origin: origin,
        observation_type: claim.into(),
        extracted_value: json!(true),
        unit: None,
        evidence_classification: EvidenceClassification::OwnerReported,
        limitations: vec!["SIMULATED CALL: scripted demo answer, not a real conversation.".into()],
        ..template.clone()
    }
}

/// What a decision rests on, without the ids that are new on every run.
type Shape = (Vec<String>, Vec<String>);

fn shape(a: &[InfrastructureAssessment], w: &[RenovationWorkItem]) -> Shape {
    (
        a.iter()
            .map(|x| {
                let limits: Vec<&String> = x
                    .limitations
                    .iter()
                    .filter(|l| *l != SIMULATED_NOT_COUNTED)
                    .collect();
                format!(
                    "{} {:?} {:?} {:?} {:?} {:?}",
                    x.requirement_id,
                    x.status,
                    x.observed_value,
                    x.evidence_ids,
                    x.proposed_work,
                    limits
                )
            })
            .collect(),
        w.iter()
            .map(|x| {
                format!(
                    "{:?} {} {:?} {:?} {:?} {:?} {:?}",
                    x.category,
                    x.description,
                    x.quantity,
                    x.unit_cost_low,
                    x.unit_cost_expected,
                    x.unit_cost_high,
                    x.evidence_ids
                )
            })
            .collect(),
    )
}

struct Case {
    prop: PropertyDigitalProfile,
    evidence: Vec<SourceEvidence>,
    run: Box<
        dyn Fn(
            &PropertyDigitalProfile,
            &[SourceEvidence],
        ) -> (Vec<InfrastructureAssessment>, Vec<RenovationWorkItem>),
    >,
}

/// A fixture property whose plumbing and HVAC are not documented in the unit, so that a call's
/// answers on both would change its assessments and work items.
fn case() -> Case {
    let profile = demo_profile(json!({}));
    let r = analyze_in_memory(&profile, None);
    let reqs = requirements_for(&profile);
    let run = move |p: &PropertyDigitalProfile, ev: &[SourceEvidence]| {
        FeasibilityEngine
            .evaluate(&profile, &reqs, p, ev)
            .expect("evaluates")
    };
    for p in r.props.values() {
        let ev = r.evidence[&p.property_id].clone();
        let Some(template) = ev.first().cloned() else {
            continue;
        };
        let mut with = ev.clone();
        with.push(answer(
            &template,
            "in_unit_plumbing",
            DataOrigin::FixtureSynthetic,
        ));
        with.push(answer(
            &template,
            "hvac_zonable",
            DataOrigin::FixtureSynthetic,
        ));
        let (a0, w0) = run(p, &ev);
        let (a1, w1) = run(p, &with);
        let costs = |w: &[RenovationWorkItem]| -> Vec<String> {
            w.iter()
                .map(|x| format!("{:?}", x.unit_cost_expected))
                .collect()
        };
        if shape(&a0, &w0).0 != shape(&a1, &w1).0 && costs(&w0) != costs(&w1) {
            return Case {
                prop: p.clone(),
                evidence: ev,
                run: Box::new(run),
            };
        }
    }
    panic!("no fixture property is moved by plumbing and HVAC answers");
}

fn with_origin(p: &PropertyDigitalProfile, origin: DataOrigin) -> PropertyDigitalProfile {
    let mut p = p.clone();
    if let Some(l) = p.commercial_listing.as_mut() {
        l.data_origin = origin;
    }
    p
}

fn answers(c: &Case, origin: DataOrigin) -> Vec<SourceEvidence> {
    let t = &c.evidence[0];
    let mut ev = c.evidence.clone();
    ev.push(answer(t, "in_unit_plumbing", origin));
    ev.push(answer(t, "hvac_zonable", origin));
    ev
}

#[test]
fn simulated_answers_do_not_move_a_real_listing() {
    let c = case();
    for origin in [
        DataOrigin::FixtureReal,
        DataOrigin::Live,
        DataOrigin::UserProvided,
    ] {
        let p = with_origin(&c.prop, origin);
        // The fixture's own evidence must count for this comparison to be about the call alone.
        assert!(c.evidence.iter().all(|e| counts_for(&p, e)), "{origin:?}");
        let (a0, w0) = (c.run)(&p, &c.evidence);
        let with = answers(&c, DataOrigin::FixtureSynthetic);
        let (a1, w1) = (c.run)(&p, &with);
        assert_eq!(shape(&a0, &w0), shape(&a1, &w1), "{origin:?}");
        // Before the call no assessment carries the line; after it exactly the two rules that
        // read those answers do, once each.
        assert!(a0
            .iter()
            .all(|a| !a.limitations.iter().any(|l| l == SIMULATED_NOT_COUNTED)));
        let said: Vec<&str> = a1
            .iter()
            .filter(|a| a.limitations.iter().any(|l| l == SIMULATED_NOT_COUNTED))
            .map(|a| a.requirement_id.as_str())
            .collect();
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said[0].ends_with("plumbing.treatment_sinks"), "{said:?}");
        assert!(said[1].ends_with("hvac.ventilation"), "{said:?}");
        for a in &a1 {
            let n = a
                .limitations
                .iter()
                .filter(|l| *l == SIMULATED_NOT_COUNTED)
                .count();
            assert!(n <= 1, "{a:?}");
            // The left-out evidence is not cited as support.
            assert!(with[c.evidence.len()..]
                .iter()
                .all(|e| !a.evidence_ids.contains(&e.evidence_id)));
        }
    }
}

#[test]
fn simulated_answers_count_for_the_synthetic_listing() {
    let c = case();
    let p = &c.prop;
    assert_eq!(
        p.commercial_listing.as_ref().map(|l| l.data_origin),
        Some(DataOrigin::FixtureSynthetic)
    );
    let (a0, w0) = (c.run)(p, &c.evidence);
    let (a1, w1) = (c.run)(p, &answers(&c, DataOrigin::FixtureSynthetic));
    assert_ne!(shape(&a0, &w0), shape(&a1, &w1));
    assert!(a1
        .iter()
        .all(|a| !a.limitations.iter().any(|l| l == SIMULATED_NOT_COUNTED)));
    let observed = |suffix: &str| {
        a1.iter()
            .find(|a| a.requirement_id.ends_with(suffix))
            .and_then(|a| a.observed_value.clone())
            .map(|v| format!("{v:?}"))
            .unwrap_or_default()
    };
    assert!(observed("plumbing.treatment_sinks").contains("Broker reports existing in-unit"));
    assert!(observed("hvac.ventilation").contains("can be zoned"));
}

#[test]
fn a_real_calls_answers_count_for_a_real_listing() {
    let c = case();
    let p = with_origin(&c.prop, DataOrigin::FixtureReal);
    let (a0, w0) = (c.run)(&p, &c.evidence);
    let (a1, w1) = (c.run)(&p, &answers(&c, DataOrigin::Live));
    assert_ne!(shape(&a0, &w0), shape(&a1, &w1));
    assert!(a1
        .iter()
        .all(|a| !a.limitations.iter().any(|l| l == SIMULATED_NOT_COUNTED)));
    // And exactly as for the synthetic listing with the same answers.
    let (a2, w2) = (c.run)(&c.prop, &answers(&c, DataOrigin::Live));
    assert_eq!(shape(&a1, &w1).1.len(), shape(&a2, &w2).1.len());
}

#[test]
fn a_property_without_a_listing_is_not_demo_data() {
    let c = case();
    let mut p = c.prop.clone();
    p.commercial_listing = None;
    let with = answers(&c, DataOrigin::FixtureSynthetic);
    assert!(with[c.evidence.len()..].iter().all(|e| !counts_for(&p, e)));
    assert!(with[c.evidence.len()..]
        .iter()
        .all(|e| counts_for(&c.prop, e)));
}
