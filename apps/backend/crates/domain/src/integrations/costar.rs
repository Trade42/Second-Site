//! Port of `apps/api/app/integrations/costar/__init__.py`: the provider trait, `RawListing`, the
//! fixture provider (synthetic demo listings, `provider="fixture"`) and the data-kit provider
//! (genuine CoStar records delivered as a snapshot: `provider="costar"`, `FIXTURE_REAL`).
//!
//! The kit provider only reads files (JSON Lines, CSV, JSON) from a directory: no network.
//! Where Python would raise on malformed kit data (a `KeyError`, `TypeError` or `ValueError`),
//! this port returns `ProviderError { kind: BAD_RESPONSE }`.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{json, Map, Value};
use ss_core::{DataOrigin, IntegrationStatus};

use super::base::{fixtures_dir, status_entry, ProviderError, ProviderErrorKind};
use crate::contracts::{PropertySearchRequest, ProviderStatusEntry};
use crate::py;

pub const SQFT_TO_SQM: f64 = 0.09290304;
const PROVIDER: &str = "costar";

/// A provider listing before it becomes a `PropertyDigitalProfile` (Python's `RawListing`).
#[derive(Debug, Clone, PartialEq)]
pub struct RawListing {
    pub provider: String,
    pub provider_listing_id: String,
    pub data_origin: DataOrigin,
    pub address: Map<String, Value>,
    pub listing: Map<String, Value>,
    pub interior_assets: Vec<Map<String, Value>>,
    pub exterior_assets: Vec<Map<String, Value>>,
}

/// `CoStarProvider` protocol. Synchronous: implementations here are file-based.
pub trait CoStarProvider: Send + Sync {
    fn mode(&self) -> &str;
    fn search(&self, request: &PropertySearchRequest) -> Result<Vec<RawListing>, ProviderError>;
    fn status(&self) -> ProviderStatusEntry;
}

fn bad(msg: impl Into<String>) -> ProviderError {
    ProviderError::new(PROVIDER, ProviderErrorKind::BadResponse, msg)
}

/// Compare a JSON number with a threshold (`>` when `greater`, else `<`), as Python compares
/// int/float values. Non-numbers fail like Python's `TypeError`.
fn num_cmp(
    v: Option<&Value>,
    other: f64,
    greater: bool,
    what: &str,
) -> Result<bool, ProviderError> {
    let n = v
        .and_then(Value::as_f64)
        .ok_or_else(|| bad(format!("{what} is not a number")))?;
    Ok(if greater { n > other } else { n < other })
}

// ---- fixture provider ---------------------------------------------------------------------------

/// Synthetic listings for real LA addresses. `provider` is `fixture`, never `costar`.
#[derive(Debug, Clone)]
pub struct FixtureCoStarProvider {
    pub path: PathBuf,
}

impl Default for FixtureCoStarProvider {
    fn default() -> Self {
        Self {
            path: fixtures_dir().join("properties").join("la_candidates.json"),
        }
    }
}

impl FixtureCoStarProvider {
    pub const SUPPORTED_MARKETS: &'static [(&'static str, &'static str)] =
        &[("LOS ANGELES", "CA"), ("HOUSTON", "TX")];

    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_path(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

impl CoStarProvider for FixtureCoStarProvider {
    fn mode(&self) -> &str {
        "fixture"
    }

    fn search(&self, request: &PropertySearchRequest) -> Result<Vec<RawListing>, ProviderError> {
        let city = py::upper(&request.city);
        let state = py::upper(&request.state);
        if !Self::SUPPORTED_MARKETS
            .iter()
            .any(|(c, s)| *c == city && *s == state)
        {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path).map_err(|e| {
            ProviderError::new(
                PROVIDER,
                ProviderErrorKind::NotConfigured,
                format!("fixture listings {}: {e}", self.path.display()),
            )
        })?;
        let doc: Value = serde_json::from_str(&text).map_err(|e| bad(e.to_string()))?;
        let props = doc
            .get("properties")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("fixture file has no properties list"))?;
        let mut out = Vec::new();
        for p in props {
            let p = p
                .as_object()
                .ok_or_else(|| bad("property is not an object"))?;
            let p_city = p
                .get("city")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("property city missing"))?;
            if py::upper(p_city) != city {
                continue; // Houston is supported by the adapter but has no fixture listings yet.
            }
            let lst = p
                .get("listing")
                .and_then(Value::as_object)
                .ok_or_else(|| bad("property listing missing"))?;
            if let Some(max) = request.max_monthly_rent {
                if num_cmp(
                    lst.get("asking_rent_monthly"),
                    max as f64,
                    true,
                    "asking_rent_monthly",
                )? {
                    continue;
                }
            }
            if let Some(min) = request.min_area_sqm {
                if num_cmp(
                    lst.get("available_area_sqm"),
                    min,
                    false,
                    "available_area_sqm",
                )? {
                    continue;
                }
            }
            let mut address = Map::new();
            for k in [
                "street_address",
                "unit_or_suite",
                "city",
                "state",
                "postal_code",
                "parcel_id",
                "latitude",
                "longitude",
            ] {
                let v = p
                    .get(k)
                    .ok_or_else(|| bad(format!("property {k} missing")))?;
                address.insert(k.to_owned(), v.clone());
            }
            let listing_id = match lst.get("provider_listing_id") {
                Some(Value::String(s)) => s.clone(),
                _ => return Err(bad("provider_listing_id missing")),
            };
            let assets = |key: &str| -> Result<Vec<Map<String, Value>>, ProviderError> {
                match p.get(key) {
                    None => Ok(Vec::new()),
                    Some(Value::Array(a)) => a
                        .iter()
                        .map(|x| {
                            x.as_object()
                                .cloned()
                                .ok_or_else(|| bad(format!("{key} entry is not an object")))
                        })
                        .collect(),
                    Some(_) => Err(bad(format!("{key} is not a list"))),
                }
            };
            out.push(RawListing {
                provider: "fixture".into(),
                provider_listing_id: listing_id,
                data_origin: DataOrigin::FixtureSynthetic,
                address,
                listing: lst.clone(),
                interior_assets: assets("interior_assets")?,
                exterior_assets: assets("exterior_assets")?,
            });
        }
        out.truncate(usize::try_from(request.limit.max(0)).unwrap_or(0));
        Ok(out)
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            PROVIDER,
            self.mode(),
            IntegrationStatus::Mock,
            false,
            "Synthetic listings (provider='fixture'). Set PROPERTY_PROVIDER_MODE=live to use the CoStar kit.",
        )
    }
}

// ---- CoStar data kit ----------------------------------------------------------------------------

const LISTING_ACTIVE: i64 = 1;
/// Direct, building-park, shopping-center, sublease, ...
const LEASE_LISTING_TYPES: &[i64] = &[14, 16, 17, 19, 20, 21, 28];
/// MatterportScan, MatterportStreamingKey.
const MATTERPORT_REF_TYPES: &[i64] = &[36, 234];
/// Tenant pays operating expenses on top of rent.
const NET_SERVICE_TYPES: &[&str] = &["TripleNet", "Net", "DoubleNet"];

/// `MARKET_BY_STATE`: state -> (upper-case market city, kit market id).
fn market_by_state(state: &str) -> Option<(&'static str, i64)> {
    match state {
        "CA" => Some(("LOS ANGELES", 20)),
        "TX" => Some(("HOUSTON", 14)),
        _ => None,
    }
}

/// `USE_PREFERENCE`: space uses that can host a personal-services studio, best fit first.
fn use_preference(space_use: i64) -> Option<i64> {
    match space_use {
        5 => Some(0),
        2 | 4 => Some(1),
        3 => Some(2),
        1 => Some(3),
        6 => Some(4),
        _ => None,
    }
}

static BUILDOUT_PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "second_generation_salon",
            py::py_regex(r"(?i)\b(salon|barber|nail|esthetic|med\s*spa|day\s*spa)\b"),
        ),
        (
            "open_shell",
            py::py_regex(r"(?i)\b(cold|warm|vanilla)?\s*shell\b"),
        ),
        (
            "office_buildout",
            py::py_regex(
                r"(?i)\b(built[\s-]?out|2nd\s+generation|second\s+generation|turn[\s-]?key)\b",
            ),
        ),
    ]
});
static NON_DIGIT: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"\D"));

/// `_cents`: dollars (a Decimal) to integer cents, rounding half up.
fn cents(dollars: Decimal) -> Result<i64, ProviderError> {
    let c = dollars
        .checked_mul(Decimal::ONE_HUNDRED)
        .ok_or_else(|| bad("amount overflow"))?
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero);
    i64::try_from(c).map_err(|_| bad("amount overflow"))
}

/// `Decimal(str(x))` for a JSON number, or a numeric string (Python's `Decimal` accepts both).
fn decimal_any(v: &Value) -> Result<Decimal, ProviderError> {
    match v {
        Value::String(s) => {
            let t = py::strip(s);
            Decimal::from_str(t)
                .or_else(|_| Decimal::from_scientific(t))
                .map_err(|_| bad(format!("not a decimal: {}", py::str_repr(s))))
        }
        other => py::decimal_of(other).ok_or_else(|| bad("not a number")),
    }
}

fn div(a: Decimal, b: Decimal) -> Result<Decimal, ProviderError> {
    a.checked_div(b)
        .ok_or_else(|| bad("decimal division failed"))
}

fn mul(a: Decimal, b: Decimal) -> Result<Decimal, ProviderError> {
    a.checked_mul(b).ok_or_else(|| bad("decimal overflow"))
}

/// `(x or "")` followed by string use: falsy values are absent, other non-strings fail.
fn opt_str<'a>(v: Option<&'a Value>, what: &str) -> Result<Option<&'a str>, ProviderError> {
    match v {
        None => Ok(None),
        Some(v) if !py::truthy(v) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad(format!("{what} is not a string"))),
    }
}

/// `_street`.
fn street(a: &Map<String, Value>) -> Result<Option<String>, ProviderError> {
    let mut parts = Vec::new();
    for k in [
        "streetNumber",
        "streetDirection",
        "streetName",
        "streetType",
        "streetSuffix",
    ] {
        if let Some(p) = opt_str(a.get(k), k)? {
            let t = py::strip(p);
            if !t.is_empty() {
                parts.push(t);
            }
        }
    }
    let s = parts.join(" ");
    Ok((!s.is_empty()).then_some(s))
}

/// `_date`: ISO date of a timestamp string, `None` when absent or unparseable.
fn kit_date(v: Option<&Value>) -> Result<Option<String>, ProviderError> {
    match v {
        None => Ok(None),
        Some(v) if !py::truthy(v) => Ok(None),
        Some(Value::String(s)) => Ok(py::fromisoformat(s)
            .ok()
            .map(|(dt, _)| dt.date().format("%Y-%m-%d").to_string())),
        Some(_) => Err(bad("date is not a string")),
    }
}

/// `_contact`: first displayed listing contact. The kit replaces real phones with 555 numbers.
fn contact(rec: &Map<String, Value>) -> Result<Option<Value>, ProviderError> {
    let contacts = match rec.get("listingContacts") {
        Some(Value::Array(a)) => a.as_slice(),
        Some(v) if !py::truthy(v) => &[],
        None => &[],
        Some(_) => return Err(bad("listingContacts is not a list")),
    };
    for c in contacts {
        let c = c
            .as_object()
            .ok_or_else(|| bad("contact is not an object"))?;
        let phone = opt_str(c.get("phoneNumber"), "phoneNumber")?.unwrap_or("");
        let digits = NON_DIGIT.replace_all(phone, "").into_owned();
        let mut names = Vec::new();
        for k in ["firstName", "lastName"] {
            if let Some(n) = opt_str(c.get(k), k)? {
                names.push(n);
            }
        }
        let name = names.join(" ");
        if !name.is_empty() && digits.chars().count() == 10 {
            let company = match c.get("locationName") {
                Some(v) if py::truthy(v) => v.clone(),
                _ => Value::String(String::new()),
            };
            return Ok(Some(json!({
                "name": name,
                "company": company,
                "phone": format!("+1{digits}"),
                "role": "broker",
            })));
        }
    }
    Ok(None)
}

fn photo(uri: &str) -> String {
    uri.replace("/112/", "/113/")
}

/// One available lease space, flattened from a commercial listing summary.
#[derive(Debug, Clone)]
struct KitSpace {
    space_id: Value,
    property_id: Value,
    address: Map<String, Value>,
    listing: Map<String, Value>,
    use_rank: i64,
    area_sqm: f64,
    rent_monthly: Option<i64>,
    floor_plan_url: Option<String>,
    sqft: f64,
    cam_per_sf_year: Option<f64>,
}

/// Hashable stand-in for a Python dict key taken from JSON (`100`, `100.0` and `True == 1` are
/// one key; other values keep their JSON text).
fn py_key(v: &Value) -> String {
    match py::int_key(v) {
        Some(i) => format!("i:{i}"),
        None => format!("v:{v}"),
    }
}

/// PropertyId -> Matterport model ids.
pub type ModelIndex = HashMap<i64, Vec<String>>;

/// Searches available lease spaces in the extracted CoStar data kit (Los Angeles and Houston).
#[derive(Debug)]
pub struct KitCoStarProvider {
    pub kit_dir: PathBuf,
    spaces: Mutex<Option<Arc<Vec<KitSpace>>>>,
    models: Mutex<Option<Arc<ModelIndex>>>,
    skipped_no_rate: AtomicUsize,
}

impl KitCoStarProvider {
    pub const LISTINGS: &'static str = "data/commercial-listing-summary.jsonl";
    pub const MODELS_CSV: &'static str = "commercial-properties.csv";
    pub const ENUMS: &'static str = "schemas/enums.json";

    pub fn new(kit_dir: impl AsRef<Path>) -> Self {
        Self {
            kit_dir: kit_dir.as_ref().to_path_buf(),
            spaces: Mutex::new(None),
            models: Mutex::new(None),
            skipped_no_rate: AtomicUsize::new(0),
        }
    }

    /// Spaces skipped by the last search because they had no asking rent.
    pub fn skipped_no_rate(&self) -> usize {
        self.skipped_no_rate.load(Ordering::Relaxed)
    }

    pub fn available(&self) -> bool {
        self.kit_dir.join(Self::LISTINGS).is_file()
    }

    fn require(&self) -> Result<(), ProviderError> {
        if self.available() {
            Ok(())
        } else {
            Err(ProviderError::new(
                PROVIDER,
                ProviderErrorKind::NotConfigured,
                format!(
                    "CoStar data kit not found at {} (set COSTAR_KIT_DIR; see docs/PROVIDER_STATUS.md).",
                    self.kit_dir.display()
                ),
            ))
        }
    }

    fn read(&self, rel: &str) -> Result<String, ProviderError> {
        let path = self.kit_dir.join(rel);
        std::fs::read_to_string(&path).map_err(|e| bad(format!("{}: {e}", path.display())))
    }

    /// PropertyId -> Matterport model ids, from the kit's starter CSV (one row per key and model).
    pub fn matterport_models(&self) -> Result<Arc<ModelIndex>, ProviderError> {
        let mut guard = self.models.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = guard.as_ref() {
            return Ok(m.clone());
        }
        self.require()?;
        let text = self.read(Self::MODELS_CSV)?;
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(text.as_bytes());
        let headers = rdr.headers().map_err(|e| bad(e.to_string()))?.clone();
        let col = |name: &str| headers.iter().position(|h| h == name);
        let (mid_col, pid_col) = (col("MatterportModelId"), col("PropertyId"));
        let mut models = ModelIndex::new();
        for row in rdr.records() {
            let row = row.map_err(|e| bad(e.to_string()))?;
            let mid = py::strip(mid_col.and_then(|i| row.get(i)).unwrap_or("")).to_owned();
            if mid.is_empty() {
                continue;
            }
            let pid_text = pid_col
                .and_then(|i| row.get(i))
                .ok_or_else(|| bad("PropertyId missing"))?;
            let pid: i64 = py::strip(pid_text).parse().map_err(|_| {
                bad(format!(
                    "invalid literal for int() with base 10: {}",
                    py::str_repr(pid_text)
                ))
            })?;
            let list = models.entry(pid).or_default();
            if !list.contains(&mid) {
                list.push(mid);
            }
        }
        let arc = Arc::new(models);
        *guard = Some(arc.clone());
        Ok(arc)
    }

    fn load(&self) -> Result<Arc<Vec<KitSpace>>, ProviderError> {
        let mut guard = self.spaces.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        self.require()?;
        let enums: Value =
            serde_json::from_str(&self.read(Self::ENUMS)?).map_err(|e| bad(e.to_string()))?;
        let empty = Map::new();
        let service_types = enums
            .get("ServiceType")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let space_uses = enums
            .get("SpaceUse")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let path = self.kit_dir.join(Self::LISTINGS);
        let file =
            std::fs::File::open(&path).map_err(|e| bad(format!("{}: {e}", path.display())))?;
        let mut spaces = Vec::new();
        for line in std::io::BufReader::new(file).lines() {
            let line = line.map_err(|e| bad(e.to_string()))?;
            let rec: Value = serde_json::from_str(&line).map_err(|e| bad(e.to_string()))?;
            let rec = rec
                .as_object()
                .ok_or_else(|| bad("listing is not an object"))?;
            let fl = rec.get("forLeaseSummary");
            let active = rec.get("listingStatus").and_then(py::int_key) == Some(LISTING_ACTIVE);
            if !active || !fl.is_some_and(py::truthy) {
                continue;
            }
            let lt = rec.get("listingType").and_then(py::int_key);
            let mu = rec.get("monetaryUnit");
            let mu_ok = match mu {
                None | Some(Value::Null) => true,
                Some(v) => py::int_key(v) == Some(1),
            };
            if !lt.is_some_and(|t| LEASE_LISTING_TYPES.contains(&t)) || !mu_ok {
                continue;
            }
            let mut photos = Vec::new();
            if let Some(atts) = rec.get("attachments").filter(|v| py::truthy(v)) {
                let atts = atts
                    .as_array()
                    .ok_or_else(|| bad("attachments is not a list"))?;
                for a in atts {
                    let a = a
                        .as_object()
                        .ok_or_else(|| bad("attachment is not an object"))?;
                    let uri = match a.get("attachmentUri") {
                        Some(v) if py::truthy(v) => v,
                        _ => continue,
                    };
                    let is_matterport = a
                        .get("videoExternalReferenceType")
                        .and_then(py::int_key)
                        .is_some_and(|t| MATTERPORT_REF_TYPES.contains(&t));
                    if is_matterport {
                        continue;
                    }
                    let uri = uri
                        .as_str()
                        .ok_or_else(|| bad("attachmentUri is not a string"))?;
                    photos.push(photo(uri));
                }
            }
            photos.truncate(8);
            let fl = fl
                .and_then(Value::as_object)
                .ok_or_else(|| bad("forLeaseSummary is not an object"))?;
            let space_list = match fl.get("spaces") {
                Some(Value::Array(a)) => a.as_slice(),
                Some(v) if !py::truthy(v) => &[],
                None => &[],
                Some(_) => return Err(bad("spaces is not a list")),
            };
            for s in space_list {
                let s = s.as_object().ok_or_else(|| bad("space is not an object"))?;
                if let Some(sk) = Self::space(rec, s, &photos, service_types, space_uses)? {
                    spaces.push(sk);
                }
            }
        }
        Self::estimate_missing_opex(&mut spaces)?;
        let arc = Arc::new(spaces);
        *guard = Some(arc.clone());
        Ok(arc)
    }

    /// Net leases without quoted CAM would look artificially cheap next to ones that quote it.
    /// Fill them with the median CAM ($/SF/yr) quoted by net-lease spaces in the same state in
    /// this kit, flagged as an estimate (needs >= 10 quotes).
    fn estimate_missing_opex(spaces: &mut [KitSpace]) -> Result<(), ProviderError> {
        let mut quotes: Vec<(String, Vec<f64>)> = Vec::new();
        for sp in spaces.iter() {
            let net = sp
                .listing
                .get("lease_service_type")
                .and_then(Value::as_str)
                .is_some_and(|t| NET_SERVICE_TYPES.contains(&t));
            if let Some(cam) = sp.cam_per_sf_year.filter(|c| net && *c != 0.0) {
                let state = sp
                    .address
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match quotes.iter_mut().find(|(s, _)| s == state) {
                    Some((_, v)) => v.push(cam),
                    None => quotes.push((state.to_owned(), vec![cam])),
                }
            }
        }
        for (state, mut vals) in quotes {
            if vals.len() < 10 {
                continue;
            }
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = vals.len();
            let median = if n % 2 == 1 {
                vals[n / 2]
            } else {
                (vals[n / 2 - 1] + vals[n / 2]) / 2.0
            };
            let median_dec = py::decimal_of_f64(median).ok_or_else(|| bad("median not finite"))?;
            for sp in spaces.iter_mut() {
                let lst = &mut sp.listing;
                let same_state = sp.address.get("state").and_then(Value::as_str) == Some(&state);
                let net = lst
                    .get("lease_service_type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| NET_SERVICE_TYPES.contains(&t));
                let unquoted = lst.get("nnn_charges_monthly").is_none_or(Value::is_null);
                if same_state && net && unquoted {
                    let sqft = py::decimal_of_f64(sp.sqft).ok_or_else(|| bad("sqft not finite"))?;
                    let monthly = div(mul(median_dec, sqft)?, Decimal::from(12))?;
                    lst.insert("nnn_charges_monthly".into(), json!(cents(monthly)?));
                    lst.insert("nnn_is_estimate".into(), json!(true));
                    let extra = format!(
                        " Operating costs not quoted; estimated at ${}/SF/yr, the median CAM quoted by {} {} net-lease spaces in the kit.",
                        py::format_g(median),
                        n,
                        state
                    );
                    let d = lst
                        .get("disclaimer")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    lst.insert("disclaimer".into(), Value::String(d + &extra));
                }
            }
        }
        Ok(())
    }

    fn space(
        rec: &Map<String, Value>,
        s: &Map<String, Value>,
        photos: &[String],
        service_types: &Map<String, Value>,
        space_uses: &Map<String, Value>,
    ) -> Result<Option<KitSpace>, ProviderError> {
        let truthy = |k: &str| s.get(k).is_some_and(py::truthy);
        if truthy("dateOffMarket") || truthy("isLeasePending") || truthy("isInventoryOffMarket") {
            return Ok(None);
        }
        let use_value = s.get("spaceUse").cloned().unwrap_or(Value::Null);
        let use_rank = match py::int_key(&use_value).and_then(use_preference) {
            Some(r) => r,
            None => return Ok(None),
        };
        let sqft_v = match s.get("squareFeetMax").filter(|v| py::truthy(v)) {
            Some(v) => Some(v),
            None => s.get("squareFeetMin"),
        };
        let sqft_v = match sqft_v {
            Some(v) if py::truthy(v) => v.clone(),
            _ => return Ok(None),
        };
        let sqft = sqft_v
            .as_f64()
            .ok_or_else(|| bad("squareFeet is not a number"))?;
        if sqft <= 0.0 {
            return Ok(None);
        }
        let empty = Map::new();
        let addr = [s.get("marketingAddress"), rec.get("address")]
            .into_iter()
            .flatten()
            .find(|v| py::truthy(v));
        let addr = match addr {
            Some(Value::Object(o)) => o,
            Some(_) => return Err(bad("address is not an object")),
            None => &empty,
        };
        let street = street(addr)?;
        let state = py::upper(opt_str(addr.get("state"), "state")?.unwrap_or(""));
        let (street, market) = match (street, market_by_state(&state)) {
            (Some(st), Some(m)) => (st, m),
            _ => return Ok(None),
        };
        let basis = s.get("rateBasis").and_then(py::int_key);
        let rate_v = match s.get("rateLow") {
            Some(v) if py::truthy(v) => v.clone(),
            _ => json!(0),
        };
        let rate = rate_v
            .as_f64()
            .ok_or_else(|| bad("rateLow is not a number"))?;
        let months_per_period: Option<i64> = match basis {
            Some(1) => Some(1),
            Some(2) => Some(12),
            _ => None,
        };
        let rent = match months_per_period {
            Some(m) if rate > 0.0 => Some(cents(div(
                mul(decimal_any(&rate_v)?, decimal_any(&sqft_v)?)?,
                Decimal::from(m),
            )?)?),
            _ => None,
        };
        let service_key = py::value_str(s.get("serviceType").unwrap_or(&Value::Null));
        let service = service_types
            .get(&service_key)
            .cloned()
            .unwrap_or_else(|| json!("Unknown"));
        let service_str = service.as_str().unwrap_or("").to_owned();
        let cam = s
            .get("commonAreaMaintenanceAmount")
            .filter(|v| py::truthy(v));
        let nnn = match (cam, months_per_period) {
            (Some(c), Some(m)) => Some(cents(div(
                mul(decimal_any(c)?, decimal_any(&sqft_v)?)?,
                Decimal::from(m),
            )?)?),
            _ => {
                if service_str == "FullService" || service_str == "FullServiceGrossNoPass" {
                    Some(0)
                } else {
                    None
                }
            }
        };
        let term = if truthy("leaseTermLow") {
            let mult = match s.get("leaseTermType").and_then(py::int_key) {
                Some(1) => Some(12),
                Some(2) => Some(1),
                _ => None,
            };
            match (mult, s.get("leaseTermLow")) {
                (Some(m), Some(low)) => {
                    if let Some(i) = low.as_i64() {
                        json!(i.checked_mul(m).ok_or_else(|| bad("lease term overflow"))?)
                    } else {
                        let f = low
                            .as_f64()
                            .ok_or_else(|| bad("leaseTermLow is not a number"))?
                            * m as f64;
                        // Pydantic accepts a float with no fractional part for an int field.
                        if f.fract() == 0.0 && f.abs() < 9.0e15 {
                            json!(f as i64)
                        } else {
                            json!(f)
                        }
                    }
                }
                _ => Value::Null,
            }
        } else {
            Value::Null
        };
        let mut notes = Vec::new();
        match s.get("notes") {
            Some(Value::Array(a)) => {
                for n in a {
                    let n = n.as_object().ok_or_else(|| bad("note is not an object"))?;
                    notes.push(opt_str(n.get("value"), "note")?.unwrap_or("").to_owned());
                }
            }
            Some(v) if py::truthy(v) => return Err(bad("notes is not a list")),
            _ => {}
        }
        let notes = notes.join(" ");
        let buildout = BUILDOUT_PATTERNS
            .iter()
            .find(|(_, pat)| pat.is_match(&notes))
            .map(|(k, _)| *k);
        let suite = opt_str(s.get("suite"), "suite")?
            .map(py::strip)
            .filter(|x| !x.is_empty())
            .map(str::to_owned);
        let use_name = space_uses
            .get(&py::value_str(&use_value))
            .cloned()
            .unwrap_or_else(|| json!("Unknown"));
        let rate_note = if rent.is_some() {
            format!(
                "asking ${}/SF/{} ({})",
                py::format_g(rate),
                if basis == Some(1) { "mo" } else { "yr" },
                py::value_str(&service)
            )
        } else {
            "no asking rate recorded (call for rate)".to_owned()
        };
        let certified = kit_date(rec.get("certifiedFreshDate"))?;
        let mut disclaimer = format!(
            "CoStar data kit snapshot (certified {}); {rate_note}. Asking terms, not achieved rent.",
            certified.as_deref().unwrap_or("n/a")
        );
        if let Some(b) = buildout {
            disclaimer.push_str(&format!(" Build-out '{b}' inferred from listing notes."));
        }
        let floor_plan = match s.get("floorPlan") {
            Some(Value::Object(fp)) => opt_str(fp.get("attachmentUri"), "floorPlan")?,
            Some(v) if py::truthy(v) => return Err(bad("floorPlan is not an object")),
            _ => None,
        };
        let rec_id = rec.get("id").ok_or_else(|| bad("listing id missing"))?;
        let space_id = s.get("id").ok_or_else(|| bad("space id missing"))?;
        let area_sqm = py::round(sqft * SQFT_TO_SQM, 1);
        let mut listing = Map::new();
        listing.insert("provider".into(), json!("costar"));
        listing.insert(
            "provider_listing_id".into(),
            json!(format!(
                "{}:{}",
                py::value_str(rec_id),
                py::value_str(space_id)
            )),
        );
        listing.insert("listing_type".into(), json!("LEASE"));
        listing.insert("property_type".into(), use_name);
        listing.insert("available_area_sqm".into(), json!(area_sqm));
        listing.insert("asking_rent_monthly".into(), json!(rent));
        listing.insert("nnn_charges_monthly".into(), json!(nnn));
        listing.insert("nnn_is_estimate".into(), json!(false));
        listing.insert("lease_term_months".into(), term);
        listing.insert(
            "available_from".into(),
            json!(kit_date(s.get("availableOnDate"))?),
        );
        listing.insert("existing_buildout".into(), json!(buildout));
        listing.insert("lease_service_type".into(), service);
        listing.insert(
            "building_name".into(),
            rec.get("buildingName").cloned().unwrap_or(Value::Null),
        );
        listing.insert(
            "brokerage_company".into(),
            rec.get("brokerageCompanyName")
                .cloned()
                .unwrap_or(Value::Null),
        );
        listing.insert(
            "broker_contact".into(),
            contact(rec)?.unwrap_or(Value::Null),
        );
        listing.insert("photo_urls".into(), json!(photos));
        listing.insert("source_record_date".into(), json!(certified));
        listing.insert("disclaimer".into(), json!(disclaimer));

        let city = opt_str(addr.get("city"), "city")?
            .map(py::strip)
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| py::title(market.0));
        let postcode = opt_str(addr.get("postcode"), "postcode")?.unwrap_or("");
        let postal = py::prefix_chars(postcode, 5);
        let mut address = Map::new();
        address.insert("street_address".into(), json!(street));
        address.insert("unit_or_suite".into(), json!(suite));
        address.insert("city".into(), json!(city));
        address.insert("state".into(), json!(state));
        address.insert(
            "postal_code".into(),
            if postal.is_empty() {
                Value::Null
            } else {
                json!(postal)
            },
        );
        address.insert("parcel_id".into(), Value::Null);
        address.insert(
            "latitude".into(),
            addr.get("latitude").cloned().unwrap_or(Value::Null),
        );
        address.insert(
            "longitude".into(),
            addr.get("longitude").cloned().unwrap_or(Value::Null),
        );
        let property_id = match s.get("propertyId").filter(|v| py::truthy(v)) {
            Some(v) => v.clone(),
            None => rec.get("propertyId").cloned().unwrap_or(Value::Null),
        };
        let cam_per_sf_year = match (cam, months_per_period) {
            (Some(c), Some(_)) => {
                let f = match c {
                    Value::String(t) => py::strip(t)
                        .parse::<f64>()
                        .map_err(|_| bad("CAM is not a number"))?,
                    other => other.as_f64().ok_or_else(|| bad("CAM is not a number"))?,
                };
                Some(f * if basis == Some(1) { 12.0 } else { 1.0 })
            }
            _ => None,
        };
        Ok(Some(KitSpace {
            space_id: space_id.clone(),
            property_id,
            address,
            listing,
            use_rank,
            area_sqm,
            rent_monthly: rent,
            floor_plan_url: floor_plan.map(photo),
            sqft,
            cam_per_sf_year,
        }))
    }
}

impl CoStarProvider for KitCoStarProvider {
    fn mode(&self) -> &str {
        "live"
    }

    fn search(&self, request: &PropertySearchRequest) -> Result<Vec<RawListing>, ProviderError> {
        let state = py::upper(&request.state);
        let market = match market_by_state(&state) {
            Some(m) => m,
            None => return Ok(Vec::new()),
        };
        let req_city = py::upper(py::strip(&request.city));
        let whole_market = req_city == market.0;
        let target = request.min_area_sqm;
        self.skipped_no_rate.store(0, Ordering::Relaxed);
        let spaces = self.load()?;
        let mut cands: Vec<&KitSpace> = Vec::new();
        for sp in spaces.iter() {
            let sp_state = sp
                .address
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("");
            if sp_state != state {
                continue;
            }
            let sp_city = py::upper(sp.address.get("city").and_then(Value::as_str).unwrap_or(""));
            if !whole_market && sp_city != req_city {
                continue;
            }
            if !request.neighborhoods.is_empty()
                && !request
                    .neighborhoods
                    .iter()
                    .any(|n| sp_city.contains(&py::upper(n)))
            {
                continue;
            }
            if let Some(t) = target {
                if sp.area_sqm < t || sp.area_sqm > t * 2.5 {
                    continue;
                }
            }
            let rent = match sp.rent_monthly {
                Some(r) => r,
                None => {
                    // Cannot rank total cost without an asking rent.
                    self.skipped_no_rate.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            if request.max_monthly_rent.is_some_and(|max| rent > max) {
                continue;
            }
            cands.push(sp);
        }
        let dist = |sp: &KitSpace| {
            let t = match target {
                Some(t) if t != 0.0 => t,
                _ => sp.area_sqm,
            };
            (sp.area_sqm - t).abs()
        };
        cands.sort_by(|a, b| {
            (a.use_rank, a.rent_monthly)
                .cmp(&(b.use_rank, b.rent_monthly))
                .then(
                    dist(a)
                        .partial_cmp(&dist(b))
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        let models = self.matterport_models()?;
        let mut out = Vec::new();
        let mut per_property: HashMap<String, usize> = HashMap::new();
        let limit = usize::try_from(request.limit.max(0)).unwrap_or(0);
        for sp in cands {
            let count = per_property.entry(py_key(&sp.property_id)).or_insert(0);
            if *count >= 2 {
                continue; // keep the shortlist diverse across buildings
            }
            *count += 1;
            let mut interior: Vec<Map<String, Value>> = py::int_key(&sp.property_id)
                .and_then(|pid| models.get(&pid))
                .map(|mids| {
                    mids.iter()
                        .map(|mid| {
                            let mut m = Map::new();
                            m.insert("provider".into(), json!("matterport"));
                            m.insert("asset_id".into(), json!(mid));
                            m.insert("asset_type".into(), json!("matterport_model"));
                            // Set true only after the Model API returns floor geometry.
                            m.insert("has_measured_geometry".into(), json!(false));
                            m.insert(
                                "url".into(),
                                json!(format!("https://my.matterport.com/show/?m={mid}")),
                            );
                            m
                        })
                        .collect()
                })
                .unwrap_or_default();
            if let Some(url) = &sp.floor_plan_url {
                let mut m = Map::new();
                m.insert("provider".into(), json!("costar"));
                m.insert(
                    "asset_id".into(),
                    json!(format!("floorplan-{}", py::value_str(&sp.space_id))),
                );
                m.insert("asset_type".into(), json!("floor_plan_image"));
                m.insert("url".into(), json!(url));
                interior.push(m);
            }
            let mut listing = sp.listing.clone();
            listing.insert("costar_property_id".into(), sp.property_id.clone());
            let id = listing
                .get("provider_listing_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            out.push(RawListing {
                provider: "costar".into(),
                provider_listing_id: id,
                data_origin: DataOrigin::FixtureReal,
                address: sp.address.clone(),
                listing,
                interior_assets: interior,
                exterior_assets: Vec::new(),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    fn status(&self) -> ProviderStatusEntry {
        let ok = self.available();
        status_entry(
            PROVIDER,
            self.mode(),
            if ok {
                IntegrationStatus::Live
            } else {
                IntegrationStatus::Blocked
            },
            ok,
            if ok {
                "CoStar hackathon data kit (snapshot of genuine records) read from COSTAR_KIT_DIR."
            } else {
                "CoStar data kit not extracted; set COSTAR_KIT_DIR."
            },
        )
    }
}
