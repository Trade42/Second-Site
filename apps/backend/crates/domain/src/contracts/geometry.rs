//! Port of `apps/api/app/contracts/geometry.py`. Geometry is meters in a local floor frame.
//!
//! `validate_ring` uses [`crate::geometry::explain_validity`] for Shapely's
//! `explain_validity(Polygon(points))` text.

use serde::{Deserialize, Serialize};
use ss_core::{
    new_id, string_enum, utcnow, Checker, Contract, DataOrigin, NonNegativeMinor, UtcDateTime,
    Validate,
};
use uuid::Uuid;

use crate::geometry;

/// `Coord = tuple[float, float]`: serialized as a two-element JSON array.
pub type Coord = (f64, f64);

/// `validate_ring(points, label)`: at least 3 points, a valid polygon (Shapely closes the ring),
/// non-zero area. `Err` carries the `ValueError` message.
pub fn validate_ring(points: &[Coord], label: &str) -> Result<(), String> {
    if points.len() < 3 {
        return Err(format!("{label} needs at least 3 points"));
    }
    let reason = geometry::explain_validity(points);
    if reason != "Valid Geometry" {
        return Err(format!("{label} is not a valid polygon: {reason}"));
    }
    if geometry::polygon_area(points) <= 0.0 {
        return Err(format!("{label} has zero area"));
    }
    Ok(())
}

string_enum! {
    pub enum GeometrySource {
        MatterportMeasured = "MATTERPORT_MEASURED",
        CadOrIfc = "CAD_OR_IFC",
        OwnerMeasured = "OWNER_MEASURED",
        /// Can never be scale_verified.
        PhotoEstimate = "PHOTO_ESTIMATE",
        Fixture = "FIXTURE",
        /// An assumed floor built from the listing's stated area (no scan of the suite): never
        /// measured, so it can never be scale_verified.
        ListedAreaEstimate = "LISTED_AREA_ESTIMATE",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opening {
    /// `'entrance' | 'exit' | 'window'`.
    pub kind: String,
    pub start: Coord,
    pub end: Coord,
}

impl Validate for Opening {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for Opening {
    const NAME: &'static str = "Opening";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    /// e.g. `'sink'`, `'water_heater'`, `'electrical_panel'`, `'drain'`.
    pub kind: String,
    pub location: Coord,
    #[serde(default)]
    pub evidence_ids: Vec<Uuid>,
}

impl Validate for Fixture {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for Fixture {
    const NAME: &'static str = "Fixture";
}

fn meters() -> String {
    "m".into()
}

fn local_floor_origin() -> String {
    "local_floor_origin".into()
}

pub const PHOTO_NOT_VERIFIED: &str = "photo-derived geometry cannot be scale_verified";

pub const LISTED_AREA_NOT_VERIFIED: &str =
    "a floor assumed from the listed area cannot be scale_verified";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteGeometry {
    #[serde(default = "new_id")]
    pub site_geometry_id: Uuid,
    pub property_id: Uuid,
    /// Provider asset id the geometry was derived from.
    pub source_id: String,
    pub geometry_source: GeometrySource,
    pub data_origin: DataOrigin,
    /// Internal geometry is always meters (pattern `^m$`).
    #[serde(default = "meters")]
    pub units: String,
    #[serde(default = "local_floor_origin")]
    pub coordinate_reference: String,
    pub floor_boundary: Vec<Coord>,
    #[serde(default)]
    pub fixed_obstacles: Vec<Vec<Coord>>,
    #[serde(default)]
    pub fixed_walls: Vec<(Coord, Coord)>,
    #[serde(default)]
    pub entrances: Vec<Opening>,
    #[serde(default)]
    pub exits: Vec<Opening>,
    #[serde(default)]
    pub windows: Vec<Opening>,
    #[serde(default)]
    pub fixture_locations: Vec<Fixture>,
    #[serde(default)]
    pub restricted_areas: Vec<Vec<Coord>>,
    #[serde(default)]
    pub scale_verified: bool,
    /// `>= 0`.
    #[serde(default)]
    pub measurement_uncertainty_m: Option<f64>,
}

fn check_rings(c: &mut Checker, field: &str, rings: &[Vec<Coord>]) {
    // Python's field validator stops at the first bad ring and reports it on the field.
    if let Some(msg) = rings
        .iter()
        .find_map(|r| validate_ring(r, "obstacle").err())
    {
        c.field_value_error(field, msg);
    }
}

impl Validate for SiteGeometry {
    fn check(&self, c: &mut Checker) {
        let mark = c.error_count();
        if self.units != "m" {
            c.push(
                Some("units"),
                "string_pattern_mismatch",
                "String should match pattern '^m$'",
            );
        }
        if let Err(msg) = validate_ring(&self.floor_boundary, "floor_boundary") {
            c.field_value_error("floor_boundary", msg);
        }
        check_rings(c, "fixed_obstacles", &self.fixed_obstacles);
        check_rings(c, "restricted_areas", &self.restricted_areas);
        c.ge_opt(
            "measurement_uncertainty_m",
            self.measurement_uncertainty_m,
            0.0,
        );
        c.model_rules(mark, |c| {
            if self.geometry_source == GeometrySource::PhotoEstimate && self.scale_verified {
                c.value_error(PHOTO_NOT_VERIFIED);
            }
            if self.geometry_source == GeometrySource::ListedAreaEstimate && self.scale_verified {
                c.value_error(LISTED_AREA_NOT_VERIFIED);
            }
        });
    }
}

impl Contract for SiteGeometry {
    const NAME: &'static str = "SiteGeometry";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomRequirement {
    pub room_type: String,
    /// `>= 1`.
    pub count: i64,
    /// `> 0`.
    pub min_area_sqm: f64,
    /// `> 0`.
    pub min_width_m: f64,
    #[serde(default)]
    pub needs_plumbing: bool,
}

impl Validate for RoomRequirement {
    fn check(&self, c: &mut Checker) {
        c.ge("count", self.count, 1);
        c.gt("min_area_sqm", self.min_area_sqm, 0.0);
        c.gt("min_width_m", self.min_width_m, 0.0);
    }
}

impl Contract for RoomRequirement {
    const NAME: &'static str = "RoomRequirement";
}

fn yes() -> bool {
    true
}

fn default_circulation() -> f64 {
    1.2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutPreferences {
    #[serde(default)]
    pub style: Option<String>,
    #[serde(default = "yes")]
    pub prefer_rooms_near_plumbing: bool,
    /// `> 0`.
    #[serde(default = "default_circulation")]
    pub circulation_width_m: f64,
}

impl Default for LayoutPreferences {
    fn default() -> Self {
        LayoutPreferences {
            style: None,
            prefer_rooms_near_plumbing: true,
            circulation_width_m: 1.2,
        }
    }
}

impl Validate for LayoutPreferences {
    fn check(&self, c: &mut Checker) {
        c.gt("circulation_width_m", self.circulation_width_m, 0.0);
    }
}

impl Contract for LayoutPreferences {
    const NAME: &'static str = "LayoutPreferences";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomPolygon {
    pub room_id: String,
    pub room_type: String,
    pub polygon: Vec<Coord>,
    /// `> 0`.
    pub area_sqm: f64,
}

impl Validate for RoomPolygon {
    fn check(&self, c: &mut Checker) {
        if let Err(msg) = validate_ring(&self.polygon, "room polygon") {
            c.field_value_error("polygon", msg);
        }
        c.gt("area_sqm", self.area_sqm, 0.0);
    }
}

impl Contract for RoomPolygon {
    const NAME: &'static str = "RoomPolygon";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FurniturePlacement {
    pub placement_id: String,
    pub room_id: String,
    pub item_type: String,
    pub footprint: Vec<Coord>,
}

impl Validate for FurniturePlacement {
    fn check(&self, c: &mut Checker) {
        if let Err(msg) = validate_ring(&self.footprint, "furniture footprint") {
            c.field_value_error("footprint", msg);
        }
    }
}

impl Contract for FurniturePlacement {
    const NAME: &'static str = "FurniturePlacement";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinishSpecification {
    #[serde(default)]
    pub room_id: Option<String>,
    /// `'floor' | 'wall' | 'ceiling'`.
    pub surface: String,
    pub material: String,
    /// `> 0`.
    pub area_sqm: f64,
}

impl Validate for FinishSpecification {
    fn check(&self, c: &mut Checker) {
        c.gt("area_sqm", self.area_sqm, 0.0);
    }
}

impl Contract for FinishSpecification {
    const NAME: &'static str = "FinishSpecification";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationIssue {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub room_ids: Vec<String>,
}

impl ValidationIssue {
    pub fn new(code: &str, message: impl Into<String>, room_ids: Vec<String>) -> Self {
        ValidationIssue {
            code: code.to_owned(),
            message: message.into(),
            room_ids,
        }
    }
}

impl Validate for ValidationIssue {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for ValidationIssue {
    const NAME: &'static str = "ValidationIssue";
}

/// The default `GeometryValidationReport.disclaimer`.
pub const GEOMETRY_DISCLAIMER: &str =
    "Automated geometric check only. Not a code-compliance review or professionally approved plan.";

fn geometry_disclaimer() -> String {
    GEOMETRY_DISCLAIMER.into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeometryValidationReport {
    pub valid: bool,
    #[serde(default)]
    pub issues: Vec<ValidationIssue>,
    #[serde(default = "utcnow")]
    pub checked_at: UtcDateTime,
    #[serde(default = "geometry_disclaimer")]
    pub disclaimer: String,
}

impl GeometryValidationReport {
    /// `GeometryValidationReport(valid=..., issues=...)` with the default timestamp and
    /// disclaimer.
    pub fn new(valid: bool, issues: Vec<ValidationIssue>) -> Self {
        GeometryValidationReport {
            valid,
            issues,
            checked_at: utcnow(),
            disclaimer: GEOMETRY_DISCLAIMER.into(),
        }
    }
}

impl Validate for GeometryValidationReport {
    fn check(&self, c: &mut Checker) {
        c.each("issues", &self.issues);
    }
}

impl Contract for GeometryValidationReport {
    const NAME: &'static str = "GeometryValidationReport";
}

/// What a layout on a floor assumed from the listing's stated area is, in fields a page reads
/// without parsing prose (`DesignVersion.floor_estimate`). Never present on a layout solved on a
/// measured, CAD, owner-measured, photo or fixture floor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloorEstimate {
    /// Always `LISTED_AREA_ESTIMATE`.
    pub geometry_source: GeometrySource,
    /// Always true: the floor's shape is an assumption.
    pub shape_assumed: bool,
    /// Always false: the area is the listing's stated figure, not a measurement.
    pub area_measured: bool,
    /// The listing's stated area, as stored (`> 0`).
    pub listed_area: f64,
    /// The unit of `listed_area` as stored (`'sqm'`: `CommercialListing.available_area_sqm`).
    pub listed_area_unit: String,
    /// The assumed shape, e.g. `'rectangle 3:2, one entrance mid short side'`.
    pub assumed_shape: String,
    /// What is not known about the real floor, e.g. `'columns'`, `'fixed_walls'`, `'plumbing'`,
    /// `'windows'`, `'exits'`.
    pub unknown: Vec<String>,
    /// What the sketch can and cannot show, in plain words.
    pub shows: String,
    /// A short label for display.
    pub label: String,
}

pub const FLOOR_ESTIMATE_NOT_MEASURED: &str =
    "a floor estimate is LISTED_AREA_ESTIMATE, its shape is assumed and its area is not measured";

impl Validate for FloorEstimate {
    fn check(&self, c: &mut Checker) {
        let mark = c.error_count();
        c.gt("listed_area", self.listed_area, 0.0);
        c.model_rules(mark, |c| {
            if self.geometry_source != GeometrySource::ListedAreaEstimate
                || !self.shape_assumed
                || self.area_measured
            {
                c.value_error(FLOOR_ESTIMATE_NOT_MEASURED);
            }
        });
    }
}

impl Contract for FloorEstimate {
    const NAME: &'static str = "FloorEstimate";
}

string_enum! {
    #[derive(Default)]
    pub enum DesignApprovalStatus {
        #[default]
        Draft = "DRAFT",
        Proposed = "PROPOSED",
        Approved = "APPROVED",
        Superseded = "SUPERSEDED",
        Rejected = "REJECTED",
    }
}

pub const INVALID_LAYOUT_NOT_APPROVABLE: &str = "an invalid layout cannot be approved";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignVersion {
    #[serde(default = "new_id")]
    pub design_version_id: Uuid,
    pub project_id: Uuid,
    pub property_id: Uuid,
    #[serde(default)]
    pub parent_version_id: Option<Uuid>,
    /// `>= 1`.
    pub version_number: i64,
    pub geometry_id: Uuid,
    pub room_polygons: Vec<RoomPolygon>,
    #[serde(default)]
    pub furniture_placements: Vec<FurniturePlacement>,
    #[serde(default)]
    pub proposed_walls: Vec<(Coord, Coord)>,
    #[serde(default)]
    pub finish_specifications: Vec<FinishSpecification>,
    pub validation_report: GeometryValidationReport,
    /// `>= 0` when present.
    #[serde(default)]
    pub estimated_construction_cost: Option<NonNegativeMinor>,
    #[serde(default)]
    pub created_from_revision_request: Option<Uuid>,
    #[serde(default)]
    pub approval_status: DesignApprovalStatus,
    /// Set only when the layout was solved on a floor assumed from the listing's stated area
    /// (`LISTED_AREA_ESTIMATE`): what is assumed and unknown. `null` otherwise, as the Python
    /// model dumps it: the key is always sent, last, like every other optional field here.
    #[serde(default)]
    pub floor_estimate: Option<FloorEstimate>,
}

impl Validate for DesignVersion {
    fn check(&self, c: &mut Checker) {
        let mark = c.error_count();
        c.ge("version_number", self.version_number, 1);
        c.each("room_polygons", &self.room_polygons);
        c.each("furniture_placements", &self.furniture_placements);
        c.each("finish_specifications", &self.finish_specifications);
        c.nested("validation_report", &self.validation_report);
        c.nested_opt("floor_estimate", &self.floor_estimate);
        c.ge_opt(
            "estimated_construction_cost",
            self.estimated_construction_cost,
            0,
        );
        c.model_rules(mark, |c| {
            if self.approval_status == DesignApprovalStatus::Approved
                && !self.validation_report.valid
            {
                c.value_error(INVALID_LAYOUT_NOT_APPROVABLE);
            }
        });
    }
}

impl Contract for DesignVersion {
    const NAME: &'static str = "DesignVersion";
}

string_enum! {
    #[derive(Default)]
    pub enum RevisionStatus {
        #[default]
        Received = "RECEIVED",
        Processing = "PROCESSING",
        Applied = "APPLIED",
        Infeasible = "INFEASIBLE",
        Rejected = "REJECTED",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignRevisionRequest {
    #[serde(default = "new_id")]
    pub revision_request_id: Uuid,
    pub project_id: Uuid,
    pub property_id: Uuid,
    pub base_design_version_id: Uuid,
    /// At least one entry.
    pub requested_changes: Vec<String>,
    #[serde(default)]
    pub room_requirement_overrides: Vec<RoomRequirement>,
    #[serde(default)]
    pub preserved_constraints: Vec<String>,
    pub requested_by: Uuid,
    /// `'web' | 'photon' | 'agent'`.
    pub source_channel: String,
    #[serde(default)]
    pub status: RevisionStatus,
}

impl Validate for DesignRevisionRequest {
    fn check(&self, c: &mut Checker) {
        if self.requested_changes.is_empty() {
            c.push(
                Some("requested_changes"),
                "too_short",
                "List should have at least 1 item after validation, not 0",
            );
        }
        c.each(
            "room_requirement_overrides",
            &self.room_requirement_overrides,
        );
    }
}

impl Contract for DesignRevisionRequest {
    const NAME: &'static str = "DesignRevisionRequest";
}

fn two() -> i64 {
    2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutGenerationRequest {
    pub property_id: Uuid,
    #[serde(default)]
    pub site_geometry_id: Option<Uuid>,
    pub room_requirements: Vec<RoomRequirement>,
    #[serde(default)]
    pub preferences: LayoutPreferences,
    /// `1..=5`.
    #[serde(default = "two")]
    pub candidate_count: i64,
}

impl Validate for LayoutGenerationRequest {
    fn check(&self, c: &mut Checker) {
        c.each("room_requirements", &self.room_requirements);
        c.nested("preferences", &self.preferences);
        c.ge("candidate_count", self.candidate_count, 1);
        c.le("candidate_count", self.candidate_count, 5);
    }
}

impl Contract for LayoutGenerationRequest {
    const NAME: &'static str = "LayoutGenerationRequest";
}
