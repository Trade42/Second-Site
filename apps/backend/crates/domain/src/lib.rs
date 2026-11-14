//! `ss-domain`: Second Site domain contracts and deterministic engines. No HTTP, no database.
//!
//! Port map (Python under `apps/api/app` -> Rust module):
//! - `contracts/{evidence,permits,property,infrastructure,finance}.py` -> [`contracts`]
//! - `domains/property/{address,identity,service}.py` -> [`property`]
//! - `domains/permits/{extract,service}.py` (pure part) -> [`permits`]
//! - `integrations/{base,costar,la_permits}` (fixture and data-kit providers) -> [`integrations`]
//! - `domains/requirements/templates.py` -> [`requirements`]
//! - Python built-in semantics the port depends on -> [`py`]

pub mod contracts;
pub mod error;
pub mod integrations;
pub mod permits;
pub mod property;
pub mod py;
pub mod requirements;

pub use error::DomainError;
