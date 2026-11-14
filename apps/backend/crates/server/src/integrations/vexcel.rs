//! Port of `apps/api/app/integrations/vexcel/__init__.py`.
//!
//! Live: Vexcel API 2.0 (`VEXCEL_BASE_URL`), `X-API-KEY` header, retry policy 2 attempts / 45 s:
//! - `GET /ortho/extract` (layer, wkt, bbox-dimensions, destination-dimensions, image-format)
//!   -> JPEG;
//! - `GET /ortho/collections` (wkt, include) -> GeoJSON coverage with capture dates;
//! - `GET /property/extract` (wkt, total-records) -> GeoJSON parcel and structure elements.
//!
//! Element values are model detections with scores: visual evidence, never verified facts and
//! never measured capacity. The key stays on the server: browsers load imagery through
//! `/api/v1/media/vexcel/ortho/{property_id}`.
//!
//! Differences from Python: a response body that is not JSON is a BAD_RESPONSE provider error
//! (Python lets the `JSONDecodeError` escape, because it parses outside `with_retries`). The
//! response cache is bounded (Python's grows without limit).

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{Map, Value};
use ss_core::IntegrationStatus;
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{
    status_entry, ProviderError, ProviderErrorKind, RetryPolicy,
};
use ss_domain::py;

use super::http::HttpClient;

pub const PROVIDER: &str = "vexcel";

/// Default `size_m` and `pixels` of `ortho_image`.
pub const ORTHO_SIZE_M: i64 = 80;
pub const ORTHO_PIXELS: i64 = 600;

/// Cached responses kept at most (then the cache starts over).
const CACHE_LIMIT: usize = 512;

pub trait VexcelProvider: Send + Sync {
    fn mode(&self) -> &str;
    /// A JPEG of `size_m` x `size_m` meters around the point, or `None` (fixture mode).
    fn ortho_image(
        &self,
        lat: f64,
        lon: f64,
        size_m: i64,
        pixels: i64,
    ) -> Result<Option<Vec<u8>>, ProviderError>;
    /// `properties` of each ortho collection covering the point.
    fn capture_dates(&self, lat: f64, lon: f64) -> Result<Vec<Map<String, Value>>, ProviderError>;
    /// [`summarize_property`] of the first parcel at the point, or `None`.
    fn property_elements(&self, lat: f64, lon: f64)
        -> Result<Option<Map<String, Value>>, ProviderError>;
    fn status(&self) -> ProviderStatusEntry;
}

fn bad(msg: impl Into<String>) -> ProviderError {
    ProviderError::new(PROVIDER, ProviderErrorKind::BadResponse, msg)
}

/// `_wkt_point`: `POINT(lon lat)` with 7 decimals; coordinates out of range are refused before
/// any request.
pub fn wkt_point(lat: f64, lon: f64) -> Result<String, ProviderError> {
    if !((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)) {
        return Err(bad("coordinates out of range"));
    }
    Ok(format!("POINT({lon:.7} {lat:.7})"))
}

// ---- fixture ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct FixtureVexcelProvider;

impl VexcelProvider for FixtureVexcelProvider {
    fn mode(&self) -> &str {
        "fixture"
    }

    fn ortho_image(
        &self,
        _lat: f64,
        _lon: f64,
        _size_m: i64,
        _pixels: i64,
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        Ok(None)
    }

    fn capture_dates(&self, _lat: f64, _lon: f64) -> Result<Vec<Map<String, Value>>, ProviderError> {
        Ok(Vec::new())
    }

    fn property_elements(
        &self,
        _lat: f64,
        _lon: f64,
    ) -> Result<Option<Map<String, Value>>, ProviderError> {
        Ok(None)
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            PROVIDER,
            self.mode(),
            IntegrationStatus::Mock,
            false,
            "No imagery in fixture mode. Set VEXCEL_PROVIDER_MODE=live with VEXCEL_API_KEY.",
        )
    }
}

// ---- live ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CacheKey {
    Ortho(u64, u64, i64, i64),
    Property(u64, u64),
}

#[derive(Debug, Clone)]
enum Cached {
    Image(Vec<u8>),
    Elements(Option<Map<String, Value>>),
}

pub struct LiveVexcelProvider {
    base_url: String,
    configured: bool,
    client: HttpClient,
    cache: Mutex<HashMap<CacheKey, Cached>>,
}

impl std::fmt::Debug for LiveVexcelProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveVexcelProvider")
            .field("base_url", &self.base_url)
            .field("configured", &self.configured)
            .finish()
    }
}

/// Python's `RetryPolicy(attempts=2, timeout_s=45.0)`.
pub fn default_policy() -> RetryPolicy {
    RetryPolicy {
        attempts: 2,
        timeout_s: 45.0,
        ..RetryPolicy::default()
    }
}

/// `round(x, 6)` as a hashable cache key part.
fn key6(x: f64) -> u64 {
    py::round(x, 6).to_bits()
}

impl LiveVexcelProvider {
    pub fn new(base_url: &str, api_key: Option<&str>) -> Self {
        Self::with_policy(base_url, api_key, default_policy())
    }

    pub fn with_policy(base_url: &str, api_key: Option<&str>, policy: RetryPolicy) -> Self {
        let key = api_key.unwrap_or("");
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            configured: !key.is_empty(),
            client: HttpClient::new(
                PROVIDER,
                policy,
                vec![("X-API-KEY".to_owned(), key.to_owned())],
            ),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, key: &CacheKey) -> Option<Cached> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    fn remember(&self, key: CacheKey, value: Cached) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, value);
    }

    /// `_get`: one GET with retries.
    fn get(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<super::http::HttpResponse, ProviderError> {
        if !self.configured {
            return Err(ProviderError::new(
                PROVIDER,
                ProviderErrorKind::NotConfigured,
                "VEXCEL_API_KEY not set",
            ));
        }
        let url = format!("{}{path}", self.base_url);
        self.client
            .with_retries(|| self.client.get_once(&url, params))
    }

    fn features(&self, resp: &super::http::HttpResponse) -> Result<Vec<Value>, ProviderError> {
        match resp.json(PROVIDER)? {
            Value::Object(m) => match m.get("features") {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Array(a)) => Ok(a.clone()),
                Some(v) if !py::truthy(v) => Ok(Vec::new()),
                Some(_) => Err(bad("features is not a list")),
            },
            _ => Err(bad("response is not a JSON object")),
        }
    }
}

/// `f.get("properties") or {}` of one GeoJSON feature.
fn feature_properties(f: &Value) -> Result<Map<String, Value>, ProviderError> {
    match f {
        Value::Object(m) => match m.get("properties") {
            Some(Value::Object(p)) => Ok(p.clone()),
            None | Some(Value::Null) => Ok(Map::new()),
            Some(v) if !py::truthy(v) => Ok(Map::new()),
            Some(_) => Err(bad("feature properties is not an object")),
        },
        _ => Err(bad("feature is not an object")),
    }
}

impl VexcelProvider for LiveVexcelProvider {
    fn mode(&self) -> &str {
        "live"
    }

    fn ortho_image(
        &self,
        lat: f64,
        lon: f64,
        size_m: i64,
        pixels: i64,
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        let key = CacheKey::Ortho(key6(lat), key6(lon), size_m, pixels);
        if let Some(Cached::Image(img)) = self.cached(&key) {
            return Ok(Some(img));
        }
        let wkt = wkt_point(lat, lon)?;
        let bbox = format!("{size_m},{size_m}");
        let dest = format!("{pixels},{pixels}");
        let r = self.get(
            "/ortho/extract",
            &[
                ("layer", "urban"),
                ("wkt", &wkt),
                ("bbox-dimensions", &bbox),
                ("destination-dimensions", &dest),
                ("image-format", "jpeg"),
            ],
        )?;
        if !r.content_type.starts_with("image/") {
            return Err(bad("ortho/extract did not return an image"));
        }
        self.remember(key, Cached::Image(r.body.clone()));
        Ok(Some(r.body))
    }

    fn capture_dates(&self, lat: f64, lon: f64) -> Result<Vec<Map<String, Value>>, ProviderError> {
        let wkt = wkt_point(lat, lon)?;
        let r = self.get(
            "/ortho/collections",
            &[
                ("wkt", &wkt),
                ("include", "layer,collection,first-capture-date,last-capture-date"),
            ],
        )?;
        self.features(&r)?.iter().map(feature_properties).collect()
    }

    fn property_elements(
        &self,
        lat: f64,
        lon: f64,
    ) -> Result<Option<Map<String, Value>>, ProviderError> {
        let key = CacheKey::Property(key6(lat), key6(lon));
        if let Some(Cached::Elements(e)) = self.cached(&key) {
            return Ok(e);
        }
        let wkt = wkt_point(lat, lon)?;
        let r = self.get("/property/extract", &[("wkt", &wkt), ("total-records", "1")])?;
        let feats = self.features(&r)?;
        let out = match feats.first() {
            Some(f) => Some(summarize_property(&feature_properties(f)?)),
            None => None,
        };
        self.remember(key, Cached::Elements(out.clone()));
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
                "Vexcel API 2.0: orthomosaic imagery, capture dates, roof/structure elements."
            } else {
                "VEXCEL_API_KEY missing."
            },
        )
    }
}

/// `summarize_property`: the primary structure's elements we use, keyed as Python names them.
/// Values are copied as the API sent them (`None` when absent).
pub fn summarize_property(props: &Map<String, Value>) -> Map<String, Value> {
    let structures: Vec<&Value> = match props.get("structures") {
        Some(Value::Array(a)) => a.iter().collect(),
        _ => Vec::new(),
    };
    let empty = Value::Object(Map::new());
    let primary = structures
        .iter()
        .copied()
        .find(|s| s.get("primary-structure").is_some_and(py::truthy))
        .or_else(|| structures.first().copied())
        .unwrap_or(&empty);
    let s = |k: &str| primary.get(k).cloned().unwrap_or(Value::Null);
    let p = |k: &str| props.get(k).cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("footprint_area_sqm".into(), s("footprint.area"));
    out.insert("roof_condition".into(), s("roof-characteristics.condition"));
    out.insert("roof_material".into(), s("roof-characteristics.material.class"));
    out.insert(
        "roof_material_score".into(),
        s("roof-characteristics.material.score"),
    );
    out.insert("roof_shape".into(), s("roof-characteristics.shape.value"));
    out.insert("rooftop_ac_units".into(), s("roof-objects.ac-unit.count"));
    out.insert("solar".into(), s("roof-objects.solar.value"));
    out.insert(
        "structural_damage_detected".into(),
        s("roof-defects.structural-damage.detected"),
    );
    out.insert(
        "water_pooling_detected".into(),
        s("roof-defects.discoloration.types.water-pooling.detected"),
    );
    out.insert("building_type".into(), s("building-type.class"));
    out.insert("building_type_score".into(), s("building-type.score"));
    out.insert("structure_distance_m".into(), s("structure-distance"));
    out.insert(
        "inside_parcel".into(),
        p("request-information.inside-parcel"),
    );
    out.insert("image_date".into(), p("metadata.ortho-date-estimate"));
    out.insert("collection".into(), p("metadata.collection"));
    out.insert("gsd_m".into(), p("metadata.gsd"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wkt_point_format_and_range() {
        assert_eq!(
            wkt_point(34.05, -118.25).unwrap(),
            "POINT(-118.2500000 34.0500000)"
        );
        assert!(wkt_point(91.0, 0.0).is_err());
        assert!(wkt_point(0.0, -181.0).is_err());
    }
}
