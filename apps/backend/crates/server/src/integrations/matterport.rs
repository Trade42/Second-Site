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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use base64::Engine as _;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};
use ss_core::IntegrationStatus;
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{status_entry, ProviderError, ProviderErrorKind, RetryPolicy};
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
    /// True when an earlier [`Self::get_floor_geometry`] for `asset_id` answered `None` because
    /// the provider does not know the model (or this token cannot see it), as opposed to a model
    /// without a usable floor. Only the live provider can tell; everything else says `false`.
    fn model_not_found(&self, _asset_id: &str) -> bool {
        false
    }
    /// Summaries keyed by model id, in request order; inaccessible models are left out.
    fn model_summaries(
        &self,
        model_ids: &[String],
    ) -> Result<IndexMap<String, ModelSummary>, ProviderError>;
    fn status(&self) -> ProviderStatusEntry;

    /// True when this provider can serve interior media ([`Self::model_media`],
    /// [`Self::download_media`]): only the live provider with credentials. Interior media is a
    /// deliberate addition, not a port: the Python backend has no such methods.
    fn media_enabled(&self) -> bool {
        false
    }
    /// The signed media links of one model, or `None` when the API does not know the model or
    /// this token cannot see it (`not.found`). `fresh` skips the short-lived link cache.
    fn model_media(
        &self,
        _asset_id: &str,
        _fresh: bool,
    ) -> Result<Option<ModelMedia>, ProviderError> {
        Err(media_off())
    }
    /// One signed link's bytes, fetched with **no** credentials.
    fn download_media(&self, _url: &str) -> Result<Vec<u8>, ProviderError> {
        Err(media_off())
    }
}

fn media_off() -> ProviderError {
    ProviderError::new(
        PROVIDER,
        ProviderErrorKind::NotConfigured,
        "live Matterport is not switched on",
    )
}

// ---- interior media (a deliberate addition, not a port) -----------------------------------------

/// The one query behind the interior media route: the model name, the model still, the mesh
/// entries, the 2D photos at the `web` resolution and the floor plans. Nothing else is asked for.
pub const MEDIA_QUERY: &str = "
query ModelMedia($id: ID!) {
  model(id: $id) {
    id
    name
    image { status resizeUrl(resolution: web) }
    assets {
      meshes { format resolution status filename url }
      photos { type status resizeUrl(resolution: web) }
      floorplans { format flags width height resolution origin { x y } status url }
    }
  }
}
";

/// Signed links live about five minutes (`validUntil` was about 5 min after the query on
/// 2026-11-15); a cached link set is reused for at most this long.
pub const MEDIA_LINK_TTL: std::time::Duration = std::time::Duration::from_secs(240);

/// The floor plan the media route serves: a PNG at about 100 px/m, the `alpha` one preferred.
#[derive(Clone, PartialEq)]
pub struct FloorplanLink {
    pub url: String,
    pub resolution_px_per_m: Option<f64>,
    /// Model coordinates of the image origin, metres.
    pub origin: Option<(f64, f64)>,
    pub width_px: Option<i64>,
    pub height_px: Option<i64>,
    pub alpha: bool,
}

/// What one model offers to the media route. Every `url` is a signed, short-lived link: it is
/// never logged, returned or put in an error (`Debug` leaves it out).
#[derive(Clone, PartialEq, Default)]
pub struct ModelMedia {
    pub name: Option<String>,
    /// The `obj` mesh at resolution `50k` (the `500k` entry is one chunk of a chunked mesh).
    pub mesh_url: Option<String>,
    /// `photo2D` stills at the `web` resolution, in the API's order.
    pub photo_urls: Vec<String>,
    /// The model's own still, `web` resolution.
    pub image_url: Option<String>,
    pub floorplan: Option<FloorplanLink>,
}

impl std::fmt::Debug for ModelMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelMedia")
            .field("name", &self.name)
            .field("mesh", &self.mesh_url.is_some())
            .field("photos", &self.photo_urls.len())
            .field("image", &self.image_url.is_some())
            .field("floorplan", &self.floorplan.is_some())
            .finish()
    }
}

fn available(v: &Value) -> bool {
    matches!(v.get("status"), None | Some(Value::Null))
        || v.get("status").and_then(Value::as_str) == Some("available")
}

fn url_of<'a>(v: &'a Value, field: &str) -> Option<&'a str> {
    v.get(field)
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
}

fn list(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// [`ModelMedia`] from the `model` object of [`MEDIA_QUERY`]'s answer.
pub fn media_from_model(model: &Map<String, Value>) -> ModelMedia {
    let name = model
        .get("name")
        .and_then(Value::as_str)
        .map(|s| py::strip(s).to_owned())
        .filter(|s| !s.is_empty());
    let image_url = model
        .get("image")
        .filter(|i| available(i))
        .and_then(|i| url_of(i, "resizeUrl"))
        .map(str::to_owned);
    let assets = model.get("assets");
    let mesh_url = list(assets.and_then(|a| a.get("meshes")))
        .iter()
        .filter(|m| available(m))
        .filter(|m| m.get("format").and_then(Value::as_str) == Some("obj"))
        .find(|m| m.get("resolution").and_then(Value::as_str) == Some("50k"))
        .and_then(|m| url_of(m, "url"))
        .map(str::to_owned);
    let photo_urls = list(assets.and_then(|a| a.get("photos")))
        .iter()
        .filter(|p| available(p))
        .filter(|p| {
            matches!(
                p.get("type").and_then(Value::as_str),
                None | Some("photo2D")
            )
        })
        .filter_map(|p| url_of(p, "resizeUrl"))
        .map(str::to_owned)
        .collect();
    let plans: Vec<FloorplanLink> = list(assets.and_then(|a| a.get("floorplans")))
        .iter()
        .filter(|f| available(f))
        .filter(|f| f.get("format").and_then(Value::as_str) == Some("png"))
        .filter(|f| {
            f.get("resolution")
                .and_then(Value::as_f64)
                .is_some_and(|r| (r - 100.0).abs() < 0.5)
        })
        .filter_map(|f| {
            let url = url_of(f, "url")?.to_owned();
            let origin = f
                .get("origin")
                .and_then(|o| Some((o.get("x")?.as_f64()?, o.get("y")?.as_f64()?)));
            Some(FloorplanLink {
                url,
                resolution_px_per_m: f.get("resolution").and_then(Value::as_f64),
                origin,
                width_px: f.get("width").and_then(Value::as_i64),
                height_px: f.get("height").and_then(Value::as_i64),
                alpha: list(f.get("flags"))
                    .iter()
                    .any(|x| x.as_str() == Some("alpha")),
            })
        })
        .collect();
    let floorplan = plans.iter().find(|p| p.alpha).or(plans.first()).cloned();
    ModelMedia {
        name,
        mesh_url,
        photo_urls,
        image_url,
        floorplan,
    }
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
        let not_configured =
            |m: String| ProviderError::new(PROVIDER, ProviderErrorKind::NotConfigured, m);
        let entries = std::fs::read_dir(&self.geometry_dir).map_err(|e| {
            not_configured(format!(
                "geometry fixtures {}: {e}",
                self.geometry_dir.display()
            ))
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
    /// Model ids the API answered `not.found` for (their cached geometry is `None`).
    not_found: Mutex<HashSet<String>>,
    /// Signed-link downloads: same retry policy, **no** headers (the links are signed; our
    /// credentials never go to the CDN host).
    cdn: HttpClient,
    /// [`MatterportProvider::model_media`] answers, for at most [`MEDIA_LINK_TTL`].
    media_cache: Mutex<HashMap<String, (std::time::Instant, Option<ModelMedia>)>>,
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
            not_found: Mutex::new(HashSet::new()),
            cdn: HttpClient::new(PROVIDER, policy, Vec::new()),
            media_cache: Mutex::new(HashMap::new()),
        }
    }

    /// `_gql`: the `data` object of a GraphQL response.
    pub fn gql(
        &self,
        query: &str,
        variables: Option<Value>,
    ) -> Result<Map<String, Value>, ProviderError> {
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

/// The marker of the API's answer for a model it does not know, or that this token cannot see,
/// as observed on 2026-11-15: HTTP 2xx, `data: null`, and one GraphQL error whose message is
/// `Error fetching data (path: /model, code: not.found): Not Found`. Only the message text is
/// known (it is what [`LiveMatterportProvider::gql`] keeps of each error); the raw error object
/// was not captured, so nothing else of it (`path`, `extensions`) is relied on.
const MODEL_NOT_FOUND: &str = "(path: /model, code: not.found)";

/// True when every error message `gql` joined into `e` is the `not.found` of the `model` path
/// (a `not.found` on any other path, or any other error alongside it, is not).
fn is_model_not_found(e: &ProviderError) -> bool {
    e.provider == PROVIDER
        && e.kind == ProviderErrorKind::BadResponse
        && e.message.split("; ").all(|m| m.contains(MODEL_NOT_FOUND))
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

    /// A model the API does not know (or this token cannot see) is `None`, cached like any other
    /// `None` and remembered for [`MatterportProvider::model_not_found`], not a provider error:
    /// the trait documents `None` for it, and the layout then tries the property's next asset
    /// or answers `GEOMETRY_UNAVAILABLE`. Deliberately different from Python, whose `_gql`
    /// raises `BAD_RESPONSE` here and fails the layout task (R1 of
    /// `orchestrator/reports/live-2.md`). Every other error stays an error: authentication,
    /// rate limit, unreachable, a malformed answer, and a `not.found` on any other path.
    /// `model_summaries` is unchanged.
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
        let data = match self.gql(GEOMETRY_QUERY, Some(json!({"id": id}))) {
            Ok(data) => data,
            Err(e) if is_model_not_found(&e) => {
                self.not_found
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(asset_id.to_owned());
                self.geometry_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(asset_id.to_owned(), None);
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
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

    fn media_enabled(&self) -> bool {
        self.configured
    }

    /// One [`MEDIA_QUERY`]; a `not.found` of the model is `None` (cached like an answer).
    fn model_media(
        &self,
        asset_id: &str,
        fresh: bool,
    ) -> Result<Option<ModelMedia>, ProviderError> {
        let id = safe_id(asset_id)?;
        if !fresh {
            let cache = self.media_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, hit)) = cache.get(id) {
                if at.elapsed() < MEDIA_LINK_TTL {
                    return Ok(hit.clone());
                }
            }
        }
        let media = match self.gql(MEDIA_QUERY, Some(json!({"id": id}))) {
            Ok(data) => match data.get("model") {
                Some(Value::Object(model)) if !model.is_empty() => Some(media_from_model(model)),
                Some(v) if py::truthy(v) => return Err(bad("model is not an object")),
                _ => None,
            },
            Err(e) if is_model_not_found(&e) => None,
            Err(e) => return Err(e),
        };
        self.media_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned(), (std::time::Instant::now(), media.clone()));
        Ok(media)
    }

    fn download_media(&self, url: &str) -> Result<Vec<u8>, ProviderError> {
        self.cdn
            .with_retries(|| self.cdn.get_once(url, &[]))
            .map(|r| r.body)
    }

    fn model_not_found(&self, asset_id: &str) -> bool {
        self.not_found
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(asset_id)
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
    let pattern = format!(r"(?:\b(?:SUITE|STE|UNIT)|#)\s*{}\b", regex::escape(&token));
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
