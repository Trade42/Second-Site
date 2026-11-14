//! Port of the provider conventions in `apps/api/app/integrations/base.py`: error kinds, the
//! provider error, the retry policy values, HTTP status classification and `status_entry`.
//!
//! Not ported here: `classify_http_error`'s httpx exception mapping and the blocking
//! `with_retries` loop. They belong with the live HTTP providers in `ss-server`;
//! [`kind_for_http_status`] is the pure part of that mapping.

use std::path::{Path, PathBuf};

use ss_core::{string_enum, IntegrationStatus};

use crate::contracts::ProviderStatusEntry;

string_enum! {
    pub enum ProviderErrorKind {
        Auth = "AUTH",
        NotFound = "NOT_FOUND",
        RateLimited = "RATE_LIMITED",
        Timeout = "TIMEOUT",
        Unavailable = "UNAVAILABLE",
        BadResponse = "BAD_RESPONSE",
        NotConfigured = "NOT_CONFIGURED",
    }
}

/// `base.ProviderError`; the message is Python's `str(exc)`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("[{provider}] {kind}: {message}")]
pub struct ProviderError {
    pub provider: String,
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn new(provider: &str, kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            provider: provider.to_owned(),
            kind,
            message: message.into(),
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::RateLimited
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::Unavailable
        )
    }
}

/// `base.RetryPolicy` (values only).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub backoff_s: f64,
    pub timeout_s: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            backoff_s: 0.5,
            timeout_s: 20.0,
        }
    }
}

/// The HTTP-status part of `classify_http_error`: 401/403 AUTH, 404 NOT_FOUND, 429 RATE_LIMITED,
/// other >= 500 UNAVAILABLE, anything else BAD_RESPONSE (message `HTTP {code}`).
pub fn kind_for_http_status(code: u16) -> ProviderErrorKind {
    match code {
        401 | 403 => ProviderErrorKind::Auth,
        404 => ProviderErrorKind::NotFound,
        429 => ProviderErrorKind::RateLimited,
        c if c >= 500 => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::BadResponse,
    }
}

/// `base.status_entry`.
pub fn status_entry(
    provider: &str,
    mode: &str,
    status: IntegrationStatus,
    creds: bool,
    note: &str,
) -> ProviderStatusEntry {
    ProviderStatusEntry {
        provider: provider.to_owned(),
        mode: mode.to_owned(),
        status,
        credentials_present: creds,
        note: note.to_owned(),
    }
}

/// `base.REPO_ROOT`: resolved from this crate's source location, as Python resolves it from its
/// source file (`apps/backend/crates/domain` -> four levels up).
pub fn repo_root() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    here.ancestors().nth(4).unwrap_or(here).to_path_buf()
}

/// `base.FIXTURES_DIR`. Fixture providers also accept an explicit path.
pub fn fixtures_dir() -> PathBuf {
    repo_root().join("fixtures")
}
