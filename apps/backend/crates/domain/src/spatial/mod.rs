//! Port of `apps/api/app/domains/spatial/`: deterministic layout validation.

pub mod validation;

pub use validation::{min_dimension, validate_layout, EPS};
