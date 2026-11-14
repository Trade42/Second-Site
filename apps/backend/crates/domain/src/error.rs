//! Errors of the domain services.

use ss_core::ValidationError;

use crate::integrations::base::ProviderError;
use crate::permits::extract::ExtractError;
use crate::property::address::AddressError;

/// Why a domain operation failed. Python raises in the same situations; the server maps these
/// to a `ProblemDetail` (validation -> 422, provider -> its error kind, the rest -> 500).
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Address(#[from] AddressError),
    #[error(transparent)]
    Extract(#[from] ExtractError),
    /// A source record lacks a field or has the wrong JSON type where Python would raise
    /// `KeyError` / `TypeError` / `AttributeError`.
    #[error("malformed source record: {0}")]
    BadRecord(String),
    /// Python raises `ValueError` with this message (a work item counted twice in a cost
    /// scenario, ranking priorities that are all zero). Python's API answers these with a 500.
    #[error("{0}")]
    Value(String),
    /// An integer money or count result does not fit in `i64`. Python's integers are unbounded,
    /// so Python would have continued; the Rust port stops instead of wrapping.
    #[error("integer overflow: {0}")]
    Overflow(String),
}
