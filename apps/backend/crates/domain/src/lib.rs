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
//! - Python built-in semantics the port depends on -> [`py`]

pub mod contracts;
pub mod error;
pub mod evidence;
pub mod feasibility;
pub mod finance;
pub mod geometry;
pub mod integrations;
pub mod permits;
pub mod property;
pub mod py;
pub mod ranking;
pub mod requirements;
pub mod spatial;

pub use error::DomainError;
