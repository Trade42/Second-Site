//! Value types shared by several contracts.

use std::fmt;
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::py;

/// Pydantic `float | str | bool`. Like Pydantic's smart union on JSON input: `true`/`false` stay
/// booleans, any JSON number (integer or not) becomes a float, strings stay strings. A Python
/// `int` assigned to such a field is therefore serialized as a float (`5` -> `5.0`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScalarValue {
    Bool(bool),
    Number(f64),
    Text(String),
}

impl ScalarValue {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ScalarValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            ScalarValue::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ScalarValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The JSON value Pydantic would dump.
    pub fn to_json(&self) -> Value {
        match self {
            ScalarValue::Bool(b) => Value::Bool(*b),
            ScalarValue::Number(n) => Value::from(*n),
            ScalarValue::Text(s) => Value::String(s.clone()),
        }
    }
}

impl From<bool> for ScalarValue {
    fn from(v: bool) -> Self {
        ScalarValue::Bool(v)
    }
}

impl From<f64> for ScalarValue {
    fn from(v: f64) -> Self {
        ScalarValue::Number(v)
    }
}

impl From<i64> for ScalarValue {
    fn from(v: i64) -> Self {
        ScalarValue::Number(v as f64)
    }
}

impl From<&str> for ScalarValue {
    fn from(v: &str) -> Self {
        ScalarValue::Text(v.to_owned())
    }
}

impl From<String> for ScalarValue {
    fn from(v: String) -> Self {
        ScalarValue::Text(v)
    }
}

/// A Python `Decimal` field. Serialized as a JSON string in `str(Decimal)` form (as Pydantic
/// does); accepted from a JSON number or string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PyDecimal(pub Decimal);

impl PyDecimal {
    pub const ZERO: PyDecimal = PyDecimal(Decimal::ZERO);
}

impl fmt::Display for PyDecimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&py::decimal_str(&self.0))
    }
}

impl From<Decimal> for PyDecimal {
    fn from(d: Decimal) -> Self {
        PyDecimal(d)
    }
}

impl From<i64> for PyDecimal {
    fn from(v: i64) -> Self {
        PyDecimal(Decimal::from(v))
    }
}

impl Serialize for PyDecimal {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&py::decimal_str(&self.0))
    }
}

impl<'de> Deserialize<'de> for PyDecimal {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let parsed = match &v {
            Value::Number(_) => py::decimal_of(&v),
            Value::String(s) => {
                let t = py::strip(s);
                Decimal::from_str(t)
                    .ok()
                    .or_else(|| Decimal::from_scientific(t).ok())
            }
            _ => None,
        };
        parsed
            .map(PyDecimal)
            .ok_or_else(|| serde::de::Error::custom("Decimal input should be a number or a string"))
    }
}
