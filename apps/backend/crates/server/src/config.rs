//! Port of `apps/api/app/core/config.py`: validated settings read from the same environment
//! variable names, with the same defaults. Secrets are wrapped in [`Secret`] and never printed.
//!
//! Sources, highest priority first (as pydantic-settings): process environment, `../../.env`,
//! `.env` (both relative to the working directory). Names are matched case-insensitively. An empty
//! value is a value (pydantic-settings does not ignore empty variables).
//!
//! Rust-only additions: `STORE_BACKEND` (`postgres` default, or `memory`), `HOST` and `PORT`
//! (what uvicorn took on its command line), and `FIXTURES_DIR` (where the fixture providers read
//! `fixtures/`; Python resolves it from its source file, which a container image does not have).

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
    /// `VOICE_BRIDGE_ENABLED` (default false): the Grok Voice bridge carries the call's speech.
    /// It is used only when this is on, `VOICE_PROVIDER_MODE=live` and `XAI_API_KEY` is set;
    /// otherwise calls use the carrier's `speak` and transcription, as before.
    pub voice_bridge_enabled: bool,
    /// `XAI_REALTIME_URL` (default `xai_realtime::DEFAULT_REALTIME_URL`).
    pub xai_realtime_url: String,
    /// `XAI_VOICE` (default `eve`).
    pub xai_voice: String,
    /// `VOICE_INTRO_GRACE_S` (default 15): how long after the recording's own length the call
    /// waits for the carrier's playback-ended event before it ends the call politely.
    pub voice_intro_grace_s: u64,
    // OBJECT STORAGE
    pub object_storage_mode: String,
    pub object_storage_local_dir: String,
    // SAFETY
    pub allow_live_outbound_calls: bool,
    pub allow_live_purchases: bool,
    /// `PUBLIC_PATH_GUARD` (default false): a request that came through a tunnel reaches only the
    /// carrier's routes; every other path is 404 (`http::public_path_guard`).
    pub public_path_guard: bool,
    // RUST-ONLY ADDITIONS
    pub store_backend: StoreBackend,
    pub host: String,
    pub port: u16,
    /// `FIXTURES_DIR`: the repository's `fixtures/` directory (default: resolved from this crate's
    /// source location at build time, like Python's `FIXTURES_DIR`).
    pub fixtures_dir: std::path::PathBuf,
    /// `PYTHON_SERVICE_URL`: base URL of the Python service (Photon, owner texts); unset means the
    /// owner cannot be texted from this service.
    pub python_service_url: Option<String>,
    /// `NESSIE_PROVIDER_MODE`, `NESSIE_API_KEY`, `NESSIE_BASE_URL` (Rust-only), raw: the
    /// `ss-nessie` crate parses them (`services::nessie::nessie_config`).
    pub nessie_provider_mode: Option<String>,
    pub nessie_api_key: Option<Secret>,
    pub nessie_base_url: Option<String>,
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

/// One `.env` file as pydantic-settings reads it (`dotenv_values(path, encoding="utf-8")`, whose
/// `open()` turns `\r\n` and a lone `\r` into `\n` before python-dotenv sees the text): the
/// newlines translated, then [`dotenv_values`].
fn read_dotenv(text: &str, environ: &dyn Fn(&str) -> Option<String>) -> DotenvValues {
    dotenv_values(&text.replace("\r\n", "\n").replace('\r', "\n"), environ)
}

/// `(key, value)` in python-dotenv's dict order: first appearance, last value; `None` for a key
/// without `=` (pydantic-settings treats it as unset, also over an earlier file's value).
type DotenvValues = Vec<(String, Option<String>)>;

/// python-dotenv 1.2.4's `dotenv_values(stream=...)` (`DotEnv.dict` with `interpolate=True` and
/// `override=True`, as pydantic-settings calls it), ported from its `parser.py`, `main.py` and
/// `variables.py`; `environ` is the process environment (`os.environ`, names case-sensitive).
/// `tests-golden/server/dotenv_cases.json` holds python-dotenv's answers (`gen_dotenv.py`).
///
/// The text is read as one stream, binding by binding (`parse_binding`), not line by line:
/// - a leading BOM is dropped; whitespace (Python's `\s`, which also counts U+001C..U+001F) and
///   blank lines between bindings are skipped; `\r\n`, `\n` and a lone `\r` end a line;
/// - an optional `export` plus whitespace precedes the key; a key is `'quoted'` (it may span
///   lines) or a run without `=`, `#` and whitespace; a `#` there makes the line a comment; a
///   key without `=` maps to `None`;
/// - `KEY=` followed by whitespace and `#` is empty (`PHOTON_API_KEY=      # comment`), while
///   `KEY=#x` is `#x`;
/// - an unquoted value runs to the end of the line, is cut at the first whitespace run followed
///   by `#` (`a b # c` is `a b`, `a#b` stays `a#b`) and loses its trailing whitespace;
/// - a `'single'` or `"double"` quoted value runs to the closing quote, across line breaks (a
///   backslash escapes any character, so `\"` does not close it), keeps `#` and spaces, and
///   decodes python-dotenv's escapes (`\\` and `\'` in single quotes; also `\"`, `\a`, `\b`,
///   `\f`, `\n`, `\r`, `\t`, `\v` in double quotes); a comment may follow it;
/// - anything else after a value, or a quote that never closes, is an error: that binding is
///   dropped and reading resumes after the end of the line where the error was found.
///
/// Then each value, in file order, has `${NAME}` and `${NAME:-default}` replaced (in quoted
/// values too, single quotes included): `NAME` is looked up among the values bound earlier in
/// this file first, then in `environ`; a name bound to `None` earlier, or unset with no default,
/// reads as empty; a set but empty name stays empty (no default). `$NAME` without braces stays
/// as it is.
fn dotenv_values(text: &str, environ: &dyn Fn(&str) -> Option<String>) -> DotenvValues {
    let chars: Vec<char> = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .chars()
        .collect();
    let mut r = DotenvReader { s: &chars, pos: 0 };
    let mut out: DotenvValues = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    while r.pos < chars.len() {
        let Some((Some(key), value)) = r.binding() else {
            continue;
        };
        // `resolve_variables(..., override=True)`: earlier values of this file over `environ`.
        let value = value.map(|v| {
            interpolate(&v, &|name: &str| match index.get(name) {
                Some(&i) => Some(out[i].1.clone()),
                None => environ(name).map(Some),
            })
        });
        match index.get(&key) {
            Some(&i) => out[i].1 = value,
            None => {
                index.insert(key.clone(), out.len());
                out.push((key, value));
            }
        }
    }
    out
}

/// Python's `str.isspace()`, which `\s` in its `re` module also uses: Unicode `White_Space` plus
/// the four information separators U+001C..U+001F.
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `[^\S\r\n]`: whitespace other than a line break.
fn hspace(c: char) -> bool {
    py_space(c) && c != '\r' && c != '\n'
}

/// python-dotenv's `Reader` over the text's code points.
struct DotenvReader<'a> {
    s: &'a [char],
    pos: usize,
}

impl DotenvReader<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.pos).copied()
    }

    /// Consume characters while `f` holds; how many.
    fn skip(&mut self, f: impl Fn(char) -> bool) -> usize {
        let start = self.pos;
        while self.peek().is_some_and(&f) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn take(&self, from: usize, to: usize) -> String {
        self.s[from..to].iter().collect()
    }

    /// `parse_binding`: `None` when only whitespace was left; `Some((key, value))` otherwise,
    /// with no key for a comment line or an error.
    fn binding(&mut self) -> Option<(Option<String>, Option<String>)> {
        self.skip(py_space);
        if self.pos >= self.s.len() {
            return None;
        }
        match self.binding_body() {
            Some(b) => Some(b),
            None => {
                // `_rest_of_line`, from where the error was found: `[^\r\n]*(?:\r|\n|\r\n)?`.
                self.skip(|c| c != '\r' && c != '\n');
                if matches!(self.peek(), Some('\r' | '\n')) {
                    self.pos += 1;
                }
                Some((None, None))
            }
        }
    }

    /// Everything after the leading whitespace; `None` is python-dotenv's `Error`, raised by a
    /// read that does not match and so leaves the position where that read began.
    fn binding_body(&mut self) -> Option<(Option<String>, Option<String>)> {
        // `(?:export[^\S\r\n]+)?`
        let export: Vec<char> = "export".chars().collect();
        if self.s[self.pos..].starts_with(&export)
            && self
                .s
                .get(self.pos + export.len())
                .is_some_and(|&c| hspace(c))
        {
            self.pos += export.len();
            self.skip(hspace);
        }
        let key = match self.peek() {
            Some('#') => None,
            Some('\'') => {
                // `'([^']+)'`
                let start = self.pos + 1;
                let len = self.s[start..].iter().position(|&c| c == '\'')?;
                if len == 0 {
                    return None;
                }
                self.pos = start + len + 1;
                Some(self.take(start, start + len))
            }
            _ => {
                // `([^=\#\s]+)`
                let start = self.pos;
                if self.skip(|c| c != '=' && c != '#' && !py_space(c)) == 0 {
                    return None;
                }
                Some(self.take(start, self.pos))
            }
        };
        self.skip(hspace);
        let value = if self.peek() == Some('=') {
            self.pos += 1;
            let spaced = self.skip(hspace) > 0;
            if spaced && self.peek() == Some('#') {
                Some(String::new())
            } else {
                Some(self.value()?)
            }
        } else {
            None
        };
        // `(?:[^\S\r\n]*#[^\r\n]*)?`
        let before = self.pos;
        self.skip(hspace);
        if self.peek() == Some('#') {
            self.skip(|c| c != '\r' && c != '\n');
        } else {
            self.pos = before;
        }
        // `[^\S\r\n]*(?:\r\n|\n|\r|$)`
        let before = self.pos;
        self.skip(hspace);
        match self.peek() {
            None => {}
            Some('\n') => self.pos += 1,
            Some('\r') => {
                self.pos += 1;
                if self.peek() == Some('\n') {
                    self.pos += 1;
                }
            }
            Some(_) => {
                self.pos = before;
                return None;
            }
        }
        Some((key, value))
    }

    /// `parse_value`.
    fn value(&mut self) -> Option<String> {
        match self.peek() {
            Some(q @ ('\'' | '"')) => {
                // `'((?:\\.|[^'\\])*)'` (and the double-quoted one), with DOTALL.
                let start = self.pos + 1;
                let mut i = start;
                loop {
                    match *self.s.get(i)? {
                        '\\' if i + 1 < self.s.len() => i += 2,
                        '\\' => return None,
                        c if c == q => break,
                        _ => i += 1,
                    }
                }
                self.pos = i + 1;
                Some(decode_escapes(&self.s[start..i], q))
            }
            None | Some('\n' | '\r') => Some(String::new()),
            Some(_) => {
                // `([^\r\n]*)`, then `re.sub(r"\s+#.*", "", part).rstrip()`.
                let start = self.pos;
                self.skip(|c| c != '\r' && c != '\n');
                let part = &self.s[start..self.pos];
                let mut cut = part.len();
                let mut i = 0;
                while i < part.len() {
                    if py_space(part[i]) {
                        let run = part[i..].iter().take_while(|&&c| py_space(c)).count();
                        if part.get(i + run) == Some(&'#') {
                            cut = i;
                            break;
                        }
                        i += run;
                    } else {
                        i += 1;
                    }
                }
                let kept = &part[..cut];
                let end = kept
                    .iter()
                    .rposition(|&c| !py_space(c))
                    .map_or(0, |p| p + 1);
                Some(kept[..end].iter().collect())
            }
        }
    }
}

/// `decode_escapes`: python-dotenv's escapes of a quoted value, left to right (`\\` and `\'`
/// in single quotes; also `\"`, `\a`, `\b`, `\f`, `\n`, `\r`, `\t`, `\v` in double quotes); any
/// other backslash stays.
fn decode_escapes(body: &[char], quote: char) -> String {
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        let decoded = match (body[i], body.get(i + 1).copied(), quote) {
            ('\\', Some('\\'), _) => Some('\\'),
            ('\\', Some('\''), _) => Some('\''),
            ('\\', Some('"'), '"') => Some('"'),
            ('\\', Some('a'), '"') => Some('\u{7}'),
            ('\\', Some('b'), '"') => Some('\u{8}'),
            ('\\', Some('f'), '"') => Some('\u{c}'),
            ('\\', Some('n'), '"') => Some('\n'),
            ('\\', Some('r'), '"') => Some('\r'),
            ('\\', Some('t'), '"') => Some('\t'),
            ('\\', Some('v'), '"') => Some('\u{b}'),
            _ => None,
        };
        match decoded {
            Some(d) => {
                out.push(d);
                i += 2;
            }
            None => {
                out.push(body[i]);
                i += 1;
            }
        }
    }
    out
}

/// python-dotenv's `parse_variables` and `Variable.resolve`: every `${NAME}` or
/// `${NAME:-default}` (`\$\{([^}:]*)(?::-([^}]*))?\}`, left to right) replaced by `lookup(NAME)`:
/// `Some(Some(v))` is `v`, `Some(None)` (bound without a value) is empty, `None` (unset) is the
/// default or empty. Anything else is kept as it is.
fn interpolate(value: &str, lookup: &dyn Fn(&str) -> Option<Option<String>>) -> String {
    let s: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut literal = 0;
    let mut i = 0;
    while i < s.len() {
        let Some((end, name, default)) = variable_at(&s, i) else {
            i += 1;
            continue;
        };
        out.extend(&s[literal..i]);
        out.push_str(&match lookup(&name) {
            Some(Some(v)) => v,
            Some(None) => String::new(),
            None => default.unwrap_or_default(),
        });
        i = end;
        literal = end;
    }
    out.extend(&s[literal..]);
    out
}

/// The `${NAME}` or `${NAME:-default}` starting at `i`: (end, name, default).
fn variable_at(s: &[char], i: usize) -> Option<(usize, String, Option<String>)> {
    if s.get(i) != Some(&'$') || s.get(i + 1) != Some(&'{') {
        return None;
    }
    let name_start = i + 2;
    let name_len = s[name_start..].iter().position(|&c| c == '}' || c == ':')?;
    let name: String = s[name_start..name_start + name_len].iter().collect();
    let after = name_start + name_len;
    if s[after] == '}' {
        return Some((after + 1, name, None));
    }
    // `:` must start `:-default}`.
    if s.get(after + 1) != Some(&'-') {
        return None;
    }
    let default_start = after + 2;
    let default_len = s[default_start..].iter().position(|&c| c == '}')?;
    let default = s[default_start..default_start + default_len]
        .iter()
        .collect();
    Some((default_start + default_len + 1, name, Some(default)))
}

impl Settings {
    /// `get_settings()`: environment plus `.env` files relative to the working directory.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_sources(Path::new(""), std::env::vars())
    }

    /// `from_env` with the `.env` files read relative to `dir` and `env` in place of the process
    /// environment (tests).
    pub fn from_sources(
        dir: &Path,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, ConfigError> {
        let env: Vec<(String, String)> = env.into_iter().collect();
        let environ = |name: &str| {
            env.iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let mut vars: HashMap<String, String> = HashMap::new();
        // Lowest priority first; later sources overwrite. `${NAME}` in a file reads `env`
        // (python-dotenv reads `os.environ`); a key without `=` unsets the name.
        // A file that is there but cannot be read as UTF-8 stops the start, as in Python
        // (pydantic-settings opens every `is_file()` with encoding utf-8 and raises).
        for file in [".env", "../../.env"] {
            let path = dir.join(file);
            if path.is_file() {
                let text = std::fs::read(&path)
                    .map_err(|e| ConfigError(format!("{file}: cannot be read ({})", e.kind())))
                    .and_then(|b| {
                        String::from_utf8(b).map_err(|e| {
                            let at = e.utf8_error().valid_up_to();
                            ConfigError(format!("{file}: not valid UTF-8 (at byte {at})"))
                        })
                    })?;
                for (k, v) in read_dotenv(&text, &environ) {
                    match v {
                        Some(v) => vars.insert(k.to_ascii_uppercase(), v),
                        None => vars.remove(&k.to_ascii_uppercase()),
                    };
                }
            }
        }
        for (k, v) in env {
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
            voice_bridge_enabled: s.boolean("VOICE_BRIDGE_ENABLED", false)?,
            xai_realtime_url: s.string(
                "XAI_REALTIME_URL",
                crate::integrations::xai_realtime::DEFAULT_REALTIME_URL,
            ),
            xai_voice: s.string("XAI_VOICE", "eve"),
            voice_intro_grace_s: s
                .string("VOICE_INTRO_GRACE_S", "15")
                .trim()
                .parse::<u64>()
                .unwrap_or(15)
                .min(120),
            object_storage_mode: s.string("OBJECT_STORAGE_MODE", "local"),
            object_storage_local_dir: s.string("OBJECT_STORAGE_LOCAL_DIR", "./.data/objects"),
            allow_live_outbound_calls: s.boolean("ALLOW_LIVE_OUTBOUND_CALLS", false)?,
            allow_live_purchases: s.boolean("ALLOW_LIVE_PURCHASES", false)?,
            public_path_guard: s.boolean("PUBLIC_PATH_GUARD", false)?,
            store_backend,
            host: s.string("HOST", "0.0.0.0"),
            port,
            fixtures_dir: match s.get("FIXTURES_DIR").map(str::trim) {
                None | Some("") => ss_domain::integrations::base::fixtures_dir(),
                Some(dir) => std::path::PathBuf::from(dir),
            },
            // Empty or blank is unset, as for the Python service's own optional URLs.
            python_service_url: s
                .opt("PYTHON_SERVICE_URL")
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
            nessie_provider_mode: s.opt("NESSIE_PROVIDER_MODE"),
            nessie_api_key: s.secret("NESSIE_API_KEY"),
            nessie_base_url: s.opt("NESSIE_BASE_URL"),
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

    /// Whether a call's speech goes through the Grok Voice bridge: the explicit switch, the live
    /// voice provider and an xAI key, all three. Off, calls behave exactly as before.
    pub fn voice_bridge_active(&self) -> bool {
        self.voice_bridge_enabled
            && self.voice_provider_mode == ProviderMode::Live
            && self.xai_api_key.as_ref().is_some_and(|k| !k.is_empty())
    }

    /// The settings a live call cannot do without, missing or plainly unusable: names only,
    /// never a value. Empty outside `VOICE_PROVIDER_MODE=live`. Without `TELNYX_PUBLIC_KEY` every
    /// webhook is answered 400 and without a public `https` `PUBLIC_BASE_URL` none arrives, so
    /// the server could not hear, steer or end a call it had dialed. With `VOICE_BRIDGE_ENABLED`
    /// the xAI key too (the `wss` stream address is built from the same `https` base).
    pub fn voice_live_missing(&self) -> Vec<&'static str> {
        fn public_https(url: &str) -> bool {
            let Some(rest) = url.trim().strip_prefix("https://") else {
                return false;
            };
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            let hostport = authority.rsplit('@').next().unwrap_or("");
            let host = match hostport.strip_prefix('[') {
                Some(v6) => v6.split(']').next().unwrap_or(""),
                None => hostport.split(':').next().unwrap_or(""),
            };
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            match host.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V4(a)) => {
                    !(a.is_loopback()
                        || a.is_private()
                        || a.is_link_local()
                        || a.is_unspecified()
                        || a.is_broadcast())
                }
                Ok(std::net::IpAddr::V6(a)) => {
                    let first = a.segments()[0];
                    !(a.is_loopback()
                        || a.is_unspecified()
                        || first & 0xfe00 == 0xfc00
                        || first & 0xffc0 == 0xfe80
                        || a.to_ipv4_mapped().is_some())
                }
                // A name: at least two labels, and not one that only resolves locally.
                Err(_) => {
                    host.contains('.')
                        && !host.starts_with('.')
                        && ![".localhost", ".local", ".internal", ".lan"]
                            .iter()
                            .any(|s| host.ends_with(s))
                }
            }
        }
        if self.voice_provider_mode != ProviderMode::Live {
            return Vec::new();
        }
        let set = |v: &Option<String>| v.as_deref().is_some_and(|v| !v.trim().is_empty());
        let caller = self.telnyx_from_number.as_deref().map(str::trim);
        let caller_ok = caller
            .and_then(|n| n.strip_prefix('+'))
            .is_some_and(|d| (8..=15).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()));
        let mut out = Vec::new();
        if !self
            .telnyx_api_key
            .as_ref()
            .is_some_and(|k| !k.expose().trim().is_empty())
        {
            out.push("TELNYX_API_KEY");
        }
        if !set(&self.telnyx_connection_id) {
            out.push("TELNYX_CONNECTION_ID");
        }
        if !caller_ok {
            out.push("TELNYX_FROM_NUMBER");
        }
        if !set(&self.telnyx_public_key) {
            out.push("TELNYX_PUBLIC_KEY");
        }
        if !public_https(&self.public_base_url) {
            out.push("PUBLIC_BASE_URL");
        }
        if self.voice_bridge_enabled
            && !self
                .xai_api_key
                .as_ref()
                .is_some_and(|k| !k.expose().trim().is_empty())
        {
            out.push("XAI_API_KEY");
        }
        out
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
        assert!(s.fixtures_dir.join("permits").is_dir());
        let s = Settings::from_vars([("FIXTURES_DIR", "/srv/fixtures")]).unwrap();
        assert_eq!(s.fixtures_dir, std::path::PathBuf::from("/srv/fixtures"));
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

    /// The bindings of `text` with an empty environment, keys without a value left out (the
    /// shape of the line cases).
    fn parse_dotenv(text: &str) -> Vec<(String, String)> {
        dotenv_values(text, &|_| None)
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect()
    }

    fn golden_dotenv() -> serde_json::Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests-golden/server/dotenv_cases.json"
        )))
        .unwrap()
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

    /// Every line of `dotenv_cases.json` reads as python-dotenv reads it (a key python-dotenv
    /// maps to `None`, a key without `=`, binds nothing here, as an unset variable).
    #[test]
    fn dotenv_lines_match_python_dotenv() {
        let golden = golden_dotenv();
        let cases = golden["cases"].as_array().unwrap();
        assert!(cases.len() >= 45);
        for case in cases {
            let line = case["line"].as_str().unwrap();
            let want: Vec<(String, String)> = case["values"]
                .as_object()
                .unwrap()
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect();
            assert_eq!(parse_dotenv(line), want, "{line:?}");
        }
        // The .env.example shape that used to read as "# comment".
        assert_eq!(
            parse_dotenv("TELNYX_PUBLIC_KEY=      # Ed25519 key\n"),
            vec![("TELNYX_PUBLIC_KEY".into(), String::new())]
        );
    }

    /// Every file of `dotenv_cases.json` reads as pydantic-settings reads it (python-dotenv
    /// through a real file, with the generator's environment): `${NAME}` interpolation in every
    /// quoting, lone `\r` line breaks, quoted values over several lines, dict order with
    /// duplicates, keys without a value, the BOM, Python's whitespace, the whole `.env.example`.
    #[test]
    fn dotenv_files_match_python_dotenv() {
        let golden = golden_dotenv();
        let environ: HashMap<String, String> = golden["environ"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        let files = golden["files"].as_array().unwrap();
        assert!(files.len() >= 57);
        for f in files {
            let want: DotenvValues = f["values"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    (
                        p[0].as_str().unwrap().to_owned(),
                        p[1].as_str().map(str::to_owned),
                    )
                })
                .collect();
            let got = read_dotenv(f["text"].as_str().unwrap(), &|n| environ.get(n).cloned());
            assert_eq!(got, want, "{}", f["name"]);
        }
    }

    /// The loader end to end: `${NAME}` reads the given environment, a key without a value in
    /// the higher-priority file unsets what the other file set, the environment still wins.
    #[test]
    fn dotenv_files_are_interpolated_and_merged_as_pydantic_settings_does() {
        let root = std::env::temp_dir().join(format!("ss-dotenv-{}", uuid::Uuid::new_v4()));
        let dir = root.join("a").join("b");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "DB_USER=alice\r\nDATABASE_URL=postgresql://${DB_USER}@${DB_HOST:-localhost}/db\r\n\
             PHOTON_PROJECT_ID=from-dot-env\nAPP_ENV=from-dot-env\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".env"),
            "PHOTON_PROJECT_ID\rAPP_VERSION='${VERSION}'\r",
        )
        .unwrap();
        let env = vec![
            ("VERSION".to_owned(), "9.9.9".to_owned()),
            ("APP_ENV".to_owned(), "from-env".to_owned()),
        ];
        let s = Settings::from_sources(&dir, env).unwrap();
        // A file that is not UTF-8 stops the start (Python raises UnicodeDecodeError), with
        // the file's name and no value; a directory named `.env` is skipped, as in Python.
        std::fs::write(root.join(".env"), b"APP_ENV=x\nAUTH_SECRET=\xff\xfe\n").unwrap();
        let e = Settings::from_sources(&dir, Vec::new())
            .unwrap_err()
            .to_string();
        assert!(e.contains("../../.env: not valid UTF-8"), "{e}");
        std::fs::remove_file(root.join(".env")).unwrap();
        std::fs::create_dir(root.join(".env")).unwrap();
        assert!(Settings::from_sources(&dir, Vec::new()).is_ok());
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(s.database_url, "postgresql://alice@localhost/db");
        assert_eq!(s.photon_project_id, None);
        assert_eq!(s.app_version, "9.9.9");
        assert_eq!(s.app_env, "from-env");
    }

    #[test]
    fn a_blank_python_service_url_is_unset() {
        for v in ["", "   ", "\t"] {
            let s = Settings::from_vars([("PYTHON_SERVICE_URL", v)]).unwrap();
            assert_eq!(s.python_service_url, None, "{v:?}");
        }
        let s = Settings::from_vars([("PYTHON_SERVICE_URL", " http://python:8001 ")]).unwrap();
        assert_eq!(s.python_service_url.as_deref(), Some("http://python:8001"));
    }
}
