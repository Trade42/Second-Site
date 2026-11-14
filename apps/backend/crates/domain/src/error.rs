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
}
