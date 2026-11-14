//! `ss-core`: shared primitives and the project / business-profile contracts of the Second Site
//! backend. A faithful port of `apps/api/app/contracts/common.py`, `state.py` and `project.py`.
//!
//! Every contract type:
//! - serializes field for field like the Pydantic model's `model_dump(mode="json")`;
//! - rejects unknown fields on input (`extra="forbid"`);
//! - implements [`Validate`]/[`Contract`]: `T::from_json_value` / `T::from_json_str` deserialize
//!   and then apply every Pydantic field constraint and model validator, returning a
//!   [`ValidationError`] (never panicking). Values built in code can be checked with
//!   [`Contract::validate`].

pub mod common;
pub mod enums;
pub mod project;
pub mod state;
pub mod validation;

pub use common::{
    new_id, utcnow, CurrencyCode, DataOrigin, EvidenceClassification, IntegrationStatus,
    MatchStatus, Measurement, MinorUnits, Money, NonNegativeMinor, Point2D, SourceReference,
    UtcDateTime, VerificationStatus, CURRENCY_MESSAGE, NAIVE_TIMESTAMP_MESSAGE, SCHEMA_VERSION,
};
pub use enums::UnknownEnumValue;
pub use project::{
    BusinessExpansionProfile, ConfirmationStatus, DesignPreferences, FieldProvenance,
    InfrastructureNeeds, Market, OperationalRequirements, ProjectContext, ProjectStatus,
    RankingPriorities, SpaceRequirements,
};
pub use state::{check_transition, InvalidStateTransition, TransitionTable};
pub use validation::{Checker, Contract, ErrorItem, LocItem, Validate, ValidationError};

// Re-exported so dependants use the same versions as the contracts.
pub use chrono;
pub use indexmap;
pub use serde_json;
pub use uuid;
