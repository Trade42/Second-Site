//! Port of `apps/api/app/contracts/permits.py`.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use ss_core::{
    new_id, string_enum, Checker, Contract, DataOrigin, EvidenceClassification, MatchStatus,
    Validate,
};
use uuid::Uuid;

use super::values::ScalarValue;

string_enum! {
    pub enum PermitCategory {
        Electrical = "ELECTRICAL",
        Plumbing = "PLUMBING",
        Hvac = "HVAC",
        FireSprinkler = "FIRE_SPRINKLER",
        Building = "BUILDING",
        OtherMechanical = "OTHER_MECHANICAL",
        Other = "OTHER",
    }
}

string_enum! {
    #[derive(Default)]
    pub enum PermitScope {
        /// No unit on the record.
        Building = "BUILDING",
        /// Explicit suite/unit.
        Unit = "UNIT",
        /// Floor/common area designation.
        Floor = "FLOOR",
        #[default]
        Unknown = "UNKNOWN",
    }
}

fn documented_historical() -> EvidenceClassification {
    EvidenceClassification::DocumentedHistorical
}

/// A technical value parsed from a permit description. Permit status != verification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractedInfrastructureFact {
    /// e.g. 'electrical_service_amps', 'water_service_diameter'.
    pub fact_type: String,
    pub value: ScalarValue,
    #[serde(default)]
    pub unit: Option<String>,
    /// Exact substring the value was extracted from.
    pub source_text: String,
    #[serde(default = "documented_historical")]
    pub classification: EvidenceClassification,
    #[serde(default)]
    pub limitations: Vec<String>,
}

impl Validate for ExtractedInfrastructureFact {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for ExtractedInfrastructureFact {
    const NAME: &'static str = "ExtractedInfrastructureFact";
}

fn unmatched() -> MatchStatus {
    MatchStatus::Unmatched
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitRecord {
    #[serde(default = "new_id")]
    pub permit_record_id: Uuid,
    #[serde(default)]
    pub property_id: Option<Uuid>,
    pub permit_number: String,
    pub permit_category: PermitCategory,
    /// Source status verbatim, e.g. 'Permit Finaled'.
    pub permit_status: String,
    #[serde(default)]
    pub issued_date: Option<NaiveDate>,
    /// status_date when status is finaled; otherwise null.
    #[serde(default)]
    pub finaled_date: Option<NaiveDate>,
    pub work_description: String,
    pub original_address: String,
    pub normalized_address: String,
    #[serde(default)]
    pub unit_or_suite: Option<String>,
    #[serde(default)]
    pub scope: PermitScope,
    #[serde(default)]
    pub parcel_id: Option<String>,
    pub source_dataset: String,
    #[serde(default)]
    pub source_url: Option<String>,
    pub data_origin: DataOrigin,
    #[serde(default)]
    pub matched_property_identity: Option<Uuid>,
    #[serde(default = "unmatched")]
    pub match_status: MatchStatus,
    /// In [0, 1] when set.
    #[serde(default)]
    pub match_confidence: Option<f64>,
    #[serde(default)]
    pub match_explanation: Option<String>,
    #[serde(default)]
    pub extracted_infrastructure_facts: Vec<ExtractedInfrastructureFact>,
    #[serde(default)]
    pub source_evidence_ids: Vec<Uuid>,
}

impl Validate for PermitRecord {
    fn check(&self, c: &mut Checker) {
        c.ge_opt("match_confidence", self.match_confidence, 0.0);
        c.le_opt("match_confidence", self.match_confidence, 1.0);
        c.each(
            "extracted_infrastructure_facts",
            &self.extracted_infrastructure_facts,
        );
    }
}

impl Contract for PermitRecord {
    const NAME: &'static str = "PermitRecord";
}
