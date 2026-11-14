//! Port of `apps/api/app/integrations/matterport/__init__.py`.
//!
//! Live: Matterport Model API (GraphQL, `MATTERPORT_GRAPH_URL`), HTTP Basic auth with the token
//! id (`MATTERPORT_API_KEY`) and secret (`MATTERPORT_API_TOKEN`), retry policy 2 attempts / 40 s.
//! `model_summaries` batches 20 models per request with GraphQL aliases `m0..m19`; model ids are
//! checked against `[A-Za-z0-9_-]{4,64}` before they are put into a query. Geometry comes back in
//! meters in the model's local frame; [`floor_geometry_from_model`] turns the largest floor's wall
//! graph into the `SiteGeometry` fields the layout solver uses.
//!
//! Fixture: the synthetic geometry documents under `fixtures/geometry/`; no model summaries.
//!
//! Truth rules: the measured `areaFloor` is the scan's area, never the listing's area; a scan
//! counts as the listed suite only when its title names the suite ([`suite_matches`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use base64::Engine as _;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};
use ss_core::IntegrationStatus;
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{
    status_entry, ProviderError, ProviderErrorKind, RetryPolicy,
};
use ss_domain::py;

use super::http::HttpClient;

pub const PROVIDER: &str = "matterport";

/// `SUMMARY_FIELDS`.
pub const SUMMARY_FIELDS: &str = "id name dimensions(units: metric) { areaFloor } floors { id }";

/// `GEOMETRY_QUERY`, byte for byte.
pub const GEOMETRY_QUERY: &str = "
query ModelGeometry($id: ID!) {
  model(id: $id) {
    id
    name
    dimensions(units: metric) { areaFloor }
    floors {
      id
      sequence
      label
      dimensions(units: metric) { areaFloor }
      vertices { id position { x y } }
      edges { id type thickness vertices { id } openings { type width relativeCenter } }
    }
    rooms { id label floor { id } classifications { label confidence } dimensions(units: metric) { areaFloor width depth } }
  }
}
";

/// Name and measured area of one model (`model_summaries` values).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSummary {
    /// The model title, stripped; `None` when empty.
    pub name: Option<String>,
    /// Matterport `dimensions.areaFloor` (m2) of the whole scan; not the listing's area.
    pub area_sqm: Option<f64>,
    pub floor_count: usize,
}

/// `MatterportProvider` (plus `model_summaries`, which both Python implementations have).
pub trait MatterportProvider: Send + Sync {
    fn mode(&self) -> &str;
    /// A `SiteGeometry`-shaped document for one model, or `None`.
    fn get_floor_geometry(&self, asset_id: &str) -> Result<Option<Value>, ProviderError>;
    /// Summaries keyed by model id, in request order; inaccessible models are left out.
    fn model_summaries(
        &self,
        model_ids: &[String],
    ) -> Result<IndexMap<String, ModelSummary>, ProviderError>;
    fn status(&self) -> ProviderStatusEntry;
}

fn bad(msg: impl Into<String>) -> ProviderError {
    ProviderError::new(PROVIDER, ProviderErrorKind::BadResponse, msg)
}

// ---- fixture ------------------------------------------------------------------------------------

/// Synthetic rectangle geometry for offline work and CI (`fixtures/geometry/*.json`).
#[derive(Debug, Clone)]
pub struct FixtureMatterportProvider {
    pub geometry_dir: PathBuf,
}

impl FixtureMatterportProvider {
    pub fn new(fixtures_dir: &std::path::Path) -> Self {
        Self {
            geometry_dir: fixtures_dir.join("geometry"),
        }
    }
}

impl MatterportProvider for FixtureMatterportProvider {
    fn mode(&self) -> &str {
        "fixture"
    }

    /// The first `*.json` document whose `source_id` is `asset_id` (files in name order; Python
    /// takes directory order).
    fn get_floor_geometry(&self, asset_id: &str) -> Result<Option<Value>, ProviderError> {
        let not_configured = |m: String| {
            ProviderError::new(PROVIDER, ProviderErrorKind::NotConfigured, m)
        };
        let entries = std::fs::read_dir(&self.geometry_dir).map_err(|e| {
            not_configured(format!("geometry fixtures {}: {e}", self.geometry_dir.display()))
        })?;
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        for f in files {
            let text = std::fs::read_to_string(&f)
                .map_err(|e| not_configured(format!("{}: {e}", f.display())))?;
            let doc: Value =
                serde_json::from_str(&text).map_err(|e| bad(format!("{}: {e}", f.display())))?;
            if doc.get("source_id").and_then(Value::as_str) == Some(asset_id) {
                return Ok(Some(doc));
            }
        }
        Ok(None)
    }

    fn model_summaries(
        &self,
        _model_ids: &[String],
    ) -> Result<IndexMap<String, ModelSummary>, ProviderError> {
        Ok(IndexMap::new())
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            PROVIDER,
            self.mode(),
            IntegrationStatus::Mock,
            false,
            "Synthetic rectangle geometry. Set MATTERPORT_PROVIDER_MODE=live with Model API token.",
        )
    }
}

// ---- live ---------------------------------------------------------------------------------------

pub struct LiveMatterportProvider {
    graph_url: String,
    configured: bool,
    client: HttpClient,
    geometry_cache: Mutex<HashMap<String, Option<Value>>>,
}

impl std::fmt::Debug for LiveMatterportProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveMatterportProvider")
            .field("graph_url", &self.graph_url)
            .field("configured", &self.configured)
            .finish()
    }
}

/// Python's `RetryPolicy(attempts=2, timeout_s=40.0)`.
pub fn default_policy() -> RetryPolicy {
    RetryPolicy {
        attempts: 2,
        timeout_s: 40.0,
        ..RetryPolicy::default()
    }
}

impl LiveMatterportProvider {
    pub fn new(graph_url: &str, token_id: Option<&str>, token_secret: Option<&str>) -> Self {
        Self::with_policy(graph_url, token_id, token_secret, default_policy())
    }

    pub fn with_policy(
        graph_url: &str,
        token_id: Option<&str>,
        token_secret: Option<&str>,
        policy: RetryPolicy,
    ) -> Self {
        let id = token_id.unwrap_or("");
        let secret = token_secret.unwrap_or("");
        // httpx `auth=(id, secret)`: HTTP Basic.
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
        Self {
            graph_url: graph_url.to_owned(),
            configured: !id.is_empty() && !secret.is_empty(),
            client: HttpClient::new(
                PROVIDER,
                policy,
                vec![("Authorization".to_owned(), format!("Basic {basic}"))],
            ),
            geometry_cache: Mutex::new(HashMap::new()),
        }
    }

    /// `_gql`: the `data` object of a GraphQL response.
    pub fn gql(&self, query: &str, variables: Option<Value>) -> Result<Map<String, Value>, ProviderError> {
        if !self.configured {
            return Err(ProviderError::new(
                PROVIDER,
                ProviderErrorKind::NotConfigured,
                "MATTERPORT_API_KEY/TOKEN not set",
            ));
        }
        let payload = json!({"query": query, "variables": variables.unwrap_or_else(|| json!({}))});
        let body = self.client.with_retries(|| {
            self.client
                .post_json_once(&self.graph_url, &payload)?
                .json(PROVIDER)
        })?;
        let Value::Object(body) = body else {
            return Err(bad("GraphQL response is not an object"));
        };
        let errors = body.get("errors").filter(|e| py::truthy(e));
        let data = body.get("data").filter(|d| py::truthy(d));
        if let (Some(errors), None) = (errors, data) {
            let list = match errors {
                Value::Array(a) => a.clone(),
                other => vec![other.clone()],
            };
            let joined = list
                .iter()
                .map(|e| match e.get("message") {
                    Some(m) => py::value_str(m),
                    None => "None".to_owned(),
                })
                .collect::<Vec<_>>()
                .join("; ");
            let msg: String = joined.chars().take(300).collect();
            let kind = if msg.to_lowercase().contains("auth") {
                ProviderErrorKind::Auth
            } else {
                ProviderErrorKind::BadResponse
            };
            return Err(ProviderError::new(PROVIDER, kind, msg));
        }
        match data {
            None => Ok(Map::new()),
            Some(Value::Object(d)) => Ok(d.clone()),
            Some(_) => Err(bad("GraphQL data is not an object")),
        }
    }
}

/// `_safe_id`: model ids are interpolated into GraphQL text, so only `[A-Za-z0-9_-]{4,64}`.
pub fn safe_id(mid: &str) -> Result<&str, ProviderError> {
    static ID: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{4,64}$").expect("static regex is valid"));
    if ID.is_match(mid) {
        Ok(mid)
    } else {
        Err(bad(format!("invalid model id {}", py::str_repr(mid))))
    }
}

fn summary_of(m: &Map<String, Value>) -> Result<ModelSummary, ProviderError> {
    let name = match m.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(py::strip(s).to_owned()).filter(|s| !s.is_empty()),
        Some(v) if !py::truthy(v) => None,
        Some(_) => return Err(bad("model name is not a string")),
    };
    let area_sqm = match m.get("dimensions") {
        None | Some(Value::Null) => None,
        Some(Value::Object(d)) => match d.get("areaFloor") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => n.as_f64(),
            Some(_) => return Err(bad("areaFloor is not a number")),
        },
        Some(v) if !py::truthy(v) => None,
        Some(_) => return Err(bad("model dimensions is not an object")),
    };
    let floor_count = match m.get("floors") {
        None | Some(Value::Null) => 0,
        Some(Value::Array(a)) => a.len(),
        Some(v) if !py::truthy(v) => 0,
        Some(_) => return Err(bad("model floors is not a list")),
    };
    Ok(ModelSummary {
        name,
        area_sqm,
        floor_count,
    })
}

impl MatterportProvider for LiveMatterportProvider {
    fn mode(&self) -> &str {
        "live"
    }

    fn get_floor_geometry(&self, asset_id: &str) -> Result<Option<Value>, ProviderError> {
        if let Some(hit) = self
            .geometry_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(asset_id)
        {
            return Ok(hit.clone());
        }
        let id = safe_id(asset_id)?;
        let data = self.gql(GEOMETRY_QUERY, Some(json!({"id": id})))?;
        let doc = match data.get("model") {
            Some(Value::Object(model)) if !model.is_empty() => {
                super::matterport_geometry::floor_geometry_from_model(model)?
            }
            Some(v) if py::truthy(v) => return Err(bad("model is not an object")),
            _ => None,
        };
        self.geometry_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(asset_id.to_owned(), doc.clone());
        Ok(doc)
    }

    fn model_summaries(
        &self,
        model_ids: &[String],
    ) -> Result<IndexMap<String, ModelSummary>, ProviderError> {
        let mut out = IndexMap::new();
        for chunk in model_ids.chunks(20) {
            let mut parts = Vec::with_capacity(chunk.len());
            for (j, mid) in chunk.iter().enumerate() {
                parts.push(format!(
                    "m{j}: model(id: \"{}\") {{ {SUMMARY_FIELDS} }}",
                    safe_id(mid)?
                ));
            }
            let query = format!("query {{{}}}", parts.join(" "));
            let data = self.gql(&query, None)?;
            for (j, mid) in chunk.iter().enumerate() {
                match data.get(&format!("m{j}")) {
                    Some(Value::Object(m)) if !m.is_empty() => {
                        out.insert(mid.clone(), summary_of(m)?);
                    }
                    Some(v) if py::truthy(v) => return Err(bad("model entry is not an object")),
                    _ => {}
                }
            }
        }
        Ok(out)
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            PROVIDER,
            self.mode(),
            if self.configured {
                IntegrationStatus::Live
            } else {
                IntegrationStatus::Blocked
            },
            self.configured,
            if self.configured {
                "Matterport Model API (GraphQL): model names, measured areas, wall graph and openings."
            } else {
                "MATTERPORT_API_KEY / MATTERPORT_API_TOKEN missing."
            },
        )
    }
}

// ---- suite matching -----------------------------------------------------------------------------

/// `suite_matches`: true when the model title names the listed suite, e.g. `Suite 787` for suite
/// `787`. A number inside other text never matches (`2BR, 3BA` is not suite `2`).
pub fn suite_matches(model_name: Option<&str>, suite: Option<&str>) -> bool {
    static PREFIX: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"(?i)^(suite|ste|unit|#)\s*"));
    static NOT_ALNUM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[^A-Za-z0-9]").expect("static regex is valid"));
    static NOT_NAME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[^A-Z0-9#]+").expect("static regex is valid"));
    let (Some(model_name), Some(suite)) = (
        model_name.filter(|s| !s.is_empty()),
        suite.filter(|s| !s.is_empty()),
    ) else {
        return false;
    };
    let stripped = PREFIX.replace(py::strip(suite), "");
    let token = NOT_ALNUM.replace_all(&stripped, "").to_ascii_uppercase();
    if token.is_empty() || !token.bytes().any(|b| b.is_ascii_digit()) {
        return false;
    }
    let upper = py::upper(model_name);
    let replaced = NOT_NAME.replace_all(&upper, " ");
    let name = py::strip(&replaced);
    if name.replace(' ', "") == token {
        return true;
    }
    let pattern = format!(
        r"(?:\b(?:SUITE|STE|UNIT)|#)\s*{}\b",
        regex::escape(&token)
    );
    Regex::new(&pattern).is_ok_and(|re| re.is_match(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `test_suite_matching` (`apps/api/tests/test_sponsor_providers.py`).
    #[test]
    fn suite_matching() {
        let m = |n: &str, s: &str| suite_matches(Some(n), Some(s));
        assert!(m("Suite 110B", "110 B"));
        assert!(m("Suite 787", "787") && m("STE 300 - Medical", "300"));
        assert!(m("Unit 12", "Unit 12") && m("#4", "4"));
        assert!(!m("2BR,3BA - 2,280 SF", "2"));
        assert!(!m("Suite 1100", "110") && !m("Lobby", "Retail"));
        assert!(!suite_matches(None, Some("1")) && !suite_matches(Some("Suite 1"), None));
        assert!(!m("", "1") && !m("Suite 1", ""));
    }

    #[test]
    fn model_ids_are_checked() {
        assert!(safe_id("AAAA1111").is_ok());
        let e = safe_id("x\") { id } evil: model(id: \"y").unwrap_err();
        assert_eq!(e.kind, ProviderErrorKind::BadResponse);
        assert!(e.to_string().contains("invalid model id"), "{e}");
        assert!(safe_id("abc").is_err());
    }
}
