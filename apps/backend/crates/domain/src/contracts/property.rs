//! Port of `apps/api/app/contracts/property.py`.

use chrono::NaiveDate;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use ss_core::{
    new_id, string_enum, Checker, Contract, CurrencyCode, DataOrigin, MatchStatus,
    NonNegativeMinor, UtcDateTime, Validate,
};
use uuid::Uuid;

use super::infrastructure::{InfrastructureAssessment, InfrastructureObservation};
use super::permits::PermitRecord;

fn unmatched() -> MatchStatus {
    MatchStatus::Unmatched
}

/// Canonical identity. Providers (CoStar, Matterport, Vexcel, LADBS) do NOT share IDs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyIdentity {
    #[serde(default = "new_id")]
    pub property_id: Uuid,
    /// provider -> provider-native id, e.g. {'costar': '...'}.
    #[serde(default)]
    pub provider_property_ids: IndexMap<String, String>,
    /// Canonical uppercase form incl. direction and unit.
    pub normalized_address: String,
    pub street_address: String,
    #[serde(default)]
    pub unit_or_suite: Option<String>,
    pub city: String,
    /// Exactly two characters.
    pub state: String,
    #[serde(default)]
    pub postal_code: Option<String>,
    /// APN when known.
    #[serde(default)]
    pub parcel_id: Option<String>,
    /// [-90, 90] when set.
    #[serde(default)]
    pub latitude: Option<f64>,
    /// [-180, 180] when set.
    #[serde(default)]
    pub longitude: Option<f64>,
    #[serde(default = "unmatched")]
    pub identity_match_status: MatchStatus,
    /// [0, 1] when set.
    #[serde(default)]
    pub identity_match_confidence: Option<f64>,
}

impl Validate for PropertyIdentity {
    fn check(&self, c: &mut Checker) {
        c.min_len("state", &self.state, 2);
        c.max_len("state", &self.state, 2);
        c.ge_opt("latitude", self.latitude, -90.0);
        c.le_opt("latitude", self.latitude, 90.0);
        c.ge_opt("longitude", self.longitude, -180.0);
        c.le_opt("longitude", self.longitude, 180.0);
        c.ge_opt(
            "identity_match_confidence",
            self.identity_match_confidence,
            0.0,
        );
        c.le_opt(
            "identity_match_confidence",
            self.identity_match_confidence,
            1.0,
        );
    }
}

impl Contract for PropertyIdentity {
    const NAME: &'static str = "PropertyIdentity";
}

string_enum! {
    #[derive(Default)]
    pub enum ListingType {
        #[default]
        Lease = "LEASE",
        Sale = "SALE",
    }
}

/// `set(CommercialListing.model_fields)`, in declaration order.
pub const LISTING_FIELDS: &[&str] = &[
    "provider",
    "provider_listing_id",
    "data_origin",
    "listing_type",
    "property_type",
    "available_area_sqm",
    "currency",
    "asking_rent_monthly",
    "nnn_charges_monthly",
    "nnn_is_estimate",
    "security_deposit",
    "lease_term_months",
    "available_from",
    "existing_buildout",
    "lease_service_type",
    "building_name",
    "brokerage_company",
    "broker_contact",
    "photo_urls",
    "source_record_date",
    "listing_url",
    "disclaimer",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommercialListing {
    /// 'costar' only when sourced from CoStar; fixtures say 'fixture'.
    pub provider: String,
    #[serde(default)]
    pub provider_listing_id: Option<String>,
    pub data_origin: DataOrigin,
    #[serde(default)]
    pub listing_type: ListingType,
    #[serde(default)]
    pub property_type: Option<String>,
    /// `> 0` when set.
    #[serde(default)]
    pub available_area_sqm: Option<f64>,
    #[serde(default)]
    pub currency: CurrencyCode,
    #[serde(default)]
    pub asking_rent_monthly: Option<NonNegativeMinor>,
    #[serde(default)]
    pub nnn_charges_monthly: Option<NonNegativeMinor>,
    /// True when nnn_charges_monthly is a DERIVED estimate because the listing quoted none.
    #[serde(default)]
    pub nnn_is_estimate: bool,
    #[serde(default)]
    pub security_deposit: Option<NonNegativeMinor>,
    /// `> 0` when set.
    #[serde(default)]
    pub lease_term_months: Option<i64>,
    #[serde(default)]
    pub available_from: Option<NaiveDate>,
    /// 'open_shell' | 'office_buildout' | 'second_generation_salon' | null (unknown).
    #[serde(default)]
    pub existing_buildout: Option<String>,
    /// Which operating expenses the rent includes, e.g. 'TripleNet', 'FullService'.
    #[serde(default)]
    pub lease_service_type: Option<String>,
    #[serde(default)]
    pub building_name: Option<String>,
    #[serde(default)]
    pub brokerage_company: Option<String>,
    /// {'name','company','phone'} of the listing contact. CoStar kit phones are anonymized (555).
    #[serde(default)]
    pub broker_contact: Option<IndexMap<String, String>>,
    /// Provider CDN photo links (not stored).
    #[serde(default)]
    pub photo_urls: Vec<String>,
    /// When the provider last certified the record (snapshot freshness).
    #[serde(default)]
    pub source_record_date: Option<NaiveDate>,
    #[serde(default)]
    pub listing_url: Option<String>,
    #[serde(default)]
    pub disclaimer: Option<String>,
}

impl Validate for CommercialListing {
    fn check(&self, c: &mut Checker) {
        c.gt_opt("available_area_sqm", self.available_area_sqm, 0.0);
        c.ge_opt("asking_rent_monthly", self.asking_rent_monthly, 0);
        c.ge_opt("nnn_charges_monthly", self.nnn_charges_monthly, 0);
        c.ge_opt("security_deposit", self.security_deposit, 0);
        c.gt_opt("lease_term_months", self.lease_term_months, 0);
    }
}

impl Contract for CommercialListing {
    const NAME: &'static str = "CommercialListing";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteriorAsset {
    /// 'matterport' or fixture.
    pub provider: String,
    pub asset_id: String,
    /// e.g. 'matterport_model', 'floor_plan_image', 'photo'.
    pub asset_type: String,
    pub data_origin: DataOrigin,
    #[serde(default)]
    pub url: Option<String>,
    /// Provider title, e.g. the Matterport model name.
    #[serde(default)]
    pub label: Option<String>,
    /// Provider-measured floor area, if any; `> 0` when set.
    #[serde(default)]
    pub area_sqm: Option<f64>,
    /// True when the asset is matched to the listed suite (not just the building).
    #[serde(default)]
    pub matches_listing_unit: Option<bool>,
    #[serde(default)]
    pub has_measured_geometry: bool,
}

impl Validate for InteriorAsset {
    fn check(&self, c: &mut Checker) {
        c.gt_opt("area_sqm", self.area_sqm, 0.0);
    }
}

impl Contract for InteriorAsset {
    const NAME: &'static str = "InteriorAsset";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExteriorAsset {
    /// 'vexcel' or fixture.
    pub provider: String,
    pub asset_id: String,
    /// e.g. 'oblique', 'orthomosaic', 'roof_analysis'.
    pub asset_type: String,
    pub data_origin: DataOrigin,
    #[serde(default)]
    pub captured_at: Option<NaiveDate>,
    #[serde(default)]
    pub url: Option<String>,
}

impl Validate for ExteriorAsset {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for ExteriorAsset {
    const NAME: &'static str = "ExteriorAsset";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifestEntry {
    pub provider: String,
    #[serde(default)]
    pub dataset: Option<String>,
    /// LIVE | SNAPSHOT | MOCK | PARTIAL | BLOCKED | NOT_STARTED | NOT_FOUND.
    pub status: String,
    #[serde(default)]
    pub record_count: i64,
    #[serde(default)]
    pub retrieved_at: Option<UtcDateTime>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Validate for SourceManifestEntry {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for SourceManifestEntry {
    const NAME: &'static str = "SourceManifestEntry";
}

/// Primary unified property intelligence object. Raw provider payloads are NOT embedded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyDigitalProfile {
    pub property_id: Uuid,
    pub project_id: Uuid,
    pub property_identity: PropertyIdentity,
    #[serde(default)]
    pub commercial_listing: Option<CommercialListing>,
    #[serde(default)]
    pub interior_assets: Vec<InteriorAsset>,
    #[serde(default)]
    pub exterior_assets: Vec<ExteriorAsset>,
    #[serde(default)]
    pub permit_records: Vec<PermitRecord>,
    #[serde(default)]
    pub building_observations: Vec<InfrastructureObservation>,
    /// SiteGeometry id if available.
    #[serde(default)]
    pub geometry_reference: Option<Uuid>,
    #[serde(default)]
    pub infrastructure_assessments: Vec<InfrastructureAssessment>,
    #[serde(default)]
    pub missing_information: Vec<String>,
    #[serde(default)]
    pub source_manifest: Vec<SourceManifestEntry>,
    #[serde(default)]
    pub last_analyzed_at: Option<UtcDateTime>,
}

impl Validate for PropertyDigitalProfile {
    fn check(&self, c: &mut Checker) {
        c.nested("property_identity", &self.property_identity);
        c.nested_opt("commercial_listing", &self.commercial_listing);
        c.each("interior_assets", &self.interior_assets);
        c.each("exterior_assets", &self.exterior_assets);
        c.each("permit_records", &self.permit_records);
        c.each("building_observations", &self.building_observations);
        c.each(
            "infrastructure_assessments",
            &self.infrastructure_assessments,
        );
        c.each("source_manifest", &self.source_manifest);
    }
}

impl Contract for PropertyDigitalProfile {
    const NAME: &'static str = "PropertyDigitalProfile";
}

fn los_angeles() -> String {
    "Los Angeles".to_owned()
}
fn ca() -> String {
    "CA".to_owned()
}
fn ten() -> i64 {
    10
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertySearchRequest {
    #[serde(default = "los_angeles")]
    pub city: String,
    #[serde(default = "ca")]
    pub state: String,
    #[serde(default)]
    pub max_monthly_rent: Option<NonNegativeMinor>,
    /// `> 0` when set.
    #[serde(default)]
    pub min_area_sqm: Option<f64>,
    #[serde(default)]
    pub neighborhoods: Vec<String>,
    /// 1..=50.
    #[serde(default = "ten")]
    pub limit: i64,
}

impl Default for PropertySearchRequest {
    fn default() -> Self {
        Self {
            city: los_angeles(),
            state: ca(),
            max_monthly_rent: None,
            min_area_sqm: None,
            neighborhoods: Vec::new(),
            limit: 10,
        }
    }
}

impl Validate for PropertySearchRequest {
    fn check(&self, c: &mut Checker) {
        c.ge_opt("max_monthly_rent", self.max_monthly_rent, 0);
        c.gt_opt("min_area_sqm", self.min_area_sqm, 0.0);
        c.ge("limit", self.limit, 1);
        c.le("limit", self.limit, 50);
    }
}

impl Contract for PropertySearchRequest {
    const NAME: &'static str = "PropertySearchRequest";
}
