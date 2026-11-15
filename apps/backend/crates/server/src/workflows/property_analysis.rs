//! Port of `apps/api/app/workflows/property_analysis.py`: the property-analysis vertical slice and
//! its event-driven recomputation.
//!
//! search -> identity -> permits -> digital profile -> feasibility -> cost scenario -> ranking.
//! Subscribers:
//! - `infrastructure.evidence_updated` -> re-evaluate that property -> `financial.scenario.updated`
//! - `financial.scenario.updated` -> re-rank the project -> `property.ranking.updated`
//!
//! Event idempotency keys and payloads are Python's. Earlier assessments, work items and cost
//! scenarios are superseded, never deleted.
//!
//! Differences from Python:
//! - the evidence subscriber loads the property only from the event's own project (Python loads
//!   any property by id);
//! - a permit provider failure (unavailable, timeout, rate limited, bad answer, ...) does not
//!   fail the search or the analysis: the property continues without new permit records and
//!   says so (see `ingest_permits_or_skip`); after a timeout the rest of the search skips the
//!   provider. Python lets the `ProviderError` fail the whole task.

use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_core::{utcnow, BusinessExpansionProfile, ProjectStatus};
use ss_domain::contracts::{
    AssessmentStatus, CostScenario, InfrastructureAssessment, PropertyDigitalProfile,
    PropertyRankingResult, PropertySearchRequest, SourceManifestEntry,
};
use ss_domain::feasibility::FeasibilityEngine;
use ss_domain::finance::FinancialCalculationService;
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};
use ss_domain::property::service::{attach_permits, PropertyScoutService};
use ss_domain::ranking::RankingEngine;
use ss_domain::requirements::templates::requirements_for;
use uuid::Uuid;

use super::enrichment::{enrich_exterior, enrich_interior};
use crate::app::{App, EventHandler};
use crate::contracts::{ActivityEvent, ActorType, EventType, JsonMap};
use crate::error::{AppError, AppResult};
use crate::integrations::{blocking, Integrations};
use crate::services::events::Emit;
use crate::services::evidence::EvidenceService;
use crate::services::permits;
use crate::store::repo::superseded::{live_for_property, supersede_property_outputs};
use crate::store::repo::{
    ASSESSMENTS, COST_SCENARIOS, PERMITS, PROFILES, PROJECTS, PROPERTIES, RANKINGS, WORK_ITEMS,
};
use crate::store::{schema, Query, Tx};

pub const AGENT: &str = "expansion-supervisor";

/// `current_profile`: the project's latest business profile, or 404 `PROFILE_REQUIRED`.
pub async fn current_profile(
    tx: &mut dyn Tx,
    project_id: Uuid,
) -> AppResult<BusinessExpansionProfile> {
    let mut profiles = PROFILES
        .list(tx, Query::new().eq("project_id", project_id))
        .await?;
    profiles.pop().ok_or_else(|| {
        AppError::not_found("business profile not found; create one first").code("PROFILE_REQUIRED")
    })
}

fn is_profile_required(e: &AppError) -> bool {
    matches!(e, AppError::NotFound(_)) && e.error_code() == "PROFILE_REQUIRED"
}

/// `PropertyAnalysisWorkflow`.
pub struct PropertyAnalysisWorkflow<'a> {
    app: &'a App,
    providers: Arc<Integrations>,
}

impl<'a> PropertyAnalysisWorkflow<'a> {
    pub fn new(app: &'a App) -> Self {
        Self {
            app,
            providers: app.integrations(),
        }
    }

    /// `with_profile_limits`: fill unset search limits from the owner's business profile (area
    /// need, rent ceiling). Without a profile the request is used as given.
    pub async fn with_profile_limits(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        req: PropertySearchRequest,
    ) -> AppResult<PropertySearchRequest> {
        let profile = match current_profile(tx, project_id).await {
            Ok(p) => p,
            Err(e) if is_profile_required(&e) => return Ok(req),
            Err(e) => return Err(e),
        };
        let mut req = req;
        if req.min_area_sqm.is_none() {
            if let Some(a) = profile
                .space_requirements
                .min_area_sqm
                .filter(|a| *a != 0.0)
            {
                req.min_area_sqm = Some(a);
            }
        }
        if req.max_monthly_rent.is_none() {
            if let Some(r) = profile.monthly_rent_limit.filter(|r| *r != 0) {
                req.max_monthly_rent = Some(r);
            }
        }
        Ok(req)
    }

    /// `search`: a wider pool from the listing provider, interior enrichment, suite-matched scans
    /// first, exterior enrichment of the `limit` best, then store each new address once per
    /// project (an address already stored is returned as stored).
    pub async fn search(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        req: PropertySearchRequest,
        correlation_id: Uuid,
    ) -> AppResult<Vec<PropertyDigitalProfile>> {
        let req = self.with_profile_limits(tx, project_id, req).await?;
        let pool_req = PropertySearchRequest {
            limit: (req.limit.saturating_mul(3)).min(50),
            ..req.clone()
        };
        let costar = self.providers.costar.clone();
        let matterport = self.providers.matterport.clone();
        let vexcel = self.providers.vexcel.clone();
        let media_base = self.app.settings.public_base_url.clone();
        let limit = usize::try_from(req.limit).unwrap_or(0);
        let found = blocking(move || -> AppResult<Vec<PropertyDigitalProfile>> {
            let pool = PropertyScoutService::new(costar).search(project_id, &pool_req)?;
            let mut pool: Vec<PropertyDigitalProfile> = pool
                .into_iter()
                .map(|p| enrich_interior(p, &*matterport))
                .collect();
            // Prefer candidates whose listed suite has a measured interior scan (stable).
            pool.sort_by_key(|p| {
                !p.interior_assets
                    .iter()
                    .any(|a| a.matches_listing_unit == Some(true))
            });
            Ok(pool
                .into_iter()
                .take(limit)
                .map(|p| enrich_exterior(p, &*vexcel, &media_base))
                .collect())
        })
        .await??;
        let mut saved: Vec<PropertyDigitalProfile> = Vec::new();
        for p in found {
            let existing = PROPERTIES
                .list(
                    tx,
                    Query::new()
                        .eq("project_id", project_id)
                        .eq(
                            "normalized_address",
                            p.property_identity.normalized_address.as_str(),
                        )
                        .limit(1),
                )
                .await?;
            if let Some(stored) = existing.into_iter().next() {
                saved.push(stored);
                continue;
            }
            PROPERTIES.add(tx, &p).await?;
            self.app
                .events()
                .emit(
                    tx,
                    Emit::new(
                        EventType::PropertyIdentityResolved,
                        project_id,
                        ActorType::Agent,
                        "property-scout",
                        format!("identity:{}", p.property_id),
                        json!({
                            "property_id": p.property_id.to_string(),
                            "summary": format!(
                                "{} ({})",
                                p.property_identity.normalized_address,
                                p.property_identity.identity_match_status.as_str()
                            ),
                        }),
                    )
                    .correlation(correlation_id),
                )
                .await?;
            saved.push(p);
        }
        let origin = saved
            .first()
            .and_then(|p| p.commercial_listing.as_ref())
            .map_or("NONE", |l| l.data_origin.as_str());
        self.app
            .events()
            .emit(
                tx,
                Emit::new(
                    EventType::PropertySearchCompleted,
                    project_id,
                    ActorType::Agent,
                    "property-scout",
                    format!("search:{project_id}:{correlation_id}"),
                    json!({
                        "property_ids": saved.iter().map(|p| p.property_id.to_string()).collect::<Vec<_>>(),
                        "provider": self.providers.costar.mode(),
                        "data_origin": origin,
                    }),
                )
                .correlation(correlation_id),
            )
            .await?;
        Ok(saved)
    }

    /// `ingest_permits`: fetch and store new matched permits, attach every stored permit of the
    /// property to its profile, `property.permits.ingested`. A provider failure does not fail
    /// the caller: see [`Self::ingest_permits_or_skip`].
    pub async fn ingest_permits(
        &self,
        tx: &mut dyn Tx,
        prop: PropertyDigitalProfile,
        correlation_id: Uuid,
    ) -> AppResult<PropertyDigitalProfile> {
        Ok(self
            .ingest_permits_or_skip(tx, prop, correlation_id, None)
            .await?
            .0)
    }

    /// The permit step, tolerant of the permit provider being down.
    ///
    /// Deliberate difference from Python (`property_analysis.py` lets the `ProviderError` fail
    /// the whole task): when the provider fails for this property with a provider error, or
    /// when `skip` carries an earlier timeout of the same search, the property continues
    /// without new permit records. Its `la_open_data` manifest entry becomes `BLOCKED` with the
    /// error kind in the note (no `retrieved_at`: nothing was retrieved, and `record_count` is
    /// not a "zero permits found"), `missing_information` says the records were unavailable,
    /// no `property.permits.ingested` event is emitted, and the returned [`PermitSkip`] is put
    /// in the task result by [`Self::run`]. Nothing is stored for the property by the failed
    /// fetch. Permits stored by an earlier successful retrieval of this property stay attached
    /// (the feasibility evidence cites them) and the note says so. Any other failure (a
    /// malformed record, a database error) still fails the caller.
    pub async fn ingest_permits_or_skip(
        &self,
        tx: &mut dyn Tx,
        prop: PropertyDigitalProfile,
        correlation_id: Uuid,
        skip: Option<&ProviderError>,
    ) -> AppResult<(PropertyDigitalProfile, Option<PermitSkip>)> {
        let provider = self.providers.permits.clone();
        let live = provider.mode() == "live";
        let fetched = match skip {
            Some(earlier) => Err((earlier.clone(), false)),
            None => permits::fetch(provider, &prop.property_identity, prop.project_id)
                .await?
                .map_err(|e| (e, true)),
        };
        let fetched = match fetched {
            Ok(fetched) => fetched,
            Err((error, provider_called)) => {
                let stored = PERMITS
                    .list(tx, Query::new().eq("property_id", prop.property_id))
                    .await?;
                let prop = permits_unavailable(&prop, stored, &error, provider_called);
                PROPERTIES.upsert(tx, &prop).await?;
                let skipped = PermitSkip {
                    property_id: prop.property_id,
                    error,
                    provider_called,
                };
                return Ok((prop, Some(skipped)));
            }
        };
        let res = permits::store(tx, &prop.property_identity, fetched).await?;
        let all_permits = PERMITS
            .list(tx, Query::new().eq("property_id", prop.property_id))
            .await?;
        let count = all_permits.len();
        let origin = if live { "LIVE" } else { "FIXTURE" };
        let prop = attach_permits(&prop, all_permits, &res.datasets, origin);
        PROPERTIES.upsert(tx, &prop).await?;
        self.app
            .events()
            .emit(
                tx,
                Emit::new(
                    EventType::PropertyPermitsIngested,
                    prop.project_id,
                    ActorType::Agent,
                    "permit-intelligence",
                    format!("permits:{}:{count}", prop.property_id),
                    json!({
                        "property_id": prop.property_id.to_string(),
                        "permit_record_ids": res.permits.iter().map(|p| p.permit_record_id.to_string()).collect::<Vec<_>>(),
                        "evidence_ids": res.evidence.iter().map(|e| e.evidence_id.to_string()).collect::<Vec<_>>(),
                        "source_dataset_ids": res.datasets,
                    }),
                )
                .correlation(correlation_id),
            )
            .await?;
        Ok((prop, None))
    }

    /// `evaluate`: feasibility and the cost scenario for one property. Earlier outputs of the
    /// property are superseded first. The scenario event is dispatched only when the evaluation
    /// itself was caused by an event (the full slice ranks once at the end instead).
    pub async fn evaluate(
        &self,
        tx: &mut dyn Tx,
        prop: PropertyDigitalProfile,
        correlation_id: Uuid,
        cause: Option<&ActivityEvent>,
    ) -> AppResult<(Vec<InfrastructureAssessment>, CostScenario)> {
        let profile = current_profile(tx, prop.project_id).await?;
        let evidence = EvidenceService.for_property(tx, prop.property_id).await?;
        let requirements = requirements_for(&profile);
        let (assessments, work) =
            FeasibilityEngine.evaluate(&profile, &requirements, &prop, &evidence)?;
        // Supersede previous outputs instead of deleting them (history preserved).
        supersede_property_outputs(tx, prop.property_id).await?;
        ASSESSMENTS.add_all(tx, &assessments).await?;
        WORK_ITEMS.add_all(tx, &work).await?;
        let scenario = FinancialCalculationService.scenario(&profile, &prop, &work, None, None)?;
        COST_SCENARIOS.add(tx, &scenario).await?;
        let prop = PropertyDigitalProfile {
            infrastructure_assessments: assessments.clone(),
            last_analyzed_at: Some(utcnow()),
            ..prop
        };
        PROPERTIES.upsert(tx, &prop).await?;
        let tag = cause.map_or(correlation_id, |c| c.event_id);
        let unknown: Vec<&InfrastructureAssessment> = assessments
            .iter()
            .filter(|a| a.status == AssessmentStatus::Unknown)
            .collect();
        let mut analysis = Emit::new(
            EventType::PropertyAnalysisCompleted,
            prop.project_id,
            ActorType::Agent,
            "feasibility",
            format!("analysis:{}:{tag}", prop.property_id),
            json!({
                "property_id": prop.property_id.to_string(),
                "assessment_ids": assessments.iter().map(|a| a.assessment_id.to_string()).collect::<Vec<_>>(),
                "unknown_requirement_ids": unknown.iter().map(|a| a.requirement_id.clone()).collect::<Vec<_>>(),
            }),
        )
        .correlation(correlation_id);
        if let Some(c) = cause {
            analysis = analysis.caused_by(c);
        }
        self.app.events().emit(tx, analysis).await?;
        if !unknown.is_empty() {
            let mut e = Emit::new(
                EventType::InfrastructureUnknownDetected,
                prop.project_id,
                ActorType::Agent,
                "feasibility",
                format!("unknowns:{}:{tag}", prop.property_id),
                json!({
                    "property_id": prop.property_id.to_string(),
                    "requirement_ids": unknown.iter().map(|a| a.requirement_id.clone()).collect::<Vec<_>>(),
                    "verification_questions": unknown.iter().flat_map(|a| a.verification_questions.iter().cloned()).collect::<Vec<_>>(),
                }),
            )
            .correlation(correlation_id);
            if let Some(c) = cause {
                e = e.caused_by(c);
            }
            self.app.events().emit(tx, e).await?;
        }
        let mut e = Emit::new(
            EventType::FinancialScenarioUpdated,
            prop.project_id,
            ActorType::Agent,
            "financial-modeling",
            format!("scenario:{}", scenario.scenario_id),
            json!({
                "property_id": prop.property_id.to_string(),
                "scenario_id": scenario.scenario_id.to_string(),
                "long_term_cost": scenario.long_term_cost,
                "comparison_horizon_months": scenario.comparison_horizon_months,
            }),
        )
        .correlation(correlation_id);
        match cause {
            Some(c) => e = e.caused_by(c),
            None => e = e.without_dispatch(),
        }
        self.app.events().emit(tx, e).await?;
        Ok((assessments, scenario))
    }

    /// `rank`: rank every stored property of the project with its live scenario and assessments;
    /// a new ranking version each time.
    pub async fn rank(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        correlation_id: Uuid,
        cause: Option<&ActivityEvent>,
    ) -> AppResult<PropertyRankingResult> {
        let profile = current_profile(tx, project_id).await?;
        let props = PROPERTIES
            .list(tx, Query::new().eq("project_id", project_id))
            .await?;
        let mut scenarios: IndexMap<Uuid, CostScenario> = IndexMap::new();
        let mut assessments: IndexMap<Uuid, Vec<InfrastructureAssessment>> = IndexMap::new();
        for p in &props {
            let rows: Vec<CostScenario> = live_for_property(tx, p.property_id).await?;
            if let Some(last) = rows.into_iter().last() {
                scenarios.insert(p.property_id, last);
            }
            let arows: Vec<InfrastructureAssessment> = live_for_property(tx, p.property_id).await?;
            assessments.insert(p.property_id, arows);
        }
        let latest = tx
            .select(
                &schema::PROPERTY_RANKINGS,
                &Query::new()
                    .eq("project_id", project_id)
                    .order_desc("ranking_version")
                    .limit(1),
            )
            .await?;
        let version = match latest.first() {
            Some(r) => r.int("ranking_version")? + 1,
            None => 1,
        };
        let result = RankingEngine.rank(&profile, &props, &scenarios, &assessments, version)?;
        RANKINGS.add(tx, &result).await?;
        let top = result.ranked_properties.first().map(|r| r.property_id);
        let mut e = Emit::new(
            EventType::PropertyRankingUpdated,
            project_id,
            ActorType::Agent,
            "property-ranking",
            format!("ranking:{project_id}:{version}"),
            json!({
                "ranking_id": result.ranking_id.to_string(),
                "ranking_version": version,
                "top_property_id": top.map(|t| t.to_string()),
            }),
        )
        .correlation(correlation_id);
        if let Some(c) = cause {
            e = e.caused_by(c);
        }
        self.app.events().emit(tx, e).await?;
        Ok(result)
    }

    /// `run`: the full slice for a project (the search route's task).
    pub async fn run(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        req: PropertySearchRequest,
        correlation_id: Uuid,
    ) -> AppResult<JsonMap> {
        let project = PROJECTS.require(tx, project_id).await?;
        let props = self.search(tx, project_id, req, correlation_id).await?;
        if props.is_empty() {
            return Err(AppError::conflict("no candidate properties found").code("NO_CANDIDATES"));
        }
        // After a permit timeout, the remaining properties of this search skip the provider:
        // one slow provider costs one timeout per search, not one per property.
        let mut timed_out: Option<ProviderError> = None;
        let mut skipped: Vec<PermitSkip> = Vec::new();
        for p in &props {
            let (p, skip) = self
                .ingest_permits_or_skip(tx, p.clone(), correlation_id, timed_out.as_ref())
                .await?;
            if let Some(skip) = skip {
                if skip.error.kind == ProviderErrorKind::Timeout && timed_out.is_none() {
                    timed_out = Some(skip.error.clone());
                }
                skipped.push(skip);
            }
            self.evaluate(tx, p, correlation_id, None).await?;
        }
        let ranking = self.rank(tx, project_id, correlation_id, None).await?;
        let mut updated = project;
        updated.project_status = ProjectStatus::Comparing;
        updated.updated_at = utcnow();
        PROJECTS.upsert(tx, &updated).await?;
        let mut out = JsonMap::new();
        out.insert(
            "property_ids".into(),
            Value::from(
                props
                    .iter()
                    .map(|p| p.property_id.to_string())
                    .collect::<Vec<_>>(),
            ),
        );
        out.insert(
            "ranking_id".into(),
            Value::String(ranking.ranking_id.to_string()),
        );
        out.insert(
            "ranking_version".into(),
            Value::from(ranking.ranking_version),
        );
        // Only when a permit step was skipped (fixture-mode results are unchanged).
        if !skipped.is_empty() {
            out.insert(
                "permit_step_skipped".into(),
                Value::from(skipped.iter().map(PermitSkip::to_json).collect::<Vec<_>>()),
            );
        }
        Ok(out)
    }
}

/// A property whose permit step was skipped because the permit provider failed.
#[derive(Debug, Clone)]
pub struct PermitSkip {
    pub property_id: Uuid,
    pub error: ProviderError,
    /// `false` when the provider was not called because it had timed out earlier in the search.
    pub provider_called: bool,
}

impl PermitSkip {
    /// The task-result entry. Provider messages never carry credentials (`AppError::provider`).
    pub fn to_json(&self) -> Value {
        json!({
            "property_id": self.property_id.to_string(),
            "provider": self.error.provider,
            "error_kind": self.error.kind.as_str(),
            "error_message": self.error.to_string(),
            "provider_called": self.provider_called,
            "summary": if self.provider_called {
                "Permit step skipped: the permit provider could not be reached; permit records unknown"
            } else {
                "Permit step skipped: the permit provider timed out earlier in this search; permit records unknown"
            },
        })
    }
}

/// The profile after a failed permit fetch: the `la_open_data` manifest entry says the provider
/// could not be reached and why (status `BLOCKED`, the contract's closest value; no
/// `retrieved_at`), `missing_information` says the records are unavailable (and drops any
/// "No permit records matched", which would claim a successful empty answer). `stored` are the
/// permits an earlier successful retrieval already stored for this property (none for a new
/// property); they stay attached and the note says where they come from.
fn permits_unavailable(
    prop: &PropertyDigitalProfile,
    stored: Vec<ss_domain::contracts::PermitRecord>,
    error: &ProviderError,
    provider_called: bool,
) -> PropertyDigitalProfile {
    let kind = error.kind.as_str();
    let why = if provider_called {
        format!("Permit provider could not be reached ({kind})")
    } else {
        format!(
            "Permit provider could not be reached: it timed out earlier in this search and was \
             not called again ({kind})"
        )
    };
    let note = if stored.is_empty() {
        format!("{why}; permit records unknown, not zero")
    } else {
        format!(
            "{why}; the {} records shown are from an earlier retrieval; newer records unknown",
            stored.len()
        )
    };
    let mut manifest: Vec<SourceManifestEntry> = prop
        .source_manifest
        .iter()
        .filter(|m| m.provider != "la_open_data")
        .cloned()
        .collect();
    let datasets: Vec<String> = stored
        .iter()
        .map(|p| p.source_dataset.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let joined = datasets.join(",");
    manifest.push(SourceManifestEntry {
        provider: "la_open_data".into(),
        dataset: (!joined.is_empty()).then_some(joined),
        status: "BLOCKED".into(),
        record_count: stored.len() as i64,
        retrieved_at: None,
        note: Some(note),
    });
    let mut missing: Vec<String> = prop
        .missing_information
        .iter()
        .filter(|m| !m.starts_with("No permit") && !m.starts_with("Permit records unavailable"))
        .cloned()
        .collect();
    missing.push(format!(
        "Permit records unavailable: the public permit API could not be reached ({kind})"
    ));
    PropertyDigitalProfile {
        permit_records: stored,
        source_manifest: manifest,
        missing_information: missing,
        ..prop.clone()
    }
}

// ---- event subscribers (cross-domain consumers) -------------------------------------------------

/// `infrastructure.evidence_updated` -> re-evaluate that property (`feasibility.reevaluate`).
pub struct OnEvidenceUpdated;

#[async_trait]
impl EventHandler for OnEvidenceUpdated {
    async fn handle(&self, app: &App, tx: &mut dyn Tx, event: &ActivityEvent) -> AppResult<()> {
        if app
            .events()
            .already_handled(tx, event, EventType::PropertyAnalysisCompleted)
            .await?
        {
            return Ok(());
        }
        let pid = event
            .payload
            .get("property_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| {
                AppError::internal_kind("ValueError", "evidence event without a property id")
            })?;
        let prop = PROPERTIES
            .require_in_project(
                tx,
                pid,
                event.project_id,
                &format!("PropertyDigitalProfile {pid} not found"),
            )
            .await?;
        PropertyAnalysisWorkflow::new(app)
            .evaluate(tx, prop, event.correlation_id, Some(event))
            .await?;
        Ok(())
    }
}

/// `financial.scenario.updated` -> re-rank the project (`ranking.rerank`).
pub struct OnScenarioUpdated;

#[async_trait]
impl EventHandler for OnScenarioUpdated {
    async fn handle(&self, app: &App, tx: &mut dyn Tx, event: &ActivityEvent) -> AppResult<()> {
        if app
            .events()
            .already_handled(tx, event, EventType::PropertyRankingUpdated)
            .await?
        {
            return Ok(());
        }
        PropertyAnalysisWorkflow::new(app)
            .rank(tx, event.project_id, event.correlation_id, Some(event))
            .await?;
        Ok(())
    }
}

/// Register the subscribers (Python registers them when the module is imported).
pub fn register(app: &App) {
    app.subscribe(
        EventType::InfrastructureEvidenceUpdated,
        "feasibility.reevaluate",
        Arc::new(OnEvidenceUpdated),
    );
    app.subscribe(
        EventType::FinancialScenarioUpdated,
        "ranking.rerank",
        Arc::new(OnScenarioUpdated),
    );
}
