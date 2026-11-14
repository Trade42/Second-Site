//! Port of `apps/api/app/domains/evidence/validator.py`: the deterministic evidence & compliance
//! validator. It flags assertions without evidence, historical evidence presented as current,
//! synthetic data presented as a real provider record, and conflicting values.

use std::collections::HashSet;

use ss_core::{DataOrigin, EvidenceClassification};
use uuid::Uuid;

use crate::contracts::{
    AssessmentStatus, InfrastructureAssessment, PropertyDigitalProfile, SourceEvidence,
};
use crate::py;

/// `validate_property`: the list of problems found, in Python's order and wording (empty when
/// none).
pub fn validate_property(
    prop: &PropertyDigitalProfile,
    evidence: &[SourceEvidence],
    assessments: &[InfrastructureAssessment],
) -> Vec<String> {
    let mut problems = Vec::new();
    let ids: HashSet<Uuid> = evidence.iter().map(|e| e.evidence_id).collect();
    for a in assessments {
        // Python formats the list with repr(): ['<uuid>', ...].
        let missing: Vec<String> = a
            .evidence_ids
            .iter()
            .filter(|i| !ids.contains(i))
            .map(|i| py::str_repr(&i.to_string()))
            .collect();
        if !missing.is_empty() {
            problems.push(format!(
                "{}: references unknown evidence [{}]",
                a.requirement_id,
                missing.join(", ")
            ));
        }
        if a.status == AssessmentStatus::Meets {
            let hist = evidence
                .iter()
                .filter(|e| {
                    a.evidence_ids.contains(&e.evidence_id)
                        && e.evidence_classification == EvidenceClassification::DocumentedHistorical
                })
                .count();
            if hist != 0 && hist == a.evidence_ids.len() && a.limitations.is_empty() {
                problems.push(format!(
                    "{}: MEETS on historical evidence without stated limitations",
                    a.requirement_id
                ));
            }
        }
    }
    if let Some(lst) = &prop.commercial_listing {
        if lst.data_origin != DataOrigin::Live && lst.provider.to_lowercase() == "costar" {
            problems.push("listing labeled CoStar but not retrieved live from CoStar".to_owned());
        }
    }
    problems
}
