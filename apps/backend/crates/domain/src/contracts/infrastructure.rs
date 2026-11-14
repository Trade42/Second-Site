//! Port of `apps/api/app/contracts/infrastructure.py`.

use serde::{Deserialize, Serialize};
use ss_core::{new_id, string_enum, Checker, Contract, EvidenceClassification, Validate};
use uuid::Uuid;

use super::values::ScalarValue;

string_enum! {
    pub enum RequirementCategory {
        Electrical = "ELECTRICAL",
        Plumbing = "PLUMBING",
        Hvac = "HVAC",
        Space = "SPACE",
        Accessibility = "ACCESSIBILITY",
        FireLifeSafety = "FIRE_LIFE_SAFETY",
        Zoning = "ZONING",
    }
}

string_enum! {
    pub enum Severity {
        /// Failing it disqualifies the property.
        Hard = "HARD",
        /// Failing it adds cost / risk.
        Soft = "SOFT",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfrastructureRequirement {
    /// Stable template key, e.g. 'facial_studio.plumbing.treatment_sinks'.
    pub requirement_id: String,
    pub business_type: String,
    pub requirement_category: RequirementCategory,
    pub description: String,
    #[serde(default)]
    pub required_value: Option<ScalarValue>,
    #[serde(default)]
    pub required_unit: Option<String>,
    pub severity: Severity,
    /// How a professional would verify it.
    pub verification_method: String,
}

impl Validate for InfrastructureRequirement {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for InfrastructureRequirement {
    const NAME: &'static str = "InfrastructureRequirement";
}

string_enum! {
    pub enum AssessmentStatus {
        Meets = "MEETS",
        MayRequireWork = "MAY_REQUIRE_WORK",
        Fails = "FAILS",
        /// Never treated as MEETS.
        Unknown = "UNKNOWN",
    }
}

string_enum! {
    #[derive(Default)]
    pub enum ReviewStatus {
        #[default]
        Automated = "AUTOMATED",
        NeedsReview = "NEEDS_REVIEW",
        Reviewed = "REVIEWED",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfrastructureObservation {
    #[serde(default = "new_id")]
    pub observation_id: Uuid,
    #[serde(default)]
    pub property_id: Option<Uuid>,
    pub observation_type: String,
    #[serde(default)]
    pub value: Option<ScalarValue>,
    #[serde(default)]
    pub unit: Option<String>,
    pub classification: EvidenceClassification,
    #[serde(default)]
    pub evidence_ids: Vec<Uuid>,
    #[serde(default)]
    pub limitations: Vec<String>,
    pub description: String,
}

impl Validate for InfrastructureObservation {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for InfrastructureObservation {
    const NAME: &'static str = "InfrastructureObservation";
}

pub const MEETS_NEEDS_EVIDENCE: &str = "MEETS requires supporting evidence_ids";
pub const UNKNOWN_NEEDS_QUESTIONS: &str = "UNKNOWN assessments must list verification_questions";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfrastructureAssessment {
    #[serde(default = "new_id")]
    pub assessment_id: Uuid,
    pub project_id: Uuid,
    pub property_id: Uuid,
    pub requirement_id: String,
    pub status: AssessmentStatus,
    #[serde(default)]
    pub observed_value: Option<ScalarValue>,
    #[serde(default)]
    pub required_value: Option<ScalarValue>,
    #[serde(default)]
    pub evidence_ids: Vec<Uuid>,
    #[serde(default)]
    pub limitations: Vec<String>,
    #[serde(default)]
    pub proposed_work: Vec<String>,
    #[serde(default)]
    pub verification_questions: Vec<String>,
    /// Plain-language statement of what is not known.
    pub uncertainty: String,
    #[serde(default)]
    pub review_status: ReviewStatus,
}

impl Validate for InfrastructureAssessment {
    fn check(&self, c: &mut Checker) {
        let mark = c.error_count();
        c.model_rules(mark, |c| {
            if self.status == AssessmentStatus::Meets && self.evidence_ids.is_empty() {
                c.value_error(MEETS_NEEDS_EVIDENCE);
            } else if self.status == AssessmentStatus::Unknown
                && self.verification_questions.is_empty()
            {
                c.value_error(UNKNOWN_NEEDS_QUESTIONS);
            }
        });
    }
}

impl Contract for InfrastructureAssessment {
    const NAME: &'static str = "InfrastructureAssessment";
}
