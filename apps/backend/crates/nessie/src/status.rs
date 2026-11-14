//! The `/ready` entry for provider `nessie`, in the style of the other providers
//! (`ProviderStatusEntry { provider, mode, status, credentials_present, note }`).
//!
//! `IntegrationStatus` has no `NOT_CONFIGURED` value (LIVE, MOCK, PARTIAL, BLOCKED,
//! NOT_STARTED), so the finer [`Readiness`] is reported as the first word of `note` and mapped
//! to the nearest existing status. `LIVE` is reported only after an authenticated call
//! succeeded, never on the strength of configuration alone.
//!
//! | Readiness | When | status |
//! |---|---|---|
//! | `FIXTURE` | mode `fixture` or `disabled` | MOCK |
//! | `NOT_CONFIGURED` | mode `live`, no `NESSIE_API_KEY` (the fixture bank is used) | BLOCKED |
//! | `CONFIGURED_UNVERIFIED` | live client, no authenticated call has succeeded yet | PARTIAL |
//! | `VERIFIED` | the last authenticated call succeeded | LIVE |
//! | `KEY_REJECTED` | the last call got 401 or 403 | BLOCKED |
//! | `LAST_CALL_FAILED` | the last call timed out, failed in transport or got a 5xx | PARTIAL |

use serde::Serialize;
use ss_core::IntegrationStatus;
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::status_entry;

use crate::config::ProviderMode;

/// The provider name in `/ready` and in errors.
pub const PROVIDER: &str = crate::error::PROVIDER_NAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Readiness {
    #[serde(rename = "FIXTURE")]
    Fixture,
    #[serde(rename = "NOT_CONFIGURED")]
    NotConfigured,
    #[serde(rename = "CONFIGURED_UNVERIFIED")]
    ConfiguredUnverified,
    #[serde(rename = "VERIFIED")]
    Verified,
    #[serde(rename = "KEY_REJECTED")]
    KeyRejected,
    #[serde(rename = "LAST_CALL_FAILED")]
    LastCallFailed,
}

impl Readiness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Readiness::Fixture => "FIXTURE",
            Readiness::NotConfigured => "NOT_CONFIGURED",
            Readiness::ConfiguredUnverified => "CONFIGURED_UNVERIFIED",
            Readiness::Verified => "VERIFIED",
            Readiness::KeyRejected => "KEY_REJECTED",
            Readiness::LastCallFailed => "LAST_CALL_FAILED",
        }
    }

    pub const fn integration_status(self) -> IntegrationStatus {
        match self {
            Readiness::Fixture => IntegrationStatus::Mock,
            Readiness::NotConfigured | Readiness::KeyRejected => IntegrationStatus::Blocked,
            Readiness::ConfiguredUnverified | Readiness::LastCallFailed => {
                IntegrationStatus::Partial
            }
            Readiness::Verified => IntegrationStatus::Live,
        }
    }
}

/// Builds the entry. `detail` (optional) is appended to the note, e.g. the time of the last
/// successful call or the failure kind; it must never contain a credential.
pub fn nessie_status(
    mode: ProviderMode,
    credentials_present: bool,
    readiness: Readiness,
    detail: Option<&str>,
) -> ProviderStatusEntry {
    let base = match readiness {
        Readiness::Fixture => {
            "in-memory fixture bank: fixture data, not the Capital One Nessie sandbox and not a \
             bank. Set NESSIE_PROVIDER_MODE=live with NESSIE_API_KEY to use the sandbox."
        }
        Readiness::NotConfigured => {
            "NESSIE_API_KEY is not set, so the in-memory fixture bank (fixture data, not the \
             Capital One Nessie sandbox) is used."
        }
        Readiness::ConfiguredUnverified => {
            "Capital One Nessie v2 sandbox (mock money): key configured, not yet verified by a \
             successful authenticated call."
        }
        Readiness::Verified => {
            "Capital One Nessie v2 sandbox (mock money): the last authenticated call succeeded."
        }
        Readiness::KeyRejected => {
            "Capital One Nessie v2 sandbox (mock money): the key was rejected (HTTP 401/403)."
        }
        Readiness::LastCallFailed => {
            "Capital One Nessie v2 sandbox (mock money): the last call failed."
        }
    };
    let note = match detail {
        Some(d) if !d.is_empty() => format!("{}: {base} {d}", readiness.as_str()),
        _ => format!("{}: {base}", readiness.as_str()),
    };
    status_entry(
        PROVIDER,
        mode.as_str(),
        readiness.integration_status(),
        credentials_present,
        &note,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_only_when_verified() {
        for r in [
            Readiness::Fixture,
            Readiness::NotConfigured,
            Readiness::ConfiguredUnverified,
            Readiness::KeyRejected,
            Readiness::LastCallFailed,
        ] {
            assert_ne!(r.integration_status(), IntegrationStatus::Live, "{r:?}");
        }
        assert_eq!(
            Readiness::Verified.integration_status(),
            IntegrationStatus::Live
        );
    }

    #[test]
    fn not_configured_entry() {
        let e = nessie_status(ProviderMode::Live, false, Readiness::NotConfigured, None);
        assert_eq!(e.provider, "nessie");
        assert_eq!(e.mode, "live");
        assert_eq!(e.status, IntegrationStatus::Blocked);
        assert!(!e.credentials_present);
        assert!(e.note.starts_with("NOT_CONFIGURED: "), "{}", e.note);
        assert!(e.note.contains("fixture data"));
    }

    #[test]
    fn every_note_names_mock_money_or_fixture_data() {
        for r in [
            Readiness::Fixture,
            Readiness::NotConfigured,
            Readiness::ConfiguredUnverified,
            Readiness::Verified,
            Readiness::KeyRejected,
            Readiness::LastCallFailed,
        ] {
            let e = nessie_status(ProviderMode::Live, true, r, Some("detail"));
            assert!(
                e.note.contains("mock money") || e.note.contains("fixture data"),
                "{}",
                e.note
            );
            assert!(e.note.starts_with(r.as_str()));
            assert!(e.note.ends_with(" detail"));
        }
    }
}
