//! Port of `apps/api/app/contracts/evidence.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ss_core::{
    new_id, utcnow, Checker, Contract, DataOrigin, EvidenceClassification, UtcDateTime, Validate,
    VerificationStatus,
};
use uuid::Uuid;

pub const HISTORICAL_NEEDS_LIMITATIONS: &str = "DOCUMENTED_HISTORICAL evidence must state its limitations (historical records do not establish current conditions).";

fn unverified() -> VerificationStatus {
    VerificationStatus::Unverified
}

/// One sourced observation. Conflicting evidence coexists; nothing is overwritten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEvidence {
    #[serde(default = "new_id")]
    pub evidence_id: Uuid,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub property_id: Option<Uuid>,
    /// e.g. 'la_open_data', 'costar', 'matterport', 'owner', 'ifc_bench'.
    pub provider: String,
    /// e.g. 'ysqd-apz7'.
    #[serde(default)]
    pub dataset: Option<String>,
    /// Provider-native ID, e.g. permit number.
    #[serde(default)]
    pub source_record_id: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub source_asset_reference: Option<String>,
    pub data_origin: DataOrigin,
    #[serde(default = "utcnow")]
    pub retrieved_at: UtcDateTime,
    /// When the fact was true at the source (e.g. permit final date).
    #[serde(default)]
    pub source_effective_at: Option<UtcDateTime>,
    /// e.g. 'electrical_service', 'water_service', 'sink_present'.
    pub observation_type: String,
    /// Python `Any`.
    #[serde(default)]
    pub extracted_value: Value,
    #[serde(default)]
    pub unit: Option<String>,
    pub evidence_classification: EvidenceClassification,
    /// In [0, 1] when set.
    #[serde(default)]
    pub match_confidence: Option<f64>,
    #[serde(default = "unverified")]
    pub verification_status: VerificationStatus,
    #[serde(default)]
    pub limitations: Vec<String>,
}

impl Validate for SourceEvidence {
    fn check(&self, c: &mut Checker) {
        let mark = c.error_count();
        c.ge_opt("match_confidence", self.match_confidence, 0.0);
        c.le_opt("match_confidence", self.match_confidence, 1.0);
        c.model_rules(mark, |c| {
            if self.evidence_classification == EvidenceClassification::DocumentedHistorical
                && self.limitations.is_empty()
            {
                c.value_error(HISTORICAL_NEEDS_LIMITATIONS);
            }
        });
    }
}

impl Contract for SourceEvidence {
    const NAME: &'static str = "SourceEvidence";
}
