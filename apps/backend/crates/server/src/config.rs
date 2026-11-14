//! Port of `apps/api/app/core/config.py`: validated settings read from the same environment
//! variable names, with the same defaults. Secrets are wrapped in [`Secret`] and never printed.
//!
//! Sources, highest priority first (as pydantic-settings): process environment, `../../.env`,
//! `.env` (both relative to the working directory). Names are matched case-insensitively. An empty
//! value is a value (pydantic-settings does not ignore empty variables).
//!
//! Rust-only additions: `STORE_BACKEND` (`postgres` default, or `memory`), `HOST` and `PORT`
//! (what uvicorn took on its command line).

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

/// `config.ProviderMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderMode {
    Live,
    Fixture,
    Disabled,
}

impl ProviderMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderMode::Live => "live",
            ProviderMode::Fixture => "fixture",
            ProviderMode::Disabled => "disabled",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "live" => Some(ProviderMode::Live),
            "fixture" => Some(ProviderMode::Fixture),
            "disabled" => Some(ProviderMode::Disabled),
            _ => None,
        }
    }
}

impl fmt::Display for ProviderMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A secret value (`pydantic.SecretStr`). `Debug` and `Display` never show the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(v: impl Into<String>) -> Self {
        Self(v.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(**********)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("**********")
    }
}

/// Which `Store` implementation to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreBackend {
    Postgres,
    Memory,
}

pub const DEV_AUTH_SECRET: &str = "dev-only-change-me";

#[derive(Debug, thiserror::Error)]
#[error("invalid configuration: {0}")]
pub struct ConfigError(pub String);

/// Every field of `config.Settings`, plus the Rust-only additions at the end.
#[derive(Debug, Clone)]
pub struct Settings {
    // APPLICATION
    pub app_env: String,
    pub app_version: String,
    pub log_level: String,
    pub cors_origins: String,
    pub public_base_url: String,
    pub public_web_url: String,
    pub task_execution_mode: String,
    // DATABASE / REDIS
    pub database_url: String,
    pub redis_url: String,
    // AUTHENTICATION
    pub auth_secret: Secret,
    pub allow_dev_tokens: bool,
    // MODEL PROVIDER
    pub supervisor_model: Option<String>,
    pub anthropic_api_key: Option<Secret>,
    pub agent_checkpointer: String,
    // PROVIDER MODES
    pub property_provider_mode: ProviderMode,
    pub permit_provider_mode: ProviderMode,
    pub voice_provider_mode: ProviderMode,
    pub messaging_provider_mode: ProviderMode,
    pub procurement_provider_mode: ProviderMode,
    // COSTAR
    pub costar_kit_dir: String,
    // MATTERPORT
    pub matterport_provider_mode: ProviderMode,
    pub matterport_api_key: Option<Secret>,
    pub matterport_api_token: Option<Secret>,
    pub matterport_sdk_key: Option<Secret>,
    pub matterport_graph_url: String,
    // VEXCEL
    pub vexcel_provider_mode: ProviderMode,
    pub vexcel_api_key: Option<Secret>,
    pub vexcel_base_url: String,
    // LA OPEN DATA
    pub la_open_data_base_url: String,
    pub la_open_data_app_token: Option<Secret>,
    pub la_electrical_dataset: String,
    pub la_mechanical_dataset: String,
    // PHOTON
    pub photon_api_key: Option<Secret>,
    pub photon_project_id: Option<String>,
    pub photon_webhook_secret: Option<Secret>,
    pub photon_agent_handle: Option<String>,
    // TELNYX
    pub telnyx_api_key: Option<Secret>,
    pub telnyx_public_key: Option<String>,
    pub telnyx_connection_id: Option<String>,
    pub telnyx_from_number: Option<String>,
    // XAI VOICE
    pub xai_api_key: Option<Secret>,
    // OBJECT STORAGE
    pub object_storage_mode: String,
    pub object_storage_local_dir: String,
    // SAFETY
    pub allow_live_outbound_calls: bool,
    pub allow_live_purchases: bool,
    // RUST-ONLY ADDITIONS
    pub store_backend: StoreBackend,
    pub host: String,
    pub port: u16,
}

/// The repository root as Python computes it (`Path(__file__).parents[4]`): resolved from this
/// crate's source location at build time.
pub fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..")
}

struct Source {
    vars: HashMap<String, String>,
}

impl Source {
    fn get(&self, name: &str) -> Option<&str> {
        self.vars
            .get(&name.to_ascii_uppercase())
            .map(String::as_str)
    }

    fn string(&self, name: &str, default: &str) -> String {
        self.get(name).unwrap_or(default).to_owned()
    }

    fn opt(&self, name: &str) -> Option<String> {
        self.get(name).map(str::to_owned)
    }

    fn secret(&self, name: &str) -> Option<Secret> {
        self.get(name).map(Secret::new)
    }

    fn boolean(&self, name: &str, default: bool) -> Result<bool, ConfigError> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => parse_bool(v)
                .ok_or_else(|| ConfigError(format!("{name}: input should be a valid boolean"))),
        }
    }

    fn mode(&self, name: &str) -> Result<ProviderMode, ConfigError> {
        match self.get(name) {
            None => Ok(ProviderMode::Fixture),
            Some(v) => ProviderMode::parse(v).ok_or_else(|| {
                ConfigError(format!(
                    "{name}: input should be 'live', 'fixture' or 'disabled'"
                ))
            }),
        }
    }
}

/// Pydantic's lax boolean parsing of strings.
fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Minimal dotenv parser (`KEY=VALUE`, `export KEY=VALUE`, `#` comments, optional quotes).
fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            continue;
        }
        let mut val = v.trim().to_owned();
        let quoted = val.len() >= 2
            && ((val.starts_with('"') && val.ends_with('"'))
                || (val.starts_with('\'') && val.ends_with('\'')));
        if quoted {
            val = val[1..val.len() - 1].to_owned();
        } else if let Some(i) = val.find(" #") {
            val = val[..i].trim_end().to_owned();
        }
        out.push((key.to_owned(), val));
    }
    out
}

impl Settings {
    /// `get_settings()`: environment plus `.env` files relative to the working directory.
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut vars: HashMap<String, String> = HashMap::new();
        // Lowest priority first; later sources overwrite.
        for file in [".env", "../../.env"] {
            if let Ok(text) = std::fs::read_to_string(file) {
                for (k, v) in parse_dotenv(&text) {
                    vars.insert(k.to_ascii_uppercase(), v);
                }
            }
        }
        for (k, v) in std::env::vars() {
            vars.insert(k.to_ascii_uppercase(), v);
        }
        Self::from_map(vars)
    }

    /// Build from explicit variables only (tests). Names are case-insensitive.
    pub fn from_vars<I, K, V>(vars: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        Self::from_map(
            vars.into_iter()
                .map(|(k, v)| (k.as_ref().to_ascii_uppercase(), v.into()))
                .collect(),
        )
    }

    fn from_map(vars: HashMap<String, String>) -> Result<Self, ConfigError> {
        let s = Source { vars };
        let default_kit = repo_root().join(".data").join("costar-kit");
        let store_backend = match s.get("STORE_BACKEND").map(str::trim) {
            None | Some("") | Some("postgres") | Some("postgresql") => StoreBackend::Postgres,
            Some("memory") => StoreBackend::Memory,
            Some(other) => {
                return Err(ConfigError(format!(
                    "STORE_BACKEND: expected 'postgres' or 'memory', got {other:?}"
                )))
            }
        };
        let port = match s.get("PORT") {
            None => 8000,
            Some(p) => p
                .trim()
                .parse::<u16>()
                .map_err(|_| ConfigError("PORT: expected an integer 0-65535".into()))?,
        };
        Ok(Settings {
            app_env: s.string("APP_ENV", "development"),
            app_version: s.string("APP_VERSION", "0.1.0"),
            log_level: s.string("LOG_LEVEL", "INFO"),
            cors_origins: s.string("CORS_ORIGINS", "http://localhost:3000"),
            public_base_url: s.string("PUBLIC_BASE_URL", "http://localhost:8000"),
            public_web_url: s.string("PUBLIC_WEB_URL", "http://localhost:3000"),
            task_execution_mode: s.string("TASK_EXECUTION_MODE", "inline"),
            database_url: s.string(
                "DATABASE_URL",
                "postgresql+psycopg://secondsite:secondsite@localhost:5432/secondsite",
            ),
            redis_url: s.string("REDIS_URL", "redis://localhost:6379/0"),
            auth_secret: Secret::new(s.string("AUTH_SECRET", DEV_AUTH_SECRET)),
            allow_dev_tokens: s.boolean("ALLOW_DEV_TOKENS", true)?,
            supervisor_model: s.opt("SUPERVISOR_MODEL"),
            anthropic_api_key: s.secret("ANTHROPIC_API_KEY"),
            agent_checkpointer: s.string("AGENT_CHECKPOINTER", "memory"),
            property_provider_mode: s.mode("PROPERTY_PROVIDER_MODE")?,
            permit_provider_mode: s.mode("PERMIT_PROVIDER_MODE")?,
            voice_provider_mode: s.mode("VOICE_PROVIDER_MODE")?,
            messaging_provider_mode: s.mode("MESSAGING_PROVIDER_MODE")?,
            procurement_provider_mode: s.mode("PROCUREMENT_PROVIDER_MODE")?,
            costar_kit_dir: s.string("COSTAR_KIT_DIR", &default_kit.to_string_lossy()),
            matterport_provider_mode: s.mode("MATTERPORT_PROVIDER_MODE")?,
            matterport_api_key: s.secret("MATTERPORT_API_KEY"),
            matterport_api_token: s.secret("MATTERPORT_API_TOKEN"),
            matterport_sdk_key: s.secret("MATTERPORT_SDK_KEY"),
            matterport_graph_url: s.string(
                "MATTERPORT_GRAPH_URL",
                "https://api.matterport.com/api/models/graph",
            ),
            vexcel_provider_mode: s.mode("VEXCEL_PROVIDER_MODE")?,
            vexcel_api_key: s.secret("VEXCEL_API_KEY"),
            vexcel_base_url: s.string("VEXCEL_BASE_URL", "https://api.vexcelgroup.com/v2"),
            la_open_data_base_url: s.string("LA_OPEN_DATA_BASE_URL", "https://data.lacity.org"),
            la_open_data_app_token: s.secret("LA_OPEN_DATA_APP_TOKEN"),
            la_electrical_dataset: s.string("LA_ELECTRICAL_DATASET", "ysqd-apz7"),
            la_mechanical_dataset: s.string("LA_MECHANICAL_DATASET", "67is-svtd"),
            photon_api_key: s.secret("PHOTON_API_KEY"),
            photon_project_id: s.opt("PHOTON_PROJECT_ID"),
            photon_webhook_secret: s.secret("PHOTON_WEBHOOK_SECRET"),
            photon_agent_handle: s.opt("PHOTON_AGENT_HANDLE"),
            telnyx_api_key: s.secret("TELNYX_API_KEY"),
            telnyx_public_key: s.opt("TELNYX_PUBLIC_KEY"),
            telnyx_connection_id: s.opt("TELNYX_CONNECTION_ID"),
            telnyx_from_number: s.opt("TELNYX_FROM_NUMBER"),
            xai_api_key: s.secret("XAI_API_KEY"),
            object_storage_mode: s.string("OBJECT_STORAGE_MODE", "local"),
            object_storage_local_dir: s.string("OBJECT_STORAGE_LOCAL_DIR", "./.data/objects"),
            allow_live_outbound_calls: s.boolean("ALLOW_LIVE_OUTBOUND_CALLS", false)?,
            allow_live_purchases: s.boolean("ALLOW_LIVE_PURCHASES", false)?,
            store_backend,
            host: s.string("HOST", "0.0.0.0"),
            port,
        })
    }

    /// `Settings.cors_origin_list`.
    pub fn cors_origin_list(&self) -> Vec<String> {
        self.cors_origins
            .split(',')
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// `Settings.missing_for_live()`: live-mode providers lacking credentials (names only).
    /// Order matches the Python dict: matterport, vexcel, messaging, voice.
    pub fn missing_for_live(&self) -> Vec<(&'static str, Vec<&'static str>)> {
        let req: [(&'static str, ProviderMode, &[&'static str]); 4] = [
            (
                "matterport",
                self.matterport_provider_mode,
                &["MATTERPORT_API_KEY", "MATTERPORT_API_TOKEN"],
            ),
            ("vexcel", self.vexcel_provider_mode, &["VEXCEL_API_KEY"]),
            (
                "messaging",
                self.messaging_provider_mode,
                &[
                    "PHOTON_API_KEY",
                    "PHOTON_PROJECT_ID",
                    "PHOTON_WEBHOOK_SECRET",
                ],
            ),
            (
                "voice",
                self.voice_provider_mode,
                &["TELNYX_API_KEY", "TELNYX_PUBLIC_KEY", "XAI_API_KEY"],
            ),
        ];
        let mut out = Vec::new();
        for (name, mode, envs) in req {
            if mode == ProviderMode::Live {
                let missing: Vec<&'static str> =
                    envs.iter().copied().filter(|e| !self.has(e)).collect();
                if !missing.is_empty() {
                    out.push((name, missing));
                }
            }
        }
        out
    }

    /// `Settings._has(env_name)`: is the field behind this variable set to a non-empty value.
    pub fn has(&self, env_name: &str) -> bool {
        fn sec(s: &Option<Secret>) -> bool {
            s.as_ref().is_some_and(|v| !v.is_empty())
        }
        fn opt(s: &Option<String>) -> bool {
            s.as_ref().is_some_and(|v| !v.is_empty())
        }
        match env_name {
            "MATTERPORT_API_KEY" => sec(&self.matterport_api_key),
            "MATTERPORT_API_TOKEN" => sec(&self.matterport_api_token),
            "MATTERPORT_SDK_KEY" => sec(&self.matterport_sdk_key),
            "VEXCEL_API_KEY" => sec(&self.vexcel_api_key),
            "PHOTON_API_KEY" => sec(&self.photon_api_key),
            "PHOTON_PROJECT_ID" => opt(&self.photon_project_id),
            "PHOTON_WEBHOOK_SECRET" => sec(&self.photon_webhook_secret),
            "PHOTON_AGENT_HANDLE" => opt(&self.photon_agent_handle),
            "TELNYX_API_KEY" => sec(&self.telnyx_api_key),
            "TELNYX_PUBLIC_KEY" => opt(&self.telnyx_public_key),
            "TELNYX_CONNECTION_ID" => opt(&self.telnyx_connection_id),
            "TELNYX_FROM_NUMBER" => opt(&self.telnyx_from_number),
            "XAI_API_KEY" => sec(&self.xai_api_key),
            "ANTHROPIC_API_KEY" => sec(&self.anthropic_api_key),
            "LA_OPEN_DATA_APP_TOKEN" => sec(&self.la_open_data_app_token),
            "SUPERVISOR_MODEL" => opt(&self.supervisor_model),
            "AUTH_SECRET" => !self.auth_secret.is_empty(),
            _ => false,
        }
    }

    /// Startup checks the Python service does not make. Refuses to run in production with the
    /// development signing secret (or an empty one), and refuses modes this backend does not have.
    pub fn check_startup(&self) -> Result<(), ConfigError> {
        if self.app_env == "production"
            && (self.auth_secret.expose() == DEV_AUTH_SECRET || self.auth_secret.is_empty())
        {
            return Err(ConfigError(
                "APP_ENV=production requires AUTH_SECRET to be set to a real secret \
                 (it is still the development default)"
                    .into(),
            ));
        }
        if self.task_execution_mode != "inline" {
            return Err(ConfigError(format!(
                "TASK_EXECUTION_MODE={:?} is not supported by the Rust backend yet; use 'inline'",
                self.task_execution_mode
            )));
        }
        Ok(())
    }

    /// `DATABASE_URL` without SQLAlchemy's `+driver` suffix (`postgresql+psycopg://` ->
    /// `postgresql://`), so the same value works for both backends.
    pub fn sqlx_database_url(&self) -> String {
        normalize_database_url(&self.database_url)
    }
}

/// Strip a SQLAlchemy driver suffix from a database URL scheme.
pub fn normalize_database_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if let Some((base, _driver)) = scheme.split_once('+') {
            return format!("{base}://{rest}");
        }
    }
    url.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let s = Settings::from_vars(Vec::<(String, String)>::new()).unwrap();
        assert_eq!(s.app_env, "development");
        assert_eq!(s.app_version, "0.1.0");
        assert_eq!(s.auth_secret.expose(), DEV_AUTH_SECRET);
        assert!(s.allow_dev_tokens);
        assert_eq!(s.property_provider_mode, ProviderMode::Fixture);
        assert_eq!(s.cors_origin_list(), vec!["http://localhost:3000"]);
        assert_eq!(s.store_backend, StoreBackend::Postgres);
        assert_eq!(
            s.sqlx_database_url(),
            "postgresql://secondsite:secondsite@localhost:5432/secondsite"
        );
        assert!(s.missing_for_live().is_empty());
        assert!(s.check_startup().is_ok());
    }

    #[test]
    fn names_are_case_insensitive_and_bools_lax() {
        let s = Settings::from_vars([("allow_dev_tokens", "off"), ("App_Env", "test")]).unwrap();
        assert!(!s.allow_dev_tokens);
        assert_eq!(s.app_env, "test");
        assert!(Settings::from_vars([("ALLOW_DEV_TOKENS", "maybe")]).is_err());
        assert!(Settings::from_vars([("VEXCEL_PROVIDER_MODE", "LIVE")]).is_err());
    }

    #[test]
    fn production_refuses_dev_secret() {
        let s = Settings::from_vars([("APP_ENV", "production")]).unwrap();
        assert!(s.check_startup().is_err());
        let s = Settings::from_vars([("APP_ENV", "production"), ("AUTH_SECRET", "")]).unwrap();
        assert!(s.check_startup().is_err());
        let s = Settings::from_vars([("APP_ENV", "production"), ("AUTH_SECRET", "s3cr3t-value")])
            .unwrap();
        assert!(s.check_startup().is_ok());
    }

    #[test]
    fn redis_mode_is_refused() {
        let s = Settings::from_vars([("TASK_EXECUTION_MODE", "redis")]).unwrap();
        assert!(s.check_startup().is_err());
    }

    #[test]
    fn missing_for_live_lists_names_only() {
        let s = Settings::from_vars([
            ("VEXCEL_PROVIDER_MODE", "live"),
            ("VOICE_PROVIDER_MODE", "live"),
            ("TELNYX_API_KEY", "x"),
        ])
        .unwrap();
        assert_eq!(
            s.missing_for_live(),
            vec![
                ("vexcel", vec!["VEXCEL_API_KEY"]),
                ("voice", vec!["TELNYX_PUBLIC_KEY", "XAI_API_KEY"]),
            ]
        );
    }

    #[test]
    fn secrets_are_not_printed() {
        let s = Settings::from_vars([("AUTH_SECRET", "hunter2")]).unwrap();
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn dotenv_parsing() {
        let v = parse_dotenv("# c\nexport A=1\nB = \"two words\"\nC=x # comment\n\nD='q'\n");
        assert_eq!(
            v,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "x".into()),
                ("D".into(), "q".into()),
            ]
        );
    }
}
