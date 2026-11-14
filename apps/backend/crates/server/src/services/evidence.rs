//! Port of `apps/api/app/services/evidence.py`: query sourced evidence. Conflicting values
//! coexist and are surfaced, never merged.

use indexmap::IndexMap;
use ss_core::Contract;
use ss_domain::contracts::SourceEvidence;
use ss_domain::py;
use uuid::Uuid;

use crate::error::AppResult;
use crate::store::repo::EVIDENCE;
use crate::store::{Query, Tx};

#[derive(Debug, Clone, Copy, Default)]
pub struct EvidenceService;

impl EvidenceService {
    /// `for_property`: every evidence row of the property, in `created_at` order (ties in
    /// insertion order). The order matters to feasibility (first maximum, last broker answer).
    pub async fn for_property(
        &self,
        tx: &mut dyn Tx,
        property_id: Uuid,
    ) -> AppResult<Vec<SourceEvidence>> {
        EVIDENCE
            .list(tx, Query::new().eq("property_id", property_id))
            .await
    }

    /// `add`: validate and store one evidence record.
    pub async fn add(&self, tx: &mut dyn Tx, evidence: &SourceEvidence) -> AppResult<()> {
        evidence.validate()?;
        EVIDENCE.add(tx, evidence).await
    }

    /// `conflicts`: observation type -> evidence, for the types whose sources disagree on the
    /// value (compared as Python's `str(extracted_value)`), in first-seen order.
    pub fn conflicts(evidence: &[SourceEvidence]) -> IndexMap<String, Vec<SourceEvidence>> {
        let mut groups: IndexMap<String, Vec<SourceEvidence>> = IndexMap::new();
        for e in evidence {
            groups
                .entry(e.observation_type.clone())
                .or_default()
                .push(e.clone());
        }
        groups.retain(|_, v| {
            let mut values: Vec<String> = v.iter().map(|e| py::value_str(&e.extracted_value)).collect();
            values.sort();
            values.dedup();
            values.len() > 1
        });
        groups
    }
}
