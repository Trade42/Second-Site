//! Port of `apps/api/app/contracts/common.py`.
//!
//! Conventions: internal ids are UUIDs; timestamps are timezone-aware UTC; money is integer minor
//! units plus an ISO 4217 code, never floats; geometry is meters; unknown values are `None` or an
//! explicit UNKNOWN member, never a guessed default.

use std::fmt;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Timelike, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::string_enum;
use crate::validation::{Checker, Contract, Validate};

/// `common.SCHEMA_VERSION`.
pub const SCHEMA_VERSION: &str = "1.0";

/// Integer minor currency units (e.g. cents). Negative values only where a field says so.
pub type MinorUnits = i64;
/// Non-negative integer minor currency units; the `>= 0` rule is checked by the owning contract.
pub type NonNegativeMinor = i64;

/// `common.new_id()`: a random (v4) UUID.
pub fn new_id() -> Uuid {
    Uuid::new_v4()
}

/// `common.utcnow()`.
pub fn utcnow() -> UtcDateTime {
    UtcDateTime::now()
}

pub(crate) fn default_schema_version() -> String {
    SCHEMA_VERSION.to_owned()
}

// ---- UtcDatetime ------------------------------------------------------------------------------

/// A timezone-aware instant normalized to UTC (`common.UtcDatetime`).
///
/// - Parsing requires an explicit offset (`Z` or `±HH:MM`); a naive timestamp is rejected with
///   Pydantic's message "timestamps must be timezone-aware (UTC)". Other offsets convert to UTC.
/// - Precision is microseconds, like Python's `datetime`; finer input digits are truncated.
/// - Serialization matches Pydantic: `YYYY-MM-DDTHH:MM:SSZ`, or with `.ffffff` (always six
///   digits) when the microsecond part is non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcDateTime(DateTime<Utc>);

pub const NAIVE_TIMESTAMP_MESSAGE: &str = "timestamps must be timezone-aware (UTC)";

impl UtcDateTime {
    pub fn now() -> Self {
        Self::from_datetime(Utc::now())
    }

    /// Wrap a chrono instant, truncating to microseconds.
    pub fn from_datetime(dt: DateTime<Utc>) -> Self {
        let micros = dt.nanosecond() / 1_000 * 1_000;
        Self(dt.with_nanosecond(micros).unwrap_or(dt))
    }

    pub fn as_datetime(&self) -> DateTime<Utc> {
        self.0
    }

    /// Parse an ISO 8601 / RFC 3339 timestamp with an explicit offset.
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
            // chrono accepts second 60 as a leap second; Pydantic rejects it, and the serialized
            // form (`...:60.000000Z`) would not parse in Python.
            if dt.nanosecond() >= 1_000_000_000 {
                return Err(format!("input is not a valid datetime: {s:?}"));
            }
            return Ok(Self::from_datetime(dt.with_timezone(&Utc)));
        }
        for fmt in [
            "%Y-%m-%dT%H:%M:%S%.f",
            "%Y-%m-%d %H:%M:%S%.f",
            "%Y-%m-%dT%H:%M",
        ] {
            if NaiveDateTime::parse_from_str(t, fmt).is_ok() {
                return Err(NAIVE_TIMESTAMP_MESSAGE.to_owned());
            }
        }
        if chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").is_ok() {
            return Err(NAIVE_TIMESTAMP_MESSAGE.to_owned());
        }
        Err(format!("input is not a valid datetime: {s:?}"))
    }

    /// Pydantic's JSON form.
    pub fn to_json_string(&self) -> String {
        if self.0.nanosecond() == 0 {
            self.0.to_rfc3339_opts(SecondsFormat::Secs, true)
        } else {
            self.0.to_rfc3339_opts(SecondsFormat::Micros, true)
        }
    }
}

impl From<DateTime<Utc>> for UtcDateTime {
    fn from(dt: DateTime<Utc>) -> Self {
        Self::from_datetime(dt)
    }
}

impl From<UtcDateTime> for DateTime<Utc> {
    fn from(v: UtcDateTime) -> Self {
        v.0
    }
}

impl fmt::Display for UtcDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_json_string())
    }
}

impl Serialize for UtcDateTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_json_string())
    }
}

impl<'de> Deserialize<'de> for UtcDateTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        UtcDateTime::parse(&s).map_err(|m| {
            if m == NAIVE_TIMESTAMP_MESSAGE {
                serde::de::Error::custom(format!("Value error, {m}"))
            } else {
                serde::de::Error::custom(m)
            }
        })
    }
}

// ---- CurrencyCode -----------------------------------------------------------------------------

pub const CURRENCY_MESSAGE: &str = "currency must be an ISO 4217 code such as 'USD'";

/// `common.CurrencyCode`: three alphabetic upper-case characters (Python `isalpha`/`isupper`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CurrencyCode(String);

impl CurrencyCode {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let v = value.into();
        if is_currency(&v) {
            Ok(Self(v))
        } else {
            Err(CURRENCY_MESSAGE.to_owned())
        }
    }

    pub fn usd() -> Self {
        Self("USD".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Python: `len(value) == 3 and value.isalpha() and value.isupper()`.
fn is_currency(v: &str) -> bool {
    let chars: Vec<char> = v.chars().collect();
    if chars.len() != 3 || !chars.iter().all(|c| c.is_alphabetic()) {
        return false;
    }
    // isupper: no lower-case character and at least one upper-case (cased) character.
    !chars.iter().any(|c| c.is_lowercase()) && chars.iter().any(|c| c.is_uppercase())
}

impl Default for CurrencyCode {
    fn default() -> Self {
        Self::usd()
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for CurrencyCode {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for CurrencyCode {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for CurrencyCode {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl<'de> Deserialize<'de> for CurrencyCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        CurrencyCode::new(s).map_err(|m| serde::de::Error::custom(format!("Value error, {m}")))
    }
}

// ---- Money / Measurement ----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Money {
    pub amount_minor: MinorUnits,
    #[serde(default)]
    pub currency: CurrencyCode,
}

impl Money {
    pub fn usd(amount_minor: MinorUnits) -> Self {
        Self {
            amount_minor,
            currency: CurrencyCode::usd(),
        }
    }
}

impl Validate for Money {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for Money {
    const NAME: &'static str = "Money";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub value: f64,
    /// Explicit unit, e.g. 'A', 'V', 'in', 'm', 'm2', 'sqft'.
    pub unit: String,
}

impl Validate for Measurement {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for Measurement {
    const NAME: &'static str = "Measurement";
}

// ---- Shared enums -----------------------------------------------------------------------------

string_enum! {
    /// How an assertion is known. Never collapsed into a single confidence score.
    pub enum EvidenceClassification {
        DocumentedHistorical = "DOCUMENTED_HISTORICAL",
        VisuallyObserved = "VISUALLY_OBSERVED",
        GeometryMeasured = "GEOMETRY_MEASURED",
        OwnerReported = "OWNER_REPORTED",
        ProfessionallyVerified = "PROFESSIONALLY_VERIFIED",
        DerivedEstimate = "DERIVED_ESTIMATE",
        Unknown = "UNKNOWN",
        Conflicting = "CONFLICTING",
    }
}

string_enum! {
    pub enum VerificationStatus {
        Unverified = "UNVERIFIED",
        /// The record exists at the source as described.
        SourceConfirmed = "SOURCE_CONFIRMED",
        /// Independent sources agree.
        Corroborated = "CORROBORATED",
        ProfessionallyVerified = "PROFESSIONALLY_VERIFIED",
        Disputed = "DISPUTED",
    }
}

string_enum! {
    pub enum MatchStatus {
        Exact = "EXACT",
        Probable = "PROBABLE",
        Ambiguous = "AMBIGUOUS",
        Unmatched = "UNMATCHED",
        Conflicting = "CONFLICTING",
        NeedsReview = "NEEDS_REVIEW",
    }
}

string_enum! {
    /// Where a record's content came from. Fixtures must never masquerade as live data.
    pub enum DataOrigin {
        Live = "LIVE",
        /// Snapshot of a genuine record (public data or sponsor data kit).
        FixtureReal = "FIXTURE_REAL",
        /// Invented demo data.
        FixtureSynthetic = "FIXTURE_SYNTHETIC",
        UserProvided = "USER_PROVIDED",
        Derived = "DERIVED",
    }
}

string_enum! {
    pub enum IntegrationStatus {
        Live = "LIVE",
        Mock = "MOCK",
        Partial = "PARTIAL",
        Blocked = "BLOCKED",
        NotStarted = "NOT_STARTED",
    }
}

// ---- Point2D / SourceReference ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point2D {
    pub x: f64,
    pub y: f64,
}

impl Validate for Point2D {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for Point2D {
    const NAME: &'static str = "Point2D";
}

/// Lightweight pointer used inside other contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceReference {
    #[serde(default)]
    pub evidence_id: Option<Uuid>,
    pub description: String,
    #[serde(default)]
    pub url: Option<String>,
}

impl Validate for SourceReference {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for SourceReference {
    const NAME: &'static str = "SourceReference";
}
