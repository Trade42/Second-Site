//! `ss-domain`: Second Site domain contracts and deterministic engines. No HTTP, no database.
//!
//! Port map (Python under `apps/api/app` -> Rust module):
//! - `contracts/{evidence,permits,property,infrastructure,finance}.py` -> [`contracts`]
//! - `domains/property/{address,identity,service}.py` -> [`property`]
//! - `domains/permits/{extract,service}.py` (pure part) -> [`permits`]
//! - `integrations/{base,costar,la_permits}` (fixture and data-kit providers) -> [`integrations`]
//! - `domains/requirements/templates.py` -> [`requirements`]
//! - `domains/feasibility/engine.py` -> [`feasibility`]
//! - `domains/finance/{calculator,cost_catalog}.py` -> [`finance`]
//! - `domains/ranking/engine.py` -> [`ranking`]
//! - `domains/evidence/validator.py` -> [`evidence`]
//! - `contracts/{geometry,procurement}.py` -> [`contracts::geometry`], [`contracts::procurement`]
//! - `domains/spatial/validation.py` -> [`spatial`]
//! - `domains/design/geometry/{framing,solver}.py`, `domains/design/interior/finishes.py` ->
//!   [`design`] (layout solver: own exact search over CP-SAT's integer model)
//! - `domains/procurement/bom.py` -> [`procurement`]; `domains/logistics/timeline.py` ->
//!   [`logistics`]
//! - `integrations/{catalog,ifc}` (fixture catalog; IFC STEP-text reader only) ->
//!   [`integrations::catalog`], [`integrations::ifc`]
//! - The Shapely/GEOS operations the layout code uses -> [`geometry`]
//! - Python built-in semantics the port depends on -> [`py`]

pub mod contracts;
pub mod design;
pub mod error;
pub mod evidence;
pub mod feasibility;
pub mod finance;
pub mod geometry;
pub mod integrations;
pub mod logistics;
pub mod permits;
pub mod procurement;
pub mod property;
pub mod py;
pub mod ranking;
pub mod requirements;
pub mod spatial;

pub use error::DomainError;
