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
//! - the solver runs on tokio's blocking pool (it is CPU work, up to a few seconds);
//! - a layout task runs in three phases ([`DesignService::run_layout_task`]) so that nothing is
//!   held while the floor is fetched from the provider and solved: a short read transaction, then
//!   the fetch and the solve with no transaction, pool connection or lock, then one write
//!   transaction with the task's whole visible lifecycle (F1 of the part 3 review). Python runs
//!   it all in the request's one transaction; its observable answers, rows and events are the
//!   same;
//! - the floor of an interior asset is read from the asset's own provider, not the configured
//!   mode ([`DesignService::prepare_site`]): only a `provider: "matterport"` asset goes to the
//!   configured Matterport provider, and any other asset (the fixture listing's stand-in scan,
//!   `provider: "fixture"`) is read from the fixture geometry whatever the mode, so a fixture id
//!   never reaches the live API. Python (`service.py:63-66`) sends every measured asset to the
//!   configured provider; it never showed there because its live mode also switched the
//!   listings to the CoStar kit (D1 of `orchestrator/reports/live-2.md`).

use std::sync::Arc;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use ss_core::{new_id, Contract, DataOrigin};
use ss_domain::contracts::geometry::FloorEstimate;
use ss_domain::contracts::{
    BillOfMaterials, DesignApprovalStatus, DesignRevisionRequest, DesignVersion, GeometrySource,
    LayoutGenerationRequest, LayoutPreferences, Opening, RevisionStatus, RoomRequirement,
    SiteGeometry,
};
use ss_domain::design::geometry::framing::{
    frame_for_solver_checked, normalize_plan, MIN_FRAME_AREA,
};
use ss_domain::design::geometry::solver::{generate_candidates, Candidate};
use ss_domain::design::interior::finishes::finish_specifications;
use ss_domain::integrations::base::ProviderError;
use ss_domain::procurement::bom::generate_bom_for;
use ss_domain::py;
use ss_domain::requirements::templates::sqm_per_room;
use uuid::Uuid;

use crate::app::App;
use crate::contracts::{ActorType, EventType, JsonMap, TaskRecord};
use crate::error::{AppError, AppResult};
use crate::http::extract::py_truthy;
use crate::integrations::blocking;
use crate::integrations::matterport::{FixtureMatterportProvider, MatterportProvider};
use crate::services::events::Emit;
use crate::store::repo::{designs, rooms, BOMS, DESIGNS, GEOMETRIES, PROFILES, PROPERTIES, TASKS};
use crate::store::{Query, Tx};

/// `ROOM_MIN_WIDTH` (meters); other room types get 2.4.
pub const ROOM_MIN_WIDTH: &[(&str, f64)] =
    &[("treatment", 2.8), ("reception", 3.0), ("storage", 1.5)];

/// How often [`DesignService::persist`] renumbers after losing the version-number race to a
/// writer that does not take the advisory lock.
pub const VERSION_ATTEMPTS: usize = 5;

/// How often [`DesignService::run_layout_task`] starts over when the property's stored geometry
/// changed between its read phase and its write phase (another request stored the property's
/// first geometry meanwhile; the task then solves again on that one).
pub const SITE_ATTEMPTS: usize = 3;

/// Where the floor of a layout comes from, read in a layout task's first phase.
#[derive(Debug, Clone)]
pub enum SiteSource {
    /// The property's last stored geometry that is not an assumed floor
    /// (`LISTED_AREA_ESTIMATE`).
    Stored(Box<SiteGeometry>),
    /// No measured geometry is stored yet: the property's interior assets with measured geometry
    /// (scans of the listed suite itself), in order, as `(asset_id, provider)` (the asset's own
    /// `provider`, e.g. `"matterport"` or `"fixture"`), and what an assumed floor would need when
    /// none of them has a floor.
    Assets(Vec<(String, String)>, Box<ListedArea>),
}

/// What a floor assumed from the listing needs: the listing's stated area and id, and the
/// assumed floor already stored for the property, if any (it is reused when it is still the
/// rectangle of the stated area).
#[derive(Debug, Clone, Default)]
pub struct ListedArea {
    /// `CommercialListing.available_area_sqm`, square meters, as stored.
    pub area_sqm: Option<f64>,
    /// The listing's id: `provider_listing_id`, or `listing-of-property:<property_id>` when the
    /// listing has none. Never a scan id.
    pub listing_id: String,
    /// The property's last stored `LISTED_AREA_ESTIMATE` geometry.
    pub stored: Option<SiteGeometry>,
}

/// The ratio of the assumed floor's long side to its short side. An assumption, not a
/// measurement: 3:2 is a common proportion for a small commercial suite (deeper than its
/// frontage, a corridor or storefront on the short side), and a moderate ratio keeps every room
/// program that fits a real suite of that area from being refused for the rectangle's shape alone.
pub const ASSUMED_RATIO: f64 = 1.5;

/// The width of the one assumed entrance (meters), a common commercial door leaf.
pub const ASSUMED_ENTRANCE_WIDTH_M: f64 = 0.9;

/// The smallest stated area laid out on an assumed floor (square meters): below it the listing's
/// figure is not a suite's floor area.
pub const MIN_LISTED_AREA_SQM: f64 = 1.0;

/// The largest stated area laid out on an assumed floor (square meters, about 107,600 sq ft).
/// A listed suite larger than this is far outside what a clinic or salon leases and is more
/// likely a data error (a building's or a parcel's area); it is refused, not sketched.
pub const MAX_LISTED_AREA_SQM: f64 = 10_000.0;

/// The `coordinate_reference` of an assumed floor.
pub const ASSUMED_COORDINATE_REFERENCE: &str =
    "assumed rectangle of the listed area; not measured; origin at one corner";

/// What a layout on an assumed floor does not know about the real one
/// (`FloorEstimate.unknown`).
pub const ASSUMED_UNKNOWN: &[&str] = &["columns", "fixed_walls", "plumbing", "windows", "exits"];

/// The task message when there is no measured floor and the listing states no area.
pub const NO_FLOOR_NO_AREA: &str = "this listing has no 3D scan of its own suite and states no \
                                    area, so there is no floor to lay rooms out on";

/// The floor assumed from a listing's stated area: a rectangle of exactly `area_sqm` square
/// meters, sides in the ratio [`ASSUMED_RATIO`] (short side along x, long side along y), one
/// assumed entrance of [`ASSUMED_ENTRANCE_WIDTH_M`] in the middle of the short side at y = 0, and
/// nothing else (no windows, exits, fixed walls, obstacles or fixtures: they are unknown).
/// `LISTED_AREA_ESTIMATE`, `DERIVED`, never `scale_verified`.
///
/// The short side is rounded to the centimetre and the long side is `area_sqm / short`, so the
/// area is exact and the ratio is 3:2 to within half a centimetre on the short side: the solver
/// places rooms on centimetre positions and rounds their corners to the millimetre, and on an
/// unrounded irrational width its validator finds rooms a fraction of a millimetre outside the
/// floor or inside the corridor and refuses every candidate.
///
/// `Err` (the message a person reads) when the area is not finite, below
/// [`MIN_LISTED_AREA_SQM`] or above [`MAX_LISTED_AREA_SQM`]: such an area is refused, not laid
/// out.
pub fn assumed_site(
    property_id: Uuid,
    listing_id: &str,
    area_sqm: f64,
) -> Result<SiteGeometry, String> {
    // NaN and the infinities are outside the range too.
    if !(MIN_LISTED_AREA_SQM..=MAX_LISTED_AREA_SQM).contains(&area_sqm) {
        return Err(format!(
            "this listing has no 3D scan of its own suite and its stated area ({area_sqm} m2) \
             cannot be a suite's floor (a stated area must be at least {MIN_LISTED_AREA_SQM:.0} \
             m2 and at most {MAX_LISTED_AREA_SQM:.0} m2), so no layout was sketched"
        ));
    }
    let short = ((area_sqm / ASSUMED_RATIO).sqrt() * 100.0).round() / 100.0;
    let long = area_sqm / short;
    let door = ASSUMED_ENTRANCE_WIDTH_M.min(short / 2.0);
    let mid = short / 2.0;
    let site = SiteGeometry {
        site_geometry_id: new_id(),
        property_id,
        source_id: listing_id.to_owned(),
        geometry_source: GeometrySource::ListedAreaEstimate,
        data_origin: DataOrigin::Derived,
        units: "m".into(),
        coordinate_reference: ASSUMED_COORDINATE_REFERENCE.into(),
        floor_boundary: vec![(0.0, 0.0), (short, 0.0), (short, long), (0.0, long)],
        fixed_obstacles: vec![],
        fixed_walls: vec![],
        entrances: vec![Opening {
            kind: "entrance".into(),
            start: (mid - door / 2.0, 0.0),
            end: (mid + door / 2.0, 0.0),
        }],
        exits: vec![],
        windows: vec![],
        fixture_locations: vec![],
        restricted_areas: vec![],
        scale_verified: false,
        measurement_uncertainty_m: None,
    };
    site.validate().map_err(|e| format!("assumed floor: {e}"))?;
    Ok(site)
}

/// The label of a layout on an assumed floor of `area_sqm` square meters (as stored).
pub fn floor_estimate(area_sqm: f64) -> FloorEstimate {
    FloorEstimate {
        geometry_source: GeometrySource::ListedAreaEstimate,
        shape_assumed: true,
        area_measured: false,
        listed_area: area_sqm,
        listed_area_unit: "sqm".into(),
        assumed_shape: "rectangle, sides 3:2, one entrance in the middle of a short side".into(),
        unknown: ASSUMED_UNKNOWN.iter().map(|u| (*u).to_owned()).collect(),
        shows: "whether the room program's areas fit the listed area, not where rooms can really \
                go"
        .into(),
        label: "Approximate sketch on the listed area; not a measured floor and not a plan".into(),
    }
}

/// Appended to each item's `price_note` in the bill of materials of a layout on an assumed floor.
pub const ESTIMATE_QUANTITY_NOTE: &str = "Quantity from an approximate sketch on the listing's \
                                          stated area, not from a measured floor.";

fn is_estimate(g: &SiteGeometry) -> bool {
    g.geometry_source == GeometrySource::ListedAreaEstimate
}

/// The floor a layout is solved on.
#[derive(Debug, Clone)]
pub enum PreparedSite {
    /// The property's last stored geometry when the task read it.
    Stored(SiteGeometry),
    /// Built from a provider document by this task; stored in the write phase unless another
    /// request stored one first.
    Fresh(SiteGeometry),
}

impl PreparedSite {
    pub fn geometry(&self) -> &SiteGeometry {
        match self {
            PreparedSite::Stored(g) | PreparedSite::Fresh(g) => g,
        }
    }
}

/// What a layout task does; the route builds it after its request checks.
#[derive(Debug, Clone)]
pub enum LayoutJob {
    Generate(LayoutGenerationRequest),
    Revise(DesignRevisionRequest),
}

/// What the versions of one generation are attached to.
#[derive(Debug, Clone, Copy)]
pub struct VersionContext<'r> {
    pub project_id: Uuid,
    pub correlation_id: Uuid,
    pub parent: Option<&'r DesignVersion>,
    pub revision_id: Option<Uuid>,
    /// The label of every version when the floor is assumed from the listed area.
    pub estimate: Option<&'r FloorEstimate>,
}

/// A layout task after its read phase and its solve.
struct Solved {
    req: LayoutGenerationRequest,
    site: PreparedSite,
    candidates: Vec<Candidate>,
    /// Why no candidate is valid (`None` when one is): the task's failure message.
    infeasible: Option<String>,
    /// The label of every version, set when the floor is assumed from the listed area.
    estimate: Option<FloorEstimate>,
}

/// The class name a layout task records as `error_code` when it stores no layout.
pub const LAYOUT_INFEASIBLE: &str = "LayoutInfeasibleError";

/// The message of a layout request no candidate satisfies: the measured floor, the part the
/// solver can use, what the room program needs and what the solver said. `frame` is the solver's
/// rectangle (longer side, shorter side) on that floor, `Ok(None)` when it found none, `Err` when
/// a work bound stopped the search (its size is then unknown, not absent).
pub fn infeasible_message(
    site: &SiteGeometry,
    req: &LayoutGenerationRequest,
    frame: Result<Option<(f64, f64)>, ()>,
    candidates: &[Candidate],
) -> String {
    let floor = ss_domain::geometry::polygon_area(&site.floor_boundary);
    let usable = match frame {
        Ok(Some((w, d))) => format!(
            "its largest rectangular area is {w:.1} x {d:.1} m = {:.0} m2",
            w * d
        ),
        Ok(None) => format!("it has no rectangular area of at least {MIN_FRAME_AREA:.0} m2"),
        Err(()) => "the size of its largest rectangular area is unknown".to_owned(),
    };
    let rooms: i64 = req.room_requirements.iter().map(|r| r.count).sum();
    let need: f64 = req
        .room_requirements
        .iter()
        .map(|r| r.count as f64 * r.min_area_sqm)
        .sum();
    let width = req
        .room_requirements
        .iter()
        .map(|r| r.min_width_m)
        .fold(0.0, f64::max);
    let said: Vec<String> = candidates
        .iter()
        .flat_map(|c| c.report.issues.iter().map(|i| i.message.clone()))
        .collect();
    let solver = if said.is_empty() {
        "no candidate".to_owned()
    } else {
        said.join("; ")
    };
    if is_estimate(site) {
        return format!(
            "No layout fits the assumed floor: there is no scan of this suite, so the floor was \
             assumed as a 3:2 rectangle of the listing's stated {floor:.0} m2 (not measured) and \
             {usable}; the room program needs {rooms} room(s), at least {need:.1} m2 in all plus \
             circulation, with minimum room widths up to {width:.1} m. Solver: {solver}. No layout \
             was stored."
        );
    }
    format!(
        "No layout fits the measured floor: the floor is {floor:.0} m2 and {usable}; the room \
         program needs {rooms} room(s), at least {need:.1} m2 in all plus circulation, with \
         minimum room widths up to {width:.1} m. Solver: {solver}. No layout was stored."
    )
}

fn ids(created: &[DesignVersion]) -> Value {
    json!(created
        .iter()
        .map(|d| d.design_version_id.to_string())
        .collect::<Vec<_>>())
}

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

    /// `site_geometry(property_id)`, first part (a layout task's read phase): the property's last
    /// stored geometry or, when none is stored yet, its interior assets with measured geometry.
    /// The property must belong to `project_id` (404 `property not found`), which Python did not
    /// check.
    pub async fn site_source(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        property_id: Uuid,
    ) -> AppResult<SiteSource> {
        let prop = PROPERTIES
            .require_in_project(tx, property_id, project_id, "property not found")
            .await?;
        let stored = GEOMETRIES
            .list(tx, Query::new().eq("property_id", property_id))
            .await?;
        // A stored assumed floor never shadows a measured one, nor a scan that has arrived since.
        if let Some(last) = stored.iter().rev().find(|g| !is_estimate(g)) {
            return Ok(SiteSource::Stored(Box::new(last.clone())));
        }
        let listing = prop.commercial_listing.as_ref();
        let listed = ListedArea {
            area_sqm: listing.and_then(|l| l.available_area_sqm),
            listing_id: listing
                .and_then(|l| l.provider_listing_id.clone())
                .unwrap_or_else(|| format!("listing-of-property:{property_id}")),
            stored: stored.into_iter().rev().find(is_estimate),
        };
        Ok(SiteSource::Assets(
            prop.interior_assets
                .iter()
                .filter(|a| a.has_measured_geometry)
                .map(|a| (a.asset_id.clone(), a.provider.clone()))
                .collect(),
            Box::new(listed),
        ))
    }

    /// `site_geometry(property_id)`, second part (no transaction, connection or lock held): the
    /// stored geometry as it is, or the first asset the Matterport provider has a document for,
    /// built into a `SiteGeometry` (normalized first when the document is `LIVE`); otherwise 404
    /// `GEOMETRY_UNAVAILABLE`.
    ///
    /// The document comes from the asset's own provider: an asset whose provider is
    /// `"matterport"` is read through the configured Matterport provider (live or fixture, as the
    /// mode says); any other asset is read from the fixture geometry whatever the mode
    /// ([`Self::fixture_matterport`]) and never reaches the live API. A fixture document says
    /// `FIXTURE_SYNTHETIC` (not `data_origin: LIVE`), so its `SiteGeometry` stays labelled
    /// fixture data. Python sends every asset to the configured provider (see the module doc).
    ///
    /// When no asset has a document and at least one was skipped because the provider does not
    /// know the model (or the token cannot see it, [`MatterportProvider::model_not_found`]), the
    /// 404 keeps `GEOMETRY_UNAVAILABLE` and its message says so. Python fails the task with the
    /// provider's `BAD_RESPONSE` there instead; with no such skip the message is Python's.
    pub async fn prepare_site(
        &self,
        property_id: Uuid,
        source: SiteSource,
    ) -> AppResult<PreparedSite> {
        let (assets, listed) = match source {
            SiteSource::Stored(g) => return Ok(PreparedSite::Stored(*g)),
            SiteSource::Assets(assets, listed) => (assets, listed),
        };
        let mut unknown_model = false;
        for (asset_id, provider) in assets {
            let matterport = if provider == crate::integrations::matterport::PROVIDER {
                self.app.integrations().matterport.clone()
            } else {
                self.fixture_matterport()
            };
            let id = asset_id.clone();
            let (doc, not_found) = blocking(move || {
                let doc = matterport.get_floor_geometry(&id)?;
                let not_found = doc.is_none() && matterport.model_not_found(&id);
                Ok::<_, ProviderError>((doc, not_found))
            })
            .await??;
            unknown_model |= not_found;
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
            return Ok(PreparedSite::Fresh(site_geometry_from_doc(
                property_id,
                &asset_id,
                &obj,
                live,
            )?));
        }
        if unknown_model {
            // The suite has a scan the provider cannot read: say so rather than sketch over it.
            return Err(AppError::not_found(
                "no measured geometry available for this property: Matterport does not know the \
                 model, or this token cannot see it",
            )
            .code("GEOMETRY_UNAVAILABLE"));
        }
        // No floor from a scan of the suite: an assumed floor on the listing's stated area.
        let Some(area) = listed.area_sqm else {
            return Err(AppError::not_found(NO_FLOOR_NO_AREA).code("GEOMETRY_UNAVAILABLE"));
        };
        let site = assumed_site(property_id, &listed.listing_id, area)
            .map_err(|m| AppError::not_found(m).code("GEOMETRY_UNAVAILABLE"))?;
        match listed.stored {
            Some(g) if g.floor_boundary == site.floor_boundary && g.source_id == site.source_id => {
                Ok(PreparedSite::Stored(g))
            }
            _ => Ok(PreparedSite::Fresh(site)),
        }
    }

    /// The Matterport provider for an asset whose provider is not `"matterport"`: the configured
    /// one when it is the fixture provider (fixture mode, so nothing moves there), otherwise the
    /// fixture geometry on the configured fixtures directory.
    fn fixture_matterport(&self) -> Arc<dyn MatterportProvider> {
        let configured = self.app.integrations().matterport.clone();
        if configured.mode() == "fixture" {
            configured
        } else {
            Arc::new(FixtureMatterportProvider::new(
                &self.app.settings.fixtures_dir,
            ))
        }
    }

    /// `site_geometry(property_id)`, last part (a layout task's write phase, after its first
    /// event took the project's event lock): a fresh geometry is stored and recorded as the
    /// property's `geometry_reference`. `None` when the property's stored geometry is no longer
    /// the one the layout was solved on (another request stored the first one meanwhile): the
    /// caller starts over and solves on the stored one, so a property keeps one geometry.
    pub async fn settle_site(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        site: &PreparedSite,
    ) -> AppResult<Option<SiteGeometry>> {
        let property_id = site.geometry().property_id;
        let latest = GEOMETRIES
            .list(tx, Query::new().eq("property_id", property_id))
            .await?
            .pop();
        match (site, latest) {
            (PreparedSite::Stored(g), Some(l)) if l.site_geometry_id == g.site_geometry_id => {
                Ok(Some(g.clone()))
            }
            // A fresh floor is stored when nothing is, or when only an assumed floor is (a scan
            // has arrived, or the listed area changed): an assumed floor never replaces a
            // measured one.
            (PreparedSite::Fresh(g), latest) if latest.as_ref().is_none_or(is_estimate) => {
                GEOMETRIES.add(tx, g).await?;
                if !is_estimate(g) {
                    let mut prop = PROPERTIES
                        .require_in_project(tx, property_id, project_id, "property not found")
                        .await?;
                    prop.geometry_reference = Some(g.site_geometry_id);
                    PROPERTIES.upsert(tx, &prop).await?;
                }
                // An assumed floor is not recorded as the property's `geometry_reference`: the
                // property has no measured geometry.
                Ok(Some(g.clone()))
            }
            _ => Ok(None),
        }
    }

    /// The layout solver, on tokio's blocking pool (CPU work, up to a few seconds); a layout task
    /// runs it with nothing held.
    pub async fn solve(
        site: &SiteGeometry,
        req: &LayoutGenerationRequest,
    ) -> AppResult<Vec<Candidate>> {
        let (s, reqs, prefs, count) = (
            site.clone(),
            req.room_requirements.clone(),
            req.preferences.clone(),
            req.candidate_count,
        );
        Ok(blocking(move || generate_candidates(&s, &reqs, &prefs, count)).await??)
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

    /// The write part of `generate(project_id, req, correlation_id, parent, revision_id)`, given
    /// the floor and the solver's candidates: one design version per valid candidate
    /// (`layout.version_created`), one `layout.validation_failed` per invalid one, which is never
    /// stored. Returns the created versions in candidate order.
    pub async fn record_candidates(
        &self,
        tx: &mut dyn Tx,
        ctx: &VersionContext<'_>,
        req: &LayoutGenerationRequest,
        site: &SiteGeometry,
        candidates: Vec<Candidate>,
    ) -> AppResult<Vec<DesignVersion>> {
        let VersionContext {
            project_id,
            correlation_id,
            parent,
            revision_id,
            estimate,
        } = *ctx;
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
                floor_estimate: estimate.cloned(),
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

    /// The checks of `revise(rev, correlation_id)` and the request it generates: 409
    /// `STALE_DESIGN_VERSION` when the base is superseded, 409 `REVISION_NOT_STRUCTURED` without
    /// room overrides; otherwise one candidate on the base's property with the overrides as the
    /// room program (default preferences, as in Python). The revision is `APPLIED` when that
    /// candidate is valid and stored, `INFEASIBLE` when not ([`DesignService::run_layout_task`]);
    /// the base is never modified.
    pub async fn revision_plan(
        &self,
        tx: &mut dyn Tx,
        rev: &DesignRevisionRequest,
    ) -> AppResult<(DesignVersion, LayoutGenerationRequest)> {
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
        Ok((base, req))
    }

    /// Run a layout generation or revision as a task, in three phases so that nothing is held
    /// while the floor is fetched and solved (F1 of the part 3 review):
    /// 1. a short read transaction: the property's stored geometry or the assets to fetch, and,
    ///    for a revision, its base and the checks on it ([`DesignService::revision_plan`]);
    /// 2. no transaction, pool connection or lock: the provider fetch (when nothing is stored)
    ///    and the solver, both on tokio's blocking pool;
    /// 3. one write transaction, as Python's `TaskService.run` (create, `task.started`, the work
    ///    in a savepoint, the outcome): `task.started` takes the project's event lock; the
    ///    revision checks run again; the geometry is stored only if none is stored yet, and if
    ///    another request stored one meanwhile the transaction is rolled back and the task starts
    ///    over on that geometry (at most [`SITE_ATTEMPTS`] times); then the versions, their
    ///    events and the task's outcome.
    ///
    /// Anything phases 1 and 2 raise is recorded in the task (FAILED, the Python class as
    /// `error_code`), as Python records what the work raises. `task` is the QUEUED record built
    /// when the request arrived, so its `created_at` is the request's time; nobody sees it
    /// before phase 3 commits (Python's one transaction did not show it earlier either). The
    /// lock order of phase 3 is the one of the single-transaction version: the task row (a key
    /// share on the project row), the event lock, the property row, the version lock.
    pub async fn run_layout_task(
        &self,
        task: TaskRecord,
        job: &LayoutJob,
    ) -> AppResult<TaskRecord> {
        let project_id = task.project_id;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let solved = self.plan_and_solve(project_id, job).await;
            let mut tx = self.app.store.begin().await?;
            TASKS.add(&mut *tx, &task).await?;
            let running = self.app.tasks().start(&mut *tx, task.clone()).await?;
            let correlation_id = running.task.correlation_id;
            // Deliberately different from Python, which completes a generation that stores no
            // design version and says nothing: such a task fails with the reason, keeping what
            // Python writes (the stored floor, one `layout.validation_failed` per candidate).
            let mut infeasible: Option<AppError> = None;
            let outcome = match solved {
                Err(e) => Err(e),
                Ok(s) => {
                    if let (LayoutJob::Generate(_), Some(msg)) = (job, &s.infeasible) {
                        infeasible = Some(
                            AppError::conflict(msg.clone())
                                .code("LAYOUT_INFEASIBLE")
                                .with_class_name(LAYOUT_INFEASIBLE),
                        );
                    }
                    match self
                        .write_layouts(&mut *tx, project_id, job, s, correlation_id)
                        .await
                    {
                        Ok(Some(out)) => Ok(out),
                        Ok(None) if attempt < SITE_ATTEMPTS => {
                            tx.rollback().await?;
                            continue;
                        }
                        Ok(None) => Err(AppError::conflict(
                            "the property's stored floor geometry changed while the layout was \
                             generated; try again",
                        )),
                        Err(e) => Err(e),
                    }
                }
            };
            let done = match (outcome, infeasible) {
                (Ok(_), Some(err)) => {
                    self.app
                        .tasks()
                        .fail_keeping(&mut *tx, running, err)
                        .await?
                }
                (outcome, _) => self.app.tasks().finish(&mut *tx, running, outcome).await?,
            };
            tx.commit().await?;
            return Ok(done);
        }
    }

    /// Phases 1 and 2 of [`DesignService::run_layout_task`].
    async fn plan_and_solve(&self, project_id: Uuid, job: &LayoutJob) -> AppResult<Solved> {
        let mut tx = self.app.store.begin().await?;
        let planned = async {
            let req = match job {
                LayoutJob::Generate(req) => req.clone(),
                LayoutJob::Revise(rev) => self.revision_plan(&mut *tx, rev).await?.1,
            };
            let source = self
                .site_source(&mut *tx, project_id, req.property_id)
                .await?;
            AppResult::Ok((req, source))
        }
        .await;
        // Read only: nothing to keep.
        tx.rollback().await?;
        let (req, source) = planned?;
        let listed_area = match &source {
            SiteSource::Assets(_, listed) => listed.area_sqm,
            SiteSource::Stored(_) => None,
        };
        let site = self.prepare_site(req.property_id, source).await?;
        // `prepare_site` only builds or reuses an assumed floor from the stated area.
        let estimate = listed_area
            .filter(|_| is_estimate(site.geometry()))
            .map(floor_estimate);
        let candidates = Self::solve(site.geometry(), &req).await?;
        let infeasible = if candidates.iter().any(|c| c.report.valid) {
            None
        } else {
            // The solver's rectangle again, only to say its size (CPU work, nothing held).
            let g = site.geometry().clone();
            let frame = blocking(move || {
                frame_for_solver_checked(&g, MIN_FRAME_AREA)
                    .map(|f| {
                        f.and_then(|f| ss_domain::geometry::Bounds::of(&f.site.floor_boundary))
                            .map(|b| {
                                let (w, d) = (b.maxx - b.minx, b.maxy - b.miny);
                                (w.max(d), w.min(d))
                            })
                    })
                    .map_err(|_| ())
            })
            .await?;
            Some(infeasible_message(
                site.geometry(),
                &req,
                frame,
                &candidates,
            ))
        };
        Ok(Solved {
            req,
            site,
            candidates,
            infeasible,
            estimate,
        })
    }

    /// The work of phase 3, after `task.started`: the task's output, or `None` when the stored
    /// geometry changed since phase 1 (start over).
    async fn write_layouts(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        job: &LayoutJob,
        solved: Solved,
        correlation_id: Uuid,
    ) -> AppResult<Option<JsonMap>> {
        // A revision's checks again, under the lock: its base may have been superseded since.
        let (base, revision_id) = match job {
            LayoutJob::Generate(_) => (None, None),
            LayoutJob::Revise(rev) => (
                Some(self.revision_plan(tx, rev).await?.0),
                Some(rev.revision_request_id),
            ),
        };
        let Some(site) = self.settle_site(tx, project_id, &solved.site).await? else {
            return Ok(None);
        };
        let ctx = VersionContext {
            project_id,
            correlation_id,
            parent: base.as_ref(),
            revision_id,
            estimate: solved.estimate.as_ref(),
        };
        let created = self
            .record_candidates(tx, &ctx, &solved.req, &site, solved.candidates)
            .await?;
        let mut out = JsonMap::new();
        if let Some(e) = &solved.estimate {
            out.insert(
                "floor_estimate".into(),
                serde_json::to_value(e)
                    .map_err(|e| AppError::internal(format!("serialize floor estimate: {e}")))?,
            );
        }
        match &base {
            None => {
                out.insert("design_version_ids".into(), ids(&created));
            }
            Some(base) => {
                let status = if created.is_empty() {
                    // The last valid design stays current.
                    RevisionStatus::Infeasible
                } else {
                    RevisionStatus::Applied
                };
                let current = created
                    .first()
                    .map_or(base.design_version_id, |d| d.design_version_id);
                out.insert("revision_status".into(), json!(status.as_str()));
                out.insert("design_version_ids".into(), ids(&created));
                out.insert(
                    "current_valid_version_id".into(),
                    json!(current.to_string()),
                );
            }
        }
        Ok(Some(out))
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
        // The furniture follows the kind of business of the project's current profile; with no
        // profile stored it is the list every project had before kinds (`treatment`).
        let business_type = PROFILES
            .list(tx, Query::new().eq("project_id", project_id))
            .await?
            .pop()
            .map(|p| p.business_type)
            .unwrap_or_default();
        let mut b = generate_bom_for(dv, &business_type)?;
        if dv.floor_estimate.is_some() {
            // Quantities of a sketch on an assumed floor are not take-offs of a measured one.
            for item in &mut b.items {
                item.price_note = Some(match item.price_note.take() {
                    Some(n) => format!("{n} {ESTIMATE_QUANTITY_NOTE}"),
                    None => ESTIMATE_QUANTITY_NOTE.to_owned(),
                });
            }
        }
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
