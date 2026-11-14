//! Errors. Provider failures use the shared [`ProviderError`] (kinds mapped as every other
//! provider: 401/403 AUTH, 404 NOT_FOUND, 429 RATE_LIMITED, other >= 500 UNAVAILABLE, anything
//! else BAD_RESPONSE; timeout TIMEOUT, transport UNAVAILABLE), plus the RFC 9457 problem Nessie
//! sent, whose `type` slug (`insufficient_funds`, `credit_limit_exceeded`, ...) is the stable
//! value to switch on. No message ever contains the API key or a request URL.

use serde::Serialize;
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};
use thiserror::Error;

use crate::approval::AuthorizationError;
use crate::money::MoneyError;

/// The provider name used in errors and in `/ready`.
pub const PROVIDER_NAME: &str = "nessie";

/// What Nessie's problem+json body said, cut to safe lengths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProblemSummary {
    pub http_status: u16,
    /// Last path segment of the problem `type`, e.g. `insufficient_funds`.
    pub slug: String,
    pub title: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum NessieError {
    /// The Nessie API (or the fixture bank, imitating it) refused or failed.
    #[error("{error}")]
    Provider {
        error: ProviderError,
        /// The HTTP status Nessie answered with, when it answered.
        http_status: Option<u16>,
        /// Boxed to keep the error small.
        problem: Option<Box<ProblemSummary>>,
    },
    /// A request failed the documented limits; nothing was sent.
    #[error("[nessie] invalid request: {0}")]
    InvalidRequest(String),
    /// The approval presented does not authorize this simulated payment; nothing was sent.
    #[error("[nessie] simulated payment not authorized: {0}")]
    NotAuthorized(#[from] AuthorizationError),
    #[error("[nessie] money conversion: {0}")]
    Money(#[from] MoneyError),
}

impl NessieError {
    pub fn provider(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        NessieError::Provider {
            error: ProviderError::new(PROVIDER_NAME, kind, message),
            http_status: None,
            problem: None,
        }
    }

    /// The HTTP status of a provider failure, when the bank answered.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            NessieError::Provider { http_status, .. } => *http_status,
            _ => None,
        }
    }

    /// True when the bank answered with a 4xx: the request was refused and, for a create,
    /// nothing was created (Nessie checks funds "before the transaction exists").
    pub fn is_client_refusal(&self) -> bool {
        self.http_status().is_some_and(|s| (400..500).contains(&s))
    }

    /// The provider error kind, when the failure came from the bank.
    pub fn kind(&self) -> Option<ProviderErrorKind> {
        match self {
            NessieError::Provider { error, .. } => Some(error.kind),
            _ => None,
        }
    }

    /// Nessie's problem slug, e.g. `insufficient_funds`.
    pub fn problem_slug(&self) -> Option<&str> {
        match self {
            NessieError::Provider {
                problem: Some(p), ..
            } => Some(p.slug.as_str()),
            _ => None,
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, NessieError::Provider { error, .. } if error.retryable())
    }
}

/// Cuts text to `max` characters (on a character boundary), marking the cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
