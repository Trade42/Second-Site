//! Port of `apps/api/app/contracts/project.py`.

use chrono::NaiveDate;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::{
    default_schema_version, new_id, utcnow, CurrencyCode, NonNegativeMinor, UtcDateTime,
};
use crate::string_enum;
use crate::validation::{Checker, Contract, Validate};

string_enum! {
    #[derive(Default)]
    pub enum ProjectStatus {
        #[default]
        Onboarding = "ONBOARDING",
        Searching = "SEARCHING",
        Analyzing = "ANALYZING",
        Comparing = "COMPARING",
        Designing = "DESIGNING",
        Procuring = "PROCURING",
        Opening = "OPENING",
        Archived = "ARCHIVED",
    }
}

fn default_country() -> String {
    "US".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Market {
    pub city: String,
    /// Exactly two characters.
    pub state: String,
    #[serde(default = "default_country")]
    pub country: String,
}

impl Market {
    pub fn new(city: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            city: city.into(),
            state: state.into(),
            country: default_country(),
        }
    }
}

impl Validate for Market {
    fn check(&self, c: &mut Checker) {
        c.min_len("state", &self.state, 2);
        c.max_len("state", &self.state, 2);
    }
}

impl Contract for Market {
    const NAME: &'static str = "Market";
}

fn one() -> i64 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectContext {
    #[serde(default = "new_id")]
    pub project_id: Uuid,
    pub owner_id: Uuid,
    /// 1..=200 characters.
    pub project_name: String,
    #[serde(default)]
    pub business_profile_id: Option<Uuid>,
    #[serde(default)]
    pub project_status: ProjectStatus,
    pub market: Market,
    #[serde(default)]
    pub selected_property_id: Option<Uuid>,
    #[serde(default)]
    pub selected_design_version_id: Option<Uuid>,
    /// `>= 0`.
    #[serde(default)]
    pub current_budget_revision: i64,
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    /// Optimistic concurrency counter, `>= 1`.
    #[serde(default = "one")]
    pub record_version: i64,
    #[serde(default = "utcnow")]
    pub created_at: UtcDateTime,
    #[serde(default = "utcnow")]
    pub updated_at: UtcDateTime,
}

impl ProjectContext {
    /// Required fields only; every other field takes its Python default.
    pub fn new(owner_id: Uuid, project_name: impl Into<String>, market: Market) -> Self {
        Self {
            project_id: new_id(),
            owner_id,
            project_name: project_name.into(),
            business_profile_id: None,
            project_status: ProjectStatus::default(),
            market,
            selected_property_id: None,
            selected_design_version_id: None,
            current_budget_revision: 0,
            schema_version: default_schema_version(),
            record_version: 1,
            created_at: utcnow(),
            updated_at: utcnow(),
        }
    }
}

impl Validate for ProjectContext {
    fn check(&self, c: &mut Checker) {
        c.min_len("project_name", &self.project_name, 1);
        c.max_len("project_name", &self.project_name, 200);
        c.nested("market", &self.market);
        c.ge("current_budget_revision", self.current_budget_revision, 0);
        c.ge("record_version", self.record_version, 1);
    }
}

impl Contract for ProjectContext {
    const NAME: &'static str = "ProjectContext";
}

string_enum! {
    #[derive(Default)]
    pub enum ConfirmationStatus {
        #[default]
        Draft = "DRAFT",
        /// Filled by the onboarding agent, not yet confirmed.
        Inferred = "INFERRED",
        /// Owner confirmed.
        Confirmed = "CONFIRMED",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldProvenance {
    pub field: String,
    /// e.g. 'owner_form', 'onboarding_agent', 'chat:<message_id>'.
    pub source: String,
    pub confirmation_status: ConfirmationStatus,
    #[serde(default)]
    pub note: Option<String>,
}

impl Validate for FieldProvenance {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for FieldProvenance {
    const NAME: &'static str = "FieldProvenance";
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceRequirements {
    /// `> 0` when set.
    #[serde(default)]
    pub min_area_sqm: Option<f64>,
    /// `> 0` when set.
    #[serde(default)]
    pub max_area_sqm: Option<f64>,
    /// Room type -> count, e.g. {'treatment': 3}. Insertion order is preserved as in Python.
    #[serde(default)]
    pub required_rooms: IndexMap<String, i64>,
    #[serde(default)]
    pub ground_floor_required: Option<bool>,
}

impl Validate for SpaceRequirements {
    fn check(&self, c: &mut Checker) {
        c.gt_opt("min_area_sqm", self.min_area_sqm, 0.0);
        c.gt_opt("max_area_sqm", self.max_area_sqm, 0.0);
    }
}

impl Contract for SpaceRequirements {
    const NAME: &'static str = "SpaceRequirements";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfrastructureNeeds {
    #[serde(default)]
    pub minimize_plumbing_changes: bool,
    /// `>= 0`, default 1.
    #[serde(default = "one")]
    pub sinks_per_treatment_room: i64,
    /// `> 0` when set.
    #[serde(default)]
    pub min_electrical_service_amps: Option<i64>,
    #[serde(default)]
    pub needs_dedicated_hvac_zoning: Option<bool>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Default for InfrastructureNeeds {
    fn default() -> Self {
        Self {
            minimize_plumbing_changes: false,
            sinks_per_treatment_room: 1,
            min_electrical_service_amps: None,
            needs_dedicated_hvac_zoning: None,
            notes: Vec::new(),
        }
    }
}

impl Validate for InfrastructureNeeds {
    fn check(&self, c: &mut Checker) {
        c.ge("sinks_per_treatment_room", self.sinks_per_treatment_room, 0);
        c.gt_opt(
            "min_electrical_service_amps",
            self.min_electrical_service_amps,
            0,
        );
    }
}

impl Contract for InfrastructureNeeds {
    const NAME: &'static str = "InfrastructureNeeds";
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationalRequirements {
    #[serde(default)]
    pub operating_hours: Option<String>,
    /// `>= 0` when set.
    #[serde(default)]
    pub staff_count: Option<i64>,
    #[serde(default)]
    pub parking_required: Option<bool>,
    #[serde(default = "yes")]
    pub ada_accessible_required: bool,
}

impl Default for OperationalRequirements {
    fn default() -> Self {
        Self {
            operating_hours: None,
            staff_count: None,
            parking_required: None,
            ada_accessible_required: true,
        }
    }
}

impl Validate for OperationalRequirements {
    fn check(&self, c: &mut Checker) {
        c.ge_opt("staff_count", self.staff_count, 0);
    }
}

impl Contract for OperationalRequirements {
    const NAME: &'static str = "OperationalRequirements";
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignPreferences {
    #[serde(default)]
    pub interior_style: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Validate for DesignPreferences {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for DesignPreferences {
    const NAME: &'static str = "DesignPreferences";
}

fn w_total_cost() -> f64 {
    0.5
}
fn w_infrastructure_fit() -> f64 {
    0.3
}
fn w_evidence_certainty() -> f64 {
    0.2
}
fn horizon_36() -> i64 {
    36
}

/// Relative weights. Normalized by the ranking service; must not all be zero (enforced there,
/// as in Python: the contract itself has no rule for it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankingPriorities {
    #[serde(default = "w_total_cost")]
    pub total_cost: f64,
    #[serde(default = "w_infrastructure_fit")]
    pub infrastructure_fit: f64,
    #[serde(default = "w_evidence_certainty")]
    pub evidence_certainty: f64,
    /// `0 < months <= 240`.
    #[serde(default = "horizon_36")]
    pub comparison_horizon_months: i64,
}

impl Default for RankingPriorities {
    fn default() -> Self {
        Self {
            total_cost: 0.5,
            infrastructure_fit: 0.3,
            evidence_certainty: 0.2,
            comparison_horizon_months: 36,
        }
    }
}

impl Validate for RankingPriorities {
    fn check(&self, c: &mut Checker) {
        c.ge("total_cost", self.total_cost, 0.0);
        c.ge("infrastructure_fit", self.infrastructure_fit, 0.0);
        c.ge("evidence_certainty", self.evidence_certainty, 0.0);
        c.gt(
            "comparison_horizon_months",
            self.comparison_horizon_months,
            0,
        );
        c.le(
            "comparison_horizon_months",
            self.comparison_horizon_months,
            240,
        );
    }
}

impl Contract for RankingPriorities {
    const NAME: &'static str = "RankingPriorities";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessExpansionProfile {
    #[serde(default = "new_id")]
    pub profile_id: Uuid,
    pub project_id: Uuid,
    /// e.g. 'facial_studio'; at least one character.
    pub business_type: String,
    #[serde(default)]
    pub business_description: Option<String>,
    /// `>= 0`.
    pub current_location_count: i64,
    /// {'city': 'Los Angeles', 'state': 'CA'}.
    pub target_market: IndexMap<String, String>,
    #[serde(default)]
    pub preferred_neighborhoods: Vec<String>,
    #[serde(default)]
    pub currency: CurrencyCode,
    #[serde(default)]
    pub opening_capital: Option<NonNegativeMinor>,
    #[serde(default)]
    pub monthly_rent_limit: Option<NonNegativeMinor>,
    #[serde(default)]
    pub working_capital_reserve: Option<NonNegativeMinor>,
    pub space_requirements: SpaceRequirements,
    #[serde(default)]
    pub infrastructure_requirements: InfrastructureNeeds,
    #[serde(default)]
    pub operational_requirements: OperationalRequirements,
    #[serde(default)]
    pub target_opening_date: Option<NaiveDate>,
    #[serde(default)]
    pub design_preferences: DesignPreferences,
    #[serde(default)]
    pub ranking_priorities: RankingPriorities,
    #[serde(default)]
    pub confirmation_status: ConfirmationStatus,
    #[serde(default)]
    pub source_references: Vec<FieldProvenance>,
}

impl Validate for BusinessExpansionProfile {
    fn check(&self, c: &mut Checker) {
        c.min_len("business_type", &self.business_type, 1);
        c.ge("current_location_count", self.current_location_count, 0);
        c.ge_opt("opening_capital", self.opening_capital, 0);
        c.ge_opt("monthly_rent_limit", self.monthly_rent_limit, 0);
        c.ge_opt("working_capital_reserve", self.working_capital_reserve, 0);
        c.nested("space_requirements", &self.space_requirements);
        c.nested(
            "infrastructure_requirements",
            &self.infrastructure_requirements,
        );
        c.nested("operational_requirements", &self.operational_requirements);
        c.nested("design_preferences", &self.design_preferences);
        c.nested("ranking_priorities", &self.ranking_priorities);
        c.each("source_references", &self.source_references);
    }
}

impl Contract for BusinessExpansionProfile {
    const NAME: &'static str = "BusinessExpansionProfile";
}
