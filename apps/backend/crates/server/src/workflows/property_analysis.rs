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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use ss_core::{utcnow, BusinessExpansionProfile, ProjectStatus};
use ss_domain::contracts::{
    AssessmentStatus, CostScenario, InfrastructureAssessment, PropertyDigitalProfile,
    PropertyIdentity, PropertyRankingResult, PropertySearchRequest, ProviderStatusEntry,
    SourceManifestEntry,
};
use ss_domain::feasibility::FeasibilityEngine;
use ss_domain::finance::FinancialCalculationService;
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};
use ss_domain::integrations::la_permits::{LaPermitProvider, RawPermit};
use ss_domain::property::service::{attach_permits, PropertyScoutService};
use ss_domain::ranking::RankingEngine;
use ss_domain::requirements::templates::requirements_for;
use uuid::Uuid;

use super::enrichment::{enrich_exterior, enrich_interior_pool};
use crate::app::{App, EventHandler};
use crate::contracts::{ActivityEvent, ActorType, EventType, JsonMap};
use crate::error::{AppError, AppResult};
use crate::integrations::matterport::{MatterportProvider, ModelMedia, ModelSummary};
use crate::integrations::vexcel::VexcelProvider;
use crate::integrations::{blocking, Integrations};
use crate::logging;
use crate::services::events::Emit;
use crate::services::evidence::EvidenceService;
use crate::services::permits;
use crate::store::repo::superseded::{live_for_property, supersede_property_outputs};
use crate::store::repo::{
    ASSESSMENTS, COST_SCENARIOS, PERMITS, PROFILES, PROJECTS, PROPERTIES, RANKINGS, WORK_ITEMS,
};
use crate::store::{schema, Query, Tx};

pub const AGENT: &str = "expansion-supervisor";

/// Provider requests of one search that may be in flight at once (exterior enrichment, permit
/// fetches). Tests set another bound with `app.extensions.insert(Arc::new(SearchConcurrency(n)))`.
pub const PROVIDER_CONCURRENCY: usize = 8;

/// An override of [`PROVIDER_CONCURRENCY`] (values below 1 count as 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchConcurrency(pub usize);

/// `items.map(claim).map(work)`, with at most `bound` `work` calls running at once on scoped
/// threads, results in the order of `items`. `claim` runs for one item at a time, in the order of
/// `items` (under the queue lock), so a decision it takes from shared state (for example "the
/// provider has timed out: do not call it") applies to a suffix of `items`. With a bound of 1
/// (or one item) everything runs on the calling thread, one item after the other, as before.
pub fn bounded_map<T, S, R>(
    items: Vec<T>,
    bound: usize,
    claim: impl Fn(T) -> S + Sync,
    work: impl Fn(S) -> R + Sync,
) -> Vec<R>
where
    T: Send,
    R: Send,
{
    let n = items.len();
    let workers = bound.max(1).min(n);
    if workers <= 1 {
        return items.into_iter().map(|t| work(claim(t))).collect();
    }
    let queue = Mutex::new(items.into_iter().enumerate());
    let slots: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let next = {
                    let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                    q.next().map(|(i, t)| (i, claim(t)))
                };
                let Some((i, claimed)) = next else { break };
                let r = work(claimed);
                *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            });
        }
    });
    // A worker that panicked has already made `scope` panic; every slot is filled here.
    slots
        .into_iter()
        .map(|m| {
            m.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every item was processed")
        })
        .collect()
}

/// A permit fetch done ahead of the per-property loop: the matched answer, or the provider error
/// and whether the provider was called (`false`: skipped after an earlier timeout). The outer
/// `Err` is a failure that is not a provider error (it still fails the task).
type Prefetched = AppResult<Result<permits::Fetched, (ProviderError, bool)>>;

// ---- the search's timing line -------------------------------------------------------------------

/// The calls one search made to one provider: how many, the slowest one and their summed wall
/// time. A call is one adapter method call: one Matterport `model_summaries` call is one request
/// of up to 20 ids; one Vexcel call is one request, or none when the adapter's cache answered; one
/// permit call is at least two requests made one after the other inside the adapter (the
/// electrical, then the mechanical dataset, one more per page of 1000 rows).
#[derive(Debug, Default)]
pub struct Meter {
    calls: AtomicU64,
    slowest_us: AtomicU64,
    sum_us: AtomicU64,
}

impl Meter {
    fn time<R>(&self, f: impl FnOnce() -> R) -> R {
        let started = Instant::now();
        let r = f();
        let us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.slowest_us.fetch_max(us, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        r
    }

    fn to_json(&self) -> Value {
        let ms = |a: &AtomicU64| (a.load(Ordering::Relaxed) as f64 / 100.0).round() / 10.0;
        json!({
            "calls": self.calls.load(Ordering::Relaxed),
            "slowest_ms": ms(&self.slowest_us),
            "sum_ms": ms(&self.sum_us),
        })
    }
}

/// Milliseconds, one decimal.
fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

/// A Vexcel provider that counts and times the search's calls (no request is changed).
struct MeteredVexcel(Arc<dyn VexcelProvider>, Arc<Meter>);

impl VexcelProvider for MeteredVexcel {
    fn mode(&self) -> &str {
        self.0.mode()
    }
    fn ortho_image(
        &self,
        lat: f64,
        lon: f64,
        size_m: i64,
        pixels: i64,
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        self.0.ortho_image(lat, lon, size_m, pixels)
    }
    fn capture_dates(&self, lat: f64, lon: f64) -> Result<Vec<Map<String, Value>>, ProviderError> {
        self.1.time(|| self.0.capture_dates(lat, lon))
    }
    fn property_elements(
        &self,
        lat: f64,
        lon: f64,
    ) -> Result<Option<Map<String, Value>>, ProviderError> {
        self.1.time(|| self.0.property_elements(lat, lon))
    }
    fn status(&self) -> ProviderStatusEntry {
        self.0.status()
    }
}

/// A Matterport provider that counts and times `model_summaries` (no request is changed).
struct MeteredMatterport(Arc<dyn MatterportProvider>, Arc<Meter>);

impl MatterportProvider for MeteredMatterport {
    fn mode(&self) -> &str {
        self.0.mode()
    }
    fn get_floor_geometry(&self, asset_id: &str) -> Result<Option<Value>, ProviderError> {
        self.0.get_floor_geometry(asset_id)
    }
    fn model_not_found(&self, asset_id: &str) -> bool {
        self.0.model_not_found(asset_id)
    }
    fn model_summaries(
        &self,
        model_ids: &[String],
    ) -> Result<IndexMap<String, ModelSummary>, ProviderError> {
        self.1.time(|| self.0.model_summaries(model_ids))
    }
    fn status(&self) -> ProviderStatusEntry {
        self.0.status()
    }
    fn media_enabled(&self) -> bool {
        self.0.media_enabled()
    }
    fn model_media(&self, asset_id: &str, fresh: bool) -> Result<Option<ModelMedia>, ProviderError> {
        self.0.model_media(asset_id, fresh)
    }
    fn download_media(&self, url: &str, limit: usize) -> Result<Vec<u8>, ProviderError> {
        self.0.download_media(url, limit)
    }
}

/// A permit provider that counts and times the search's calls (no request is changed).
struct MeteredPermits(Arc<dyn LaPermitProvider>, Arc<Meter>);

impl LaPermitProvider for MeteredPermits {
    fn mode(&self) -> &str {
        self.0.mode()
    }
    fn search_by_address_prefix(
        &self,
        house_number: &str,
        direction: Option<&str>,
        street_name: &str,
        zip_code: Option<&str>,
    ) -> Result<Vec<RawPermit>, ProviderError> {
        self.1.time(|| {
            self.0
                .search_by_address_prefix(house_number, direction, street_name, zip_code)
        })
    }
    fn get_permit(&self, permit_number: &str) -> Result<Vec<RawPermit>, ProviderError> {
        self.1.time(|| self.0.get_permit(permit_number))
    }
    fn status(&self) -> ProviderStatusEntry {
        self.0.status()
    }
}

/// The providers one search uses (metered in [`PropertyAnalysisWorkflow::run`]).
#[derive(Clone)]
struct SearchProviders {
    matterport: Arc<dyn MatterportProvider>,
    vexcel: Arc<dyn VexcelProvider>,
    permits: Arc<dyn LaPermitProvider>,
}

/// Where one search's time went: wall time per phase, counts. Numbers only: no address, id or URL.
#[derive(Debug, Default)]
struct SearchTiming {
    listings: Duration,
    interior: Duration,
    exterior: Duration,
    search_store: Duration,
    /// Wall time of the permit fetches themselves (they overlap the exterior phase).
    permits_fetch: Duration,
    /// How long the request waited for the permit fetches once the search was stored.
    permits_wait: Duration,
    /// Fetches redone in the loop because the stored identity differed from the predicted one.
    permits_refetch: Duration,
    permits_store: Duration,
    evaluate: Duration,
    evaluate_compute: Duration,
    rank: Duration,
    pool: usize,
    enriched: usize,
    stored_new: usize,
    permit_refetches: usize,
}

/// Tests read the timing lines here (`app.extensions.insert(Arc::new(SearchTimingSink::default()))`).
#[derive(Debug, Default)]
pub struct SearchTimingSink(pub Mutex<Vec<Value>>);

/// Fetch the permits of every result of a search before any is stored, at most `bound` at a
/// time, answers in the order of `work`. After a fetch has timed out, the fetches not yet
/// started are skipped (`provider_called: false`); those already in flight finish on their own.
/// Nothing touches the database.
fn prefetch_permits_blocking(
    provider: Arc<dyn LaPermitProvider>,
    work: Vec<(PropertyIdentity, Uuid)>,
    bound: usize,
) -> Vec<Prefetched> {
    let timed_out: Mutex<Option<ProviderError>> = Mutex::new(None);
    let earlier = || timed_out.lock().unwrap_or_else(|e| e.into_inner()).clone();
    bounded_map(
        work,
        bound,
        |item| (earlier(), item),
        |(skip, (identity, project_id))| -> Prefetched {
            if let Some(e) = skip {
                return Ok(Err((e, false)));
            }
            let answer = permits::fetch_blocking(provider.clone(), &identity, project_id)?;
            if let Err(e) = &answer {
                if e.kind == ProviderErrorKind::Timeout {
                    let mut t = timed_out.lock().unwrap_or_else(|e| e.into_inner());
                    if t.is_none() {
                        *t = Some(e.clone());
                    }
                }
            }
            Ok(answer.map_err(|e| (e, true)))
        },
    )
}

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
    /// Microseconds spent in the feasibility and cost computations of [`Self::evaluate`] (the
    /// rest of it is database work), for the search's timing line.
    compute_us: AtomicU64,
}

/// The permit fetches of a search, started before the exterior enrichment: the identity each
/// fetch used (in the order of the search's results) and the running job.
type PermitJob = (
    Vec<(PropertyIdentity, Uuid)>,
    tokio::task::JoinHandle<(Vec<Prefetched>, Duration)>,
);

impl<'a> PropertyAnalysisWorkflow<'a> {
    pub fn new(app: &'a App) -> Self {
        Self {
            app,
            providers: app.integrations(),
            compute_us: AtomicU64::new(0),
        }
    }

    fn search_providers(&self) -> SearchProviders {
        SearchProviders {
            matterport: self.providers.matterport.clone(),
            vexcel: self.providers.vexcel.clone(),
            permits: self.providers.permits.clone(),
        }
    }

    /// The bound on concurrent provider requests of one search.
    fn concurrency(&self) -> usize {
        self.app
            .extensions
            .get::<SearchConcurrency>()
            .map_or(PROVIDER_CONCURRENCY, |c| c.0.max(1))
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
        let mut timing = SearchTiming::default();
        let providers = self.search_providers();
        let (saved, _) = self
            .search_inner(tx, project_id, req, correlation_id, &providers, &mut timing, false)
            .await?;
        Ok(saved)
    }

    /// [`Self::search`], timed into `timing`. With `prefetch`, the permit fetches of the results
    /// start as soon as the results are known (after the interior enrichment), concurrently
    /// with the exterior enrichment and the storing, and the running job is returned.
    ///
    /// The fetches need each result's final identity before it is stored: a result whose address
    /// is already stored in the project is returned as stored, with the stored identity. So the
    /// existing-address lookups are made before the exterior enrichment (reads only; nothing is
    /// written to the project's properties in between but this search's own new addresses). A
    /// result whose address an earlier result of the same search adds is looked up again in the
    /// loop, as before, and its fetch uses that earlier result's identity; `run` refetches any
    /// result whose stored identity differs from the one its fetch used.
    #[allow(clippy::too_many_arguments)]
    async fn search_inner(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        req: PropertySearchRequest,
        correlation_id: Uuid,
        providers: &SearchProviders,
        timing: &mut SearchTiming,
        prefetch: bool,
    ) -> AppResult<(Vec<PropertyDigitalProfile>, Option<PermitJob>)> {
        let req = self.with_profile_limits(tx, project_id, req).await?;
        let pool_req = PropertySearchRequest {
            limit: (req.limit.saturating_mul(3)).min(50),
            ..req.clone()
        };
        let costar = self.providers.costar.clone();
        let matterport = providers.matterport.clone();
        let vexcel = providers.vexcel.clone();
        let media_base = self.app.settings.public_base_url.clone();
        let limit = usize::try_from(req.limit).unwrap_or(0);
        let bound = self.concurrency();
        let (best, listings, interior, pool_len) = blocking(
            move || -> AppResult<(Vec<PropertyDigitalProfile>, Duration, Duration, usize)> {
                let started = Instant::now();
                let pool = PropertyScoutService::new(costar).search(project_id, &pool_req)?;
                let listings = started.elapsed();
                let pool_len = pool.len();
                let started = Instant::now();
                // One `model_summaries` call per 20 ids for the whole pool, the calls
                // concurrent (see `enrich_interior_pool`).
                let mut pool = enrich_interior_pool(pool, &*matterport, bound);
                let interior = started.elapsed();
                // Prefer candidates whose listed suite has a measured interior scan (stable).
                pool.sort_by_key(|p| {
                    !p.interior_assets
                        .iter()
                        .any(|a| a.matches_listing_unit == Some(true))
                });
                let best: Vec<PropertyDigitalProfile> = pool.into_iter().take(limit).collect();
                Ok((best, listings, interior, pool_len))
            },
        )
        .await??;
        timing.listings = listings;
        timing.interior = interior;
        timing.pool = pool_len;
        timing.enriched = best.len();
        // The existing-address lookup of each result: `Some(stored or none)`, or `None` when an
        // earlier result of this search adds the same address (looked up again in the loop).
        let started = Instant::now();
        let mut known: Vec<Option<Option<PropertyDigitalProfile>>> = Vec::new();
        for (i, p) in best.iter().enumerate() {
            let address = p.property_identity.normalized_address.as_str();
            if best[..i]
                .iter()
                .zip(&known)
                .any(|(q, k)| matches!(k, Some(None)) && q.property_identity.normalized_address == address)
            {
                known.push(None);
                continue;
            }
            let existing = PROPERTIES
                .list(
                    tx,
                    Query::new()
                        .eq("project_id", project_id)
                        .eq("normalized_address", address)
                        .limit(1),
                )
                .await?;
            known.push(Some(existing.into_iter().next()));
        }
        let mut lookups = started.elapsed();
        let job = (prefetch && !best.is_empty()).then(|| {
            // The identity each result will have once stored (the exterior enrichment does not
            // change identities).
            let work: Vec<(PropertyIdentity, Uuid)> = best
                .iter()
                .zip(&known)
                .enumerate()
                .map(|(i, (p, k))| match k {
                    Some(Some(stored)) => (stored.property_identity.clone(), stored.project_id),
                    Some(None) => (p.property_identity.clone(), p.project_id),
                    None => {
                        let first = best[..i]
                            .iter()
                            .zip(&known)
                            .find(|(q, k)| {
                                matches!(k, Some(None))
                                    && q.property_identity.normalized_address
                                        == p.property_identity.normalized_address
                            })
                            .map_or(p, |(q, _)| q);
                        (first.property_identity.clone(), first.project_id)
                    }
                })
                .collect();
            let provider = providers.permits.clone();
            let handle = tokio::task::spawn_blocking({
                let work = work.clone();
                move || {
                    let started = Instant::now();
                    let answers = prefetch_permits_blocking(provider, work, bound);
                    (answers, started.elapsed())
                }
            });
            (work, handle)
        });
        let started = Instant::now();
        // The `limit` best, enriched concurrently, in their order.
        let found = blocking(move || {
            bounded_map(
                best,
                bound,
                |p| p,
                |p| enrich_exterior(p, &*vexcel, &media_base),
            )
        })
        .await?;
        timing.exterior = started.elapsed();
        let started = Instant::now();
        let mut saved: Vec<PropertyDigitalProfile> = Vec::new();
        for (p, k) in found.into_iter().zip(known) {
            let existing = match k {
                Some(existing) => existing,
                None => PROPERTIES
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
                    .await?
                    .into_iter()
                    .next(),
            };
            if let Some(stored) = existing {
                saved.push(stored);
                continue;
            }
            timing.stored_new += 1;
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
        lookups += started.elapsed();
        timing.search_store = lookups;
        Ok((saved, job))
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
        let fetched = match skip {
            Some(earlier) => Err((earlier.clone(), false)),
            None => permits::fetch(provider, &prop.property_identity, prop.project_id)
                .await?
                .map_err(|e| (e, true)),
        };
        self.store_fetched_permits(tx, prop, correlation_id, fetched)
            .await
    }

    /// The database half of the permit step, given the fetch's answer (see
    /// [`Self::ingest_permits_or_skip`]).
    async fn store_fetched_permits(
        &self,
        tx: &mut dyn Tx,
        prop: PropertyDigitalProfile,
        correlation_id: Uuid,
        fetched: Result<permits::Fetched, (ProviderError, bool)>,
    ) -> AppResult<(PropertyDigitalProfile, Option<PermitSkip>)> {
        let live = self.providers.permits.mode() == "live";
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
        let computing = Instant::now();
        let requirements = requirements_for(&profile);
        let (assessments, work) =
            FeasibilityEngine.evaluate(&profile, &requirements, &prop, &evidence)?;
        let mut compute = computing.elapsed();
        // Supersede previous outputs instead of deleting them (history preserved).
        supersede_property_outputs(tx, prop.property_id).await?;
        ASSESSMENTS.add_all(tx, &assessments).await?;
        WORK_ITEMS.add_all(tx, &work).await?;
        let computing = Instant::now();
        let scenario = FinancialCalculationService.scenario(&profile, &prop, &work, None, None)?;
        compute += computing.elapsed();
        self.compute_us.fetch_add(
            u64::try_from(compute.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
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

    /// `run`: the full slice for a project (the search route's task). Writes one `search timing`
    /// line (logger `app.search`) when the search has results.
    pub async fn run(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        req: PropertySearchRequest,
        correlation_id: Uuid,
    ) -> AppResult<JsonMap> {
        let run_started = Instant::now();
        let project = PROJECTS.require(tx, project_id).await?;
        let meters = [
            Arc::new(Meter::default()),
            Arc::new(Meter::default()),
            Arc::new(Meter::default()),
        ];
        let [m_matterport, m_vexcel, m_permits] = &meters;
        let providers = SearchProviders {
            matterport: Arc::new(MeteredMatterport(
                self.providers.matterport.clone(),
                m_matterport.clone(),
            )),
            vexcel: Arc::new(MeteredVexcel(
                self.providers.vexcel.clone(),
                m_vexcel.clone(),
            )),
            permits: Arc::new(MeteredPermits(
                self.providers.permits.clone(),
                m_permits.clone(),
            )),
        };
        let mut timing = SearchTiming::default();
        let (props, job) = self
            .search_inner(tx, project_id, req, correlation_id, &providers, &mut timing, true)
            .await?;
        if props.is_empty() {
            return Err(AppError::conflict("no candidate properties found").code("NO_CANDIDATES"));
        }
        // The permit fetches started during the search (concurrently, `prefetch_permits_blocking`);
        // after a permit timeout, the fetches not yet started skip the provider: one slow provider
        // costs about one timeout per search, not one per property. Then each property is stored,
        // attached and evaluated in order, exactly as when the fetch was made in the loop.
        let waiting = Instant::now();
        let (used, fetched) = match job {
            Some((used, handle)) => {
                let (answers, took) = handle
                    .await
                    .map_err(|e| AppError::internal(format!("provider task failed: {e}")))?;
                timing.permits_fetch = took;
                (used, answers)
            }
            None => (Vec::new(), Vec::new()),
        };
        timing.permits_wait = waiting.elapsed();
        let mut fetched = fetched.into_iter();
        let mut used = used.into_iter();
        let mut skipped: Vec<PermitSkip> = Vec::new();
        let mut timeout: Option<ProviderError> = None;
        let compute_before = self.compute_us.load(Ordering::Relaxed);
        for p in &props {
            let mut answer = fetched.next().ok_or_else(|| {
                AppError::internal("permit fetches and search results differ in number")
            })??;
            let predicted = used.next();
            if predicted.as_ref() != Some(&(p.property_identity.clone(), p.project_id)) {
                // Not the identity the fetch used (a stored row that differs from the one the
                // search predicted): fetch again for this one, as the loop did before.
                let started = Instant::now();
                timing.permit_refetches += 1;
                answer = match &timeout {
                    Some(e) => Err((e.clone(), false)),
                    None => permits::fetch(providers.permits.clone(), &p.property_identity, p.project_id)
                        .await?
                        .map_err(|e| (e, true)),
                };
                timing.permits_refetch += started.elapsed();
            }
            if let Err((e, true)) = &answer {
                if e.kind == ProviderErrorKind::Timeout && timeout.is_none() {
                    timeout = Some(e.clone());
                }
            }
            let started = Instant::now();
            let (p, skip) = self
                .store_fetched_permits(tx, p.clone(), correlation_id, answer)
                .await?;
            timing.permits_store += started.elapsed();
            if let Some(skip) = skip {
                skipped.push(skip);
            }
            let started = Instant::now();
            self.evaluate(tx, p, correlation_id, None).await?;
            timing.evaluate += started.elapsed();
        }
        timing.evaluate_compute = Duration::from_micros(
            self.compute_us
                .load(Ordering::Relaxed)
                .saturating_sub(compute_before),
        );
        let started = Instant::now();
        let ranking = self.rank(tx, project_id, correlation_id, None).await?;
        timing.rank = started.elapsed();
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
        let t = &timing;
        let phases = json!({
            "listings": ms(t.listings),
            "interior": ms(t.interior),
            "exterior": ms(t.exterior),
            "search_store": ms(t.search_store),
            "permits_fetch": ms(t.permits_fetch),
            "permits_wait": ms(t.permits_wait),
            "permits_refetch": ms(t.permits_refetch),
            "permits_store": ms(t.permits_store),
            "evaluate": ms(t.evaluate),
            "evaluate_compute": ms(t.evaluate_compute),
            "rank": ms(t.rank),
            "total": ms(run_started.elapsed()),
        });
        let counts = json!({
            "pool": t.pool,
            "enriched": t.enriched,
            "properties": props.len(),
            "new": t.stored_new,
            "permit_skipped": skipped.len(),
            "permit_refetches": t.permit_refetches,
            "bound": self.concurrency(),
        });
        let calls = json!({
            "matterport": m_matterport.to_json(),
            "vexcel": m_vexcel.to_json(),
            "permits": m_permits.to_json(),
        });
        if let Some(sink) = self.app.extensions.get::<SearchTimingSink>() {
            sink.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(json!({"phases_ms": phases, "counts": counts, "calls": calls}));
        }
        logging::info(
            "app.search",
            "search timing",
            &[("phases_ms", phases), ("counts", counts), ("calls", calls)],
        );
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
