//! Domain contracts: ports of `apps/api/app/contracts/{evidence,permits,property,infrastructure,
//! finance,geometry,procurement}.py` and `ProviderStatusEntry` from `api.py`. Field order, defaults, enum strings and
//! validators are Python's; see [`ss_core::Contract`] for the validation entry points.

pub mod evidence;
pub mod finance;
pub mod geometry;
pub mod infrastructure;
pub mod permits;
pub mod procurement;
pub mod property;
pub mod provider;
pub mod values;

pub use evidence::SourceEvidence;
pub use finance::{
    CostKind, CostLineItem, CostScenario, HardConstraintResult, PropertyRankingResult,
    RankedProperty, RenovationWorkItem, WorkCategory,
};
pub use geometry::{
    validate_ring, Coord, DesignApprovalStatus, DesignRevisionRequest, DesignVersion,
    FinishSpecification, Fixture, FurniturePlacement, GeometrySource, GeometryValidationReport,
    LayoutGenerationRequest, LayoutPreferences, Opening, RevisionStatus, RoomPolygon,
    RoomRequirement, SiteGeometry, ValidationIssue,
};
pub use infrastructure::{
    AssessmentStatus, InfrastructureAssessment, InfrastructureObservation,
    InfrastructureRequirement, RequirementCategory, ReviewStatus, Severity,
};
pub use permits::{ExtractedInfrastructureFact, PermitCategory, PermitRecord, PermitScope};
pub use procurement::{AvailabilityStatus, BillOfMaterials, BomItem, BomStatus, SupplierOffer};
pub use property::{
    CommercialListing, ExteriorAsset, InteriorAsset, ListingType, PropertyDigitalProfile,
    PropertyIdentity, PropertySearchRequest, SourceManifestEntry, LISTING_FIELDS,
};
pub use provider::ProviderStatusEntry;
pub use values::{PyDecimal, ScalarValue};
