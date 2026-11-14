//! The database half of `apps/api/app/domains/permits/service.py`: `PermitIntelligenceService
//! .ingest` (fetch, match, persist; idempotent per property and permit number). The matching
//! itself is `ss-domain`'s `fetch_and_match`.
//!
//! Persisted per new permit: the normalized `PermitRecord`, its `SourceEvidence` (record-level,
//! historical), a `property_sources` row with the raw reference (permit number and source URL;
//! raw payloads never go into consumer-facing tables) and a `property_identity_matches` row.
//!
//! Difference from Python: a permit number that occurs twice in one fetch is stored once (Python
//! computes the stored set once, so the second insert violates the unique constraint and the
//! whole ingest fails).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::json;
use ss_domain::contracts::{PropertyIdentity, SourceEvidence};
use ss_domain::integrations::la_permits::{LaPermitProvider, PROVIDER};
use ss_domain::permits::service::{PermitIngestResult, PermitIntelligenceService};
use uuid::Uuid;

use crate::error::AppResult;
use crate::integrations::blocking;
use crate::store::repo::sources::{add_match, add_source, IdentityMatch, PropertySource};
use crate::store::repo::{EVIDENCE, PERMITS};
use crate::store::{schema, Query, Tx};

/// Permit numbers already stored for the property.
async fn stored_permit_numbers(tx: &mut dyn Tx, property_id: Uuid) -> AppResult<HashSet<String>> {
    let rows = tx
        .select(
            &schema::PERMIT_RECORDS,
            &Query::new().eq("property_id", property_id),
        )
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| r.text("permit_number").ok())
        .collect())
}

/// `PermitIntelligenceService(provider).ingest(db, identity, project_id)`: the kept (new)
/// permits and evidence, every rejection, and the datasets queried.
pub async fn ingest(
    tx: &mut dyn Tx,
    provider: Arc<dyn LaPermitProvider>,
    identity: &PropertyIdentity,
    project_id: Uuid,
) -> AppResult<PermitIngestResult> {
    let svc = PermitIntelligenceService::new(provider);
    let ident = identity.clone();
    let res = blocking(move || svc.fetch_and_match(&ident, Some(project_id))).await??;
    let mut existing = stored_permit_numbers(tx, identity.property_id).await?;
    let ev_by_id: HashMap<Uuid, &SourceEvidence> =
        res.evidence.iter().map(|e| (e.evidence_id, e)).collect();
    let mut kept_permits = Vec::new();
    let mut kept_evidence = Vec::new();
    for rec in &res.permits {
        if !existing.insert(rec.permit_number.clone()) {
            continue;
        }
        PERMITS.add(tx, rec).await?;
        for eid in &rec.source_evidence_ids {
            if let Some(ev) = ev_by_id.get(eid) {
                EVIDENCE.add(tx, ev).await?;
                kept_evidence.push((*ev).clone());
            }
        }
        add_source(
            tx,
            &PropertySource {
                id: Uuid::new_v4(),
                property_id: identity.property_id,
                provider: PROVIDER.to_owned(),
                dataset: Some(rec.source_dataset.clone()),
                source_record_id: rec.permit_number.clone(),
                data_origin: rec.data_origin.as_str().to_owned(),
                raw_payload: json!({"permit_nbr": rec.permit_number, "source_url": rec.source_url}),
            },
        )
        .await?;
        add_match(
            tx,
            &IdentityMatch {
                id: Uuid::new_v4(),
                property_id: identity.property_id,
                provider: PROVIDER.to_owned(),
                provider_record_id: rec.permit_number.clone(),
                match_status: rec.match_status.as_str().to_owned(),
                match_confidence: rec.match_confidence,
                explanation: rec.match_explanation.clone().unwrap_or_default(),
            },
        )
        .await?;
        kept_permits.push(rec.clone());
    }
    Ok(PermitIngestResult {
        permits: kept_permits,
        evidence: kept_evidence,
        rejected: res.rejected,
        datasets: res.datasets,
    })
}
