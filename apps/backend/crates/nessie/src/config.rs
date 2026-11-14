//! Configuration: environment variable names, the provider mode and the redacted API key.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `NESSIE_PROVIDER_MODE` | `live`, `fixture` or `disabled` (the `*_PROVIDER_MODE` convention of `apps/api/app/core/config.py`) | `fixture` |
//! | `NESSIE_API_KEY` | the Nessie v2 key (`nsk_...`), sent only as `Authorization: Bearer` | unset |
//! | `NESSIE_BASE_URL` | the API host ("the base URL for your deployment") | [`DEFAULT_BASE_URL`] |
//!
//! Only `live` with a non-empty key selects the live client; `fixture`, `disabled`, or `live`
//! without a key select the fixture bank (see [`crate::select_bank`]). Names are read
//! case-sensitively from the process environment; the server may instead pass its own settings
//! through [`NessieConfig::from_lookup`].

use std::fmt;

pub const ENV_MODE: &str = "NESSIE_PROVIDER_MODE";
pub const ENV_API_KEY: &str = "NESSIE_API_KEY";
pub const ENV_BASE_URL: &str = "NESSIE_BASE_URL";

/// The hosted Nessie v2 API, from the API reference's quick start (`NESSIE_URL`).
pub const DEFAULT_BASE_URL: &str = "https://v2-prod-api.nessieisreal.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderMode {
    Live,
    Fixture,
    Disabled,
}

impl ProviderMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            ProviderMode::Live => "live",
            ProviderMode::Fixture => "fixture",
            ProviderMode::Disabled => "disabled",
        }
    }

    /// Python's `ProviderMode` values, case-insensitive like pydantic-settings' enum parsing of
    /// lower-case values. Unknown values are an error, not a silent fallback.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "live" => Ok(ProviderMode::Live),
            "fixture" => Ok(ProviderMode::Fixture),
            "disabled" => Ok(ProviderMode::Disabled),
            other => Err(format!(
                "{ENV_MODE} must be live, fixture or disabled, not {other:?}"
            )),
        }
    }
}

/// The Nessie API key. It is never printed: `Debug` and `Display` show `ApiKey(***)`, and it is
/// not `Serialize`. The only way to read it is [`ApiKey::expose`], used when building the
/// `Authorization` header.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// `None` for an empty or blank value (an unset key).
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let v = value.into();
        let v = v.trim();
        if v.is_empty() {
            None
        } else {
            Some(Self(v.to_owned()))
        }
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NessieConfig {
    pub mode: ProviderMode,
    pub api_key: Option<ApiKey>,
    /// Without a trailing slash.
    pub base_url: String,
    /// Connect, read and write timeout of the live client, each.
    pub timeout_s: f64,
}

impl Default for NessieConfig {
    fn default() -> Self {
        Self {
            mode: ProviderMode::Fixture,
            api_key: None,
            base_url: DEFAULT_BASE_URL.to_owned(),
            timeout_s: 20.0,
        }
    }
}

impl NessieConfig {
    /// Reads `NESSIE_PROVIDER_MODE`, `NESSIE_API_KEY` and `NESSIE_BASE_URL`.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The same, from any lookup (the server's settings, or a test map).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mode = match lookup(ENV_MODE) {
            Some(v) if !v.trim().is_empty() => ProviderMode::parse(&v)?,
            _ => ProviderMode::Fixture,
        };
        let api_key = lookup(ENV_API_KEY).and_then(ApiKey::new);
        let base_url = match lookup(ENV_BASE_URL) {
            Some(v) if !v.trim().is_empty() => normalize_base_url(&v)?,
            _ => DEFAULT_BASE_URL.to_owned(),
        };
        Ok(Self {
            mode,
            api_key,
            base_url,
            ..Self::default()
        })
    }

    /// True when the live client would be selected.
    pub fn live_selected(&self) -> bool {
        self.mode == ProviderMode::Live && self.api_key.is_some()
    }
}

/// Trims a trailing slash and requires an `http://` or `https://` URL without a query or
/// fragment (the key never goes in a URL, and nothing else should either).
pub fn normalize_base_url(raw: &str) -> Result<String, String> {
    let v = raw.trim().trim_end_matches('/');
    let lower = v.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    match rest {
        Some(host) if !host.is_empty() && !v.contains('?') && !v.contains('#') => Ok(v.to_owned()),
        _ => Err(format!(
            "{ENV_BASE_URL} must be an http(s) URL without a query string"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<NessieConfig, String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        NessieConfig::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn defaults_to_fixture_without_key() {
        let c = cfg(&[]).unwrap();
        assert_eq!(c.mode, ProviderMode::Fixture);
        assert!(c.api_key.is_none());
        assert_eq!(c.base_url, DEFAULT_BASE_URL);
        assert!(!c.live_selected());
    }

    #[test]
    fn live_needs_mode_and_key() {
        assert!(!cfg(&[(ENV_API_KEY, "nsk_test")]).unwrap().live_selected());
        assert!(!cfg(&[(ENV_MODE, "live")]).unwrap().live_selected());
        assert!(!cfg(&[(ENV_MODE, "live"), (ENV_API_KEY, "   ")])
            .unwrap()
            .live_selected());
        assert!(cfg(&[(ENV_MODE, "LIVE"), (ENV_API_KEY, "nsk_test")])
            .unwrap()
            .live_selected());
        assert!(cfg(&[(ENV_MODE, "sandbox")]).is_err());
    }

    #[test]
    fn base_url_is_normalized_and_checked() {
        let c = cfg(&[(ENV_BASE_URL, "http://127.0.0.1:9999/")]).unwrap();
        assert_eq!(c.base_url, "http://127.0.0.1:9999");
        assert!(cfg(&[(ENV_BASE_URL, "ftp://x")]).is_err());
        assert!(cfg(&[(ENV_BASE_URL, "https://x/?key=abc")]).is_err());
        assert!(cfg(&[(ENV_BASE_URL, "https://")]).is_err());
    }

    #[test]
    fn key_is_never_printed() {
        let c = cfg(&[(ENV_MODE, "live"), (ENV_API_KEY, "nsk_SECRET_VALUE")]).unwrap();
        let shown = format!("{c:?} {}", c.api_key.as_ref().unwrap());
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("ApiKey(***)"));
        assert_eq!(c.api_key.unwrap().expose(), "nsk_SECRET_VALUE");
    }
}
