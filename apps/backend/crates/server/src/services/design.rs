//! Port of `apps/api/app/domains/design/service.py` (`DesignService`): a measured floor and a
//! room program become versioned `DesignVersion`s; revisions never overwrite.
//!
//! Truth rules carried in code: every generated version is `PROPOSED`, never `APPROVED`; an
//! invalid candidate is never stored as a design version (it only emits
//! `layout.validation_failed`); an infeasible revision leaves its base untouched; fixture
//! geometry stays `FIXTURE_SYNTHETIC` and only a provider document marked `LIVE` is stored as
//! `LIVE`. Every report keeps the "Automated geometric check only. Not a code-compliance review
//! or professionally approved plan." disclaimer from the solver.
//!
//! Differences from Python, all deliberate:
//! - every record is loaded through the project: the property of a layout request must belong
//!   to the URL project (Python only checked that it exists), and a revision's base must too;
//! - version numbers are unique under concurrency: [`DesignService::persist`] takes a
//!   per-property advisory lock before it reads `max(version_number)`, and retries on the
//!   `(property_id, version_number)` unique constraint (for writers that do not take the lock);
//! - `GET .../bom` takes a per-design-version advisory lock before it looks for a stored bill,
//!   so two concurrent first requests store one bill and emit one `bom.generated` (Python can
//!   store two);
//! - the solver runs on tokio's blocking pool (it is CPU work, up to a few seconds).

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use ss_core::{new_id, Contract, DataOrigin};
use ss_domain::contracts::{
    BillOfMaterials, DesignApprovalStatus, DesignRevisionRequest, DesignVersion, GeometrySource,
    LayoutGenerationRequest, LayoutPreferences, RevisionStatus, RoomRequirement, SiteGeometry,
};
use ss_domain::design::geometry::framing::normalize_plan;
use ss_domain::design::geometry::solver::generate_candidates;
use ss_domain::design::interior::finishes::finish_specifications;
use ss_domain::procurement::bom::generate_bom;
use ss_domain::py;
use ss_domain::requirements::templates::sqm_per_room;
use uuid::Uuid;

use crate::app::App;
use crate::contracts::{ActorType, EventType};
use crate::error::{AppError, AppResult};
use crate::http::extract::py_truthy;
use crate::integrations::blocking;
use crate::services::events::Emit;
use crate::store::repo::{designs, rooms, BOMS, DESIGNS, GEOMETRIES, PROPERTIES};
use crate::store::{Query, Tx};

/// `ROOM_MIN_WIDTH` (meters); other room types get 2.4.
pub const ROOM_MIN_WIDTH: &[(&str, f64)] =
    &[("treatment", 2.8), ("reception", 3.0), ("storage", 1.5)];

/// How often [`DesignService::persist`] renumbers after losing the version-number race to a
/// writer that does not take the advisory lock.
pub const VERSION_ATTEMPTS: usize = 5;

/// `room_requirements_from_profile(rooms, plumbing_rooms)`: one requirement per room type with a
/// positive count, in the profile's order. `plumbing_rooms` empty or `None` means
/// `{"treatment"}` (Python's `plumbing_rooms or {"treatment"}`).
pub fn room_requirements_from_profile<'a>(
    rooms: impl IntoIterator<Item = (&'a String, &'a i64)>,
    plumbing_rooms: Option<&[&str]>,
) -> AppResult<Vec<RoomRequirement>> {
    let plumbing: &[&str] = match plumbing_rooms {
        Some(p) if !p.is_empty() => p,
        _ => &["treatment"],
    };
    let mut out = Vec::new();
    for (k, &v) in rooms {
        if v <= 0 {
            continue;
        }
        let r = RoomRequirement {
            room_type: k.clone(),
            count: v,
            min_area_sqm: py::round(sqm_per_room(k) * 0.9, 1),
            min_width_m: ROOM_MIN_WIDTH
                .iter()
                .find(|(t, _)| t == k)
                .map_or(2.4, |(_, w)| *w),
            needs_plumbing: plumbing.contains(&k.as_str()),
        };
        r.validate()?;
        out.push(r);
    }
    Ok(out)
}

/// A transaction-scoped advisory lock key: the first 8 bytes of
/// `SHA-256("second-site:<namespace>:<id>")` as a big-endian signed integer, the same formula as
/// the event lock (`services::events::event_lock_key`, namespace `activity_events`). Python:
/// `int.from_bytes(hashlib.sha256(f"second-site:{ns}:{id}".encode()).digest()[:8], "big",
/// signed=True)`.
pub fn lock_key(namespace: &str, id: Uuid) -> i64 {
    let digest = Sha256::digest(format!("second-site:{namespace}:{id}").as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(b)
}

/// The lock that serializes version numbering of one property's design versions.
pub fn version_lock_key(property_id: Uuid) -> i64 {
    lock_key("design_versions", property_id)
}

/// The lock that serializes the first bill of materials of one design version.
pub fn bom_lock_key(design_version_id: Uuid) -> i64 {
    lock_key("bills_of_materials", design_version_id)
}

fn list_or_empty(doc: &Map<String, Value>, key: &str) -> Value {
    doc.get(key).cloned().unwrap_or_else(|| json!([]))
}

/// `[tuple(tuple(p) for p in w) for w in doc.get("fixed_walls", [])]`: anything that is not a
/// list of lists of points fails like Python's `TypeError` (a point's own shape is left to the
/// contract, as Pydantic checks it).
fn fixed_walls(doc: &Map<String, Value>) -> AppResult<Value> {
    let type_error = || AppError::internal_kind("TypeError", "fixed_walls is not a list of walls");
    match doc.get("fixed_walls") {
        None => Ok(json!([])),
        Some(Value::Array(walls)) => {
            for w in walls {
                let Value::Array(points) = w else {
                    return Err(type_error());
                };
                if points.iter().any(|p| !p.is_array()) {
                    return Err(type_error());
                }
            }
            Ok(Value::Array(walls.clone()))
        }
        Some(_) => Err(type_error()),
    }
}

/// The `SiteGeometry(...)` call of `DesignService.site_geometry` for one provider document
/// (already normalized when it is `LIVE`).
pub fn site_geometry_from_doc(
    property_id: Uuid,
    source_id: &str,
    doc: &Map<String, Value>,
    live: bool,
) -> AppResult<SiteGeometry> {
    // `GeometrySource(doc.get("geometry_source", "FIXTURE"))`: an unknown value is a ValueError.
    let source = doc
        .get("geometry_source")
        .cloned()
        .unwrap_or_else(|| json!("FIXTURE"));
    let geometry_source: GeometrySource = serde_json::from_value(source.clone()).map_err(|_| {
        AppError::internal_kind(
            "ValueError",
            format!("{source} is not a valid GeometrySource"),
        )
    })?;
    let floor_boundary = doc
        .get("floor_boundary")
        .cloned()
        .ok_or_else(|| AppError::internal_kind("KeyError", "'floor_boundary'"))?;
    let fields = json!({
        "property_id": property_id.to_string(),
        "source_id": source_id,
        "geometry_source": geometry_source.as_str(),
        "data_origin": if live { DataOrigin::Live } else { DataOrigin::FixtureSynthetic }.as_str(),
        "coordinate_reference": doc
            .get("coordinate_reference")
            .cloned()
            .unwrap_or_else(|| json!("local_floor_origin")),
        "floor_boundary": floor_boundary,
        "fixed_obstacles": list_or_empty(doc, "fixed_obstacles"),
        "fixed_walls": fixed_walls(doc)?,
        "entrances": list_or_empty(doc, "entrances"),
        "exits": list_or_empty(doc, "exits"),
        "windows": list_or_empty(doc, "windows"),
        "fixture_locations": list_or_empty(doc, "fixture_locations"),
        "scale_verified": doc.get("scale_verified").is_some_and(py_truthy),
        "measurement_uncertainty_m": doc.get("measurement_uncertainty_m").cloned().unwrap_or(Value::Null),
    });
    Ok(SiteGeometry::from_json_value(fields)?)
}

/// `DesignService`.
pub struct DesignService<'a> {
    app: &'a App,
}

impl<'a> DesignService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// `site_geometry(property_id)`: the property's last stored geometry; otherwise the first
    /// interior asset with measured geometry that the Matterport provider has a document for
    /// (stored, and recorded as the property's `geometry_reference`); otherwise 404
    /// `GEOMETRY_UNAVAILABLE`. The property must belong to `project_id` (404 `property not
    /// found`), which Python did not check.
    pub async fn site_geometry(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        property_id: Uuid,
    ) -> AppResult<SiteGeometry> {
        let mut prop = PROPERTIES
            .require_in_project(tx, property_id, project_id, "property not found")
            .await?;
        let mut stored = GEOMETRIES
            .list(tx, Query::new().eq("property_id", property_id))
            .await?;
        if let Some(last) = stored.pop() {
            return Ok(last);
        }
        let assets: Vec<String> = prop
            .interior_assets
            .iter()
            .filter(|a| a.has_measured_geometry)
            .map(|a| a.asset_id.clone())
            .collect();
        for asset_id in assets {
            let matterport = self.app.integrations().matterport.clone();
            let id = asset_id.clone();
            let doc = blocking(move || matterport.get_floor_geometry(&id)).await??;
            let Some(doc) = doc.filter(py_truthy) else {
                continue;
            };
            let Value::Object(obj) = doc else {
                return Err(AppError::internal_kind(
                    "AttributeError",
                    "geometry document is not an object",
                ));
            };
            let live = obj.get("data_origin").and_then(Value::as_str) == Some("LIVE");
            let obj = if live {
                match normalize_plan(&Value::Object(obj))? {
                    Value::Object(m) => m,
                    _ => {
                        return Err(AppError::internal_kind(
                            "TypeError",
                            "normalized geometry is not an object",
                        ))
                    }
                }
            } else {
                obj
            };
            let geo = site_geometry_from_doc(property_id, &asset_id, &obj, live)?;
            GEOMETRIES.add(tx, &geo).await?;
            prop.geometry_reference = Some(geo.site_geometry_id);
            PROPERTIES.upsert(tx, &prop).await?;
            return Ok(geo);
        }
        Err(
            AppError::not_found("no measured geometry available for this property")
                .code("GEOMETRY_UNAVAILABLE"),
        )
    }

    /// `_next_version(property_id)`: `max(version_number) + 1`, 1 for the first version.
    pub async fn next_version(&self, tx: &mut dyn Tx, property_id: Uuid) -> AppResult<i64> {
        Ok(designs::max_version_number(tx, property_id)
            .await?
            .unwrap_or(0)
            + 1)
    }

    /// `_persist(dv)`, numbering the version first: the design version plus one `rooms` row per
    /// room polygon.
    ///
    /// Uniqueness of `version_number` under concurrency: the per-property advisory lock
    /// ([`version_lock_key`]) is taken before `max(version_number)` is read and held until the
    /// transaction ends, so two Rust writers for one property number one after the other. A
    /// writer that does not take the lock (Python) is caught by the unique constraint on
    /// `(property_id, version_number)`: the insert runs in a savepoint and is renumbered from the
    /// committed rows, up to [`VERSION_ATTEMPTS`] times.
    pub async fn persist(&self, tx: &mut dyn Tx, dv: &mut DesignVersion) -> AppResult<()> {
        tx.advisory_xact_lock(version_lock_key(dv.property_id))
            .await?;
        let mut stored = false;
        for _ in 0..VERSION_ATTEMPTS {
            dv.version_number = self.next_version(tx, dv.property_id).await?;
            dv.validate()?;
            let sp = tx.savepoint().await?;
            match designs::add_numbered(tx, dv).await {
                Ok(true) => {
                    tx.release_savepoint(sp).await?;
                    stored = true;
                    break;
                }
                // Another writer took this number first; renumber from what it committed.
                Ok(false) => tx.rollback_to_savepoint(sp).await?,
                Err(e) => {
                    tx.rollback_to_savepoint(sp).await?;
                    return Err(e);
                }
            }
        }
        if !stored {
            return Err(
                AppError::conflict("could not assign a unique design version number").details(
                    json!({"table": "design_versions", "constraint": designs::VERSION_CONSTRAINT}),
                ),
            );
        }
        for r in &dv.room_polygons {
            let polygon = serde_json::to_value(&r.polygon)
                .map_err(|e| AppError::internal(format!("serialize room polygon: {e}")))?;
            rooms::insert(
                tx,
                &rooms::RoomRow {
                    id: Uuid::new_v4(),
                    design_version_id: dv.design_version_id,
                    room_key: r.room_id.clone(),
                    room_type: r.room_type.clone(),
                    area_sqm: r.area_sqm,
                    polygon,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// `generate(project_id, req, correlation_id, parent, revision_id)`: one design version per
    /// valid candidate (`layout.version_created`), one `layout.validation_failed` per invalid
    /// one, which is never stored. Returns the created versions in candidate order.
    pub async fn generate(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        req: &LayoutGenerationRequest,
        correlation_id: Uuid,
        parent: Option<&DesignVersion>,
        revision_id: Option<Uuid>,
    ) -> AppResult<Vec<DesignVersion>> {
        let site = self.site_geometry(tx, project_id, req.property_id).await?;
        let (s, reqs, prefs, count) = (
            site.clone(),
            req.room_requirements.clone(),
            req.preferences.clone(),
            req.candidate_count,
        );
        let candidates = blocking(move || generate_candidates(&s, &reqs, &prefs, count)).await??;
        let mut created = Vec::new();
        for cand in candidates {
            if !cand.report.valid {
                let issue_codes: Vec<&str> =
                    cand.report.issues.iter().map(|i| i.code.as_str()).collect();
                self.app
                    .events()
                    .emit(
                        tx,
                        Emit::new(
                            EventType::LayoutValidationFailed,
                            project_id,
                            ActorType::Agent,
                            "floor-plan-architect",
                            format!(
                                "layout-failed:{}:{}:{}",
                                req.property_id, correlation_id, cand.name
                            ),
                            json!({
                                "design_version_id": null,
                                "property_id": req.property_id.to_string(),
                                "valid": false,
                                "issue_codes": issue_codes,
                            }),
                        )
                        .correlation(correlation_id),
                    )
                    .await?;
                continue;
            }
            let finishes = finish_specifications(&cand.rooms, req.preferences.style.as_deref());
            let mut dv = DesignVersion {
                design_version_id: new_id(),
                project_id,
                property_id: req.property_id,
                parent_version_id: parent.map(|p| p.design_version_id),
                // Assigned by `persist` under the version lock.
                version_number: 1,
                geometry_id: site.site_geometry_id,
                room_polygons: cand.rooms,
                furniture_placements: vec![],
                proposed_walls: vec![],
                finish_specifications: finishes,
                validation_report: cand.report,
                estimated_construction_cost: None,
                created_from_revision_request: revision_id,
                // Never approved here: approval is the owner's decision.
                approval_status: DesignApprovalStatus::Proposed,
            };
            self.persist(tx, &mut dv).await?;
            self.app
                .events()
                .emit(
                    tx,
                    Emit::new(
                        EventType::LayoutVersionCreated,
                        project_id,
                        ActorType::Agent,
                        "floor-plan-architect",
                        format!("layout:{}", dv.design_version_id),
                        json!({
                            "design_version_id": dv.design_version_id.to_string(),
                            "property_id": req.property_id.to_string(),
                            "valid": true,
                            "issue_codes": [],
                        }),
                    )
                    .correlation(correlation_id),
                )
                .await?;
            created.push(dv);
        }
        Ok(created)
    }

    /// `revise(rev, correlation_id)`: 409 `STALE_DESIGN_VERSION` when the base is superseded,
    /// 409 `REVISION_NOT_STRUCTURED` without room overrides; otherwise one candidate on the
    /// base's property with the overrides as the room program (default preferences, as in
    /// Python), `APPLIED` when it is valid and stored, `INFEASIBLE` when not. The base is never
    /// modified.
    pub async fn revise(
        &self,
        tx: &mut dyn Tx,
        rev: &DesignRevisionRequest,
        correlation_id: Uuid,
    ) -> AppResult<(DesignRevisionRequest, Vec<DesignVersion>)> {
        let base = DESIGNS
            .require_in_project(
                tx,
                rev.base_design_version_id,
                rev.project_id,
                "layout not found",
            )
            .await?;
        if base.approval_status == DesignApprovalStatus::Superseded {
            return Err(
                AppError::conflict("base design version is stale (superseded)")
                    .code("STALE_DESIGN_VERSION"),
            );
        }
        if rev.room_requirement_overrides.is_empty() {
            return Err(AppError::conflict(
                "revision needs room_requirement_overrides in the foundation build",
            )
            .code("REVISION_NOT_STRUCTURED"));
        }
        let req = LayoutGenerationRequest {
            property_id: base.property_id,
            site_geometry_id: Some(base.geometry_id),
            room_requirements: rev.room_requirement_overrides.clone(),
            preferences: LayoutPreferences::default(),
            candidate_count: 1,
        };
        req.validate()?;
        let created = self
            .generate(
                tx,
                rev.project_id,
                &req,
                correlation_id,
                Some(&base),
                Some(rev.revision_request_id),
            )
            .await?;
        let mut updated = rev.clone();
        updated.status = if created.is_empty() {
            // The last valid design stays current.
            RevisionStatus::Infeasible
        } else {
            RevisionStatus::Applied
        };
        Ok((updated, created))
    }

    /// `GET .../layouts/{id}/bom`: the design version's latest stored bill, or a new one
    /// (stored, `bom.generated`). Prices are illustrative fixtures, never supplier quotes.
    pub async fn bill_of_materials(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        dv: &DesignVersion,
    ) -> AppResult<BillOfMaterials> {
        tx.advisory_xact_lock(bom_lock_key(dv.design_version_id))
            .await?;
        let mut existing = BOMS
            .list(
                tx,
                Query::new().eq("design_version_id", dv.design_version_id),
            )
            .await?;
        if let Some(last) = existing.pop() {
            return Ok(last);
        }
        let b = generate_bom(dv)?;
        BOMS.add(tx, &b).await?;
        self.app
            .events()
            .emit(
                tx,
                Emit::new(
                    EventType::BomGenerated,
                    project_id,
                    ActorType::Agent,
                    "procurement",
                    format!("bom:{}", b.bom_id),
                    json!({
                        "bom_id": b.bom_id.to_string(),
                        "design_version_id": dv.design_version_id.to_string(),
                        "estimated_total": b.estimated_total,
                    }),
                ),
            )
            .await?;
        Ok(b)
    }
}
