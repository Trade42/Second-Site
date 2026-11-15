//! Port of `apps/api/app/domains/procurement/`.

pub mod bom;

pub use bom::{furnish_kind, generate_bom, generate_bom_for, FurnishKind};
