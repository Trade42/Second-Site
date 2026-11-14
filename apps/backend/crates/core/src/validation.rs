//! Shared contract validation error and checker.
//!
//! Pydantic validates field constraints first and runs `model_validator(mode="after")` rules only
//! when every field of that model is valid. [`Checker`] reproduces that order: implementations of
//! [`Validate::check`] record field errors, then call [`Checker::model_rules`] for model-level rules.
//!
//! Errors serialize like Pydantic's `ValidationError.errors(include_url=False, include_context=False)`
//! entries minus `input`: `{"type": ..., "loc": [...], "msg": ...}`. The server puts that list under
//! `ProblemDetail.details.errors`.

use std::cmp::Ordering;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// One element of an error location: a field name or a list index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum LocItem {
    Field(String),
    Index(usize),
}

impl fmt::Display for LocItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocItem::Field(s) => f.write_str(s),
            LocItem::Index(i) => write!(f, "{i}"),
        }
    }
}

/// One validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorItem {
    /// Pydantic-style error type, e.g. `missing`, `extra_forbidden`, `value_error`,
    /// `greater_than_equal`, `string_too_short`.
    #[serde(rename = "type")]
    pub kind: String,
    pub loc: Vec<LocItem>,
    pub msg: String,
}

/// A contract failed validation. `title` is the contract name (Pydantic's `ValidationError.title`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
pub struct ValidationError {
    pub title: String,
    pub errors: Vec<ErrorItem>,
}

impl ValidationError {
    /// A single model-level `value_error` (Pydantic: `ValueError` raised inside a validator).
    pub fn value_error(title: &str, msg: impl Into<String>) -> Self {
        Self {
            title: title.to_owned(),
            errors: vec![ErrorItem {
                kind: "value_error".into(),
                loc: Vec::new(),
                msg: format!("Value error, {}", msg.into()),
            }],
        }
    }

    /// True when any error message contains `needle` (the analogue of `pytest.raises(match=...)`).
    pub fn mentions(&self, needle: &str) -> bool {
        self.errors.iter().any(|e| e.msg.contains(needle))
    }

    /// The `errors` list as JSON, for `ProblemDetail.details.errors`.
    pub fn errors_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.errors
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "type": e.kind,
                        "loc": e.loc,
                        "msg": e.msg,
                    })
                })
                .collect(),
        )
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.errors.len();
        write!(
            f,
            "{n} validation error{} for {}",
            if n == 1 { "" } else { "s" },
            self.title
        )?;
        for e in &self.errors {
            let loc: Vec<String> = e.loc.iter().map(ToString::to_string).collect();
            write!(f, "\n{}\n  {} [type={}]", loc.join("."), e.msg, e.kind)?;
        }
        Ok(())
    }
}

/// Collects errors while walking a contract value.
#[derive(Debug, Default)]
pub struct Checker {
    path: Vec<LocItem>,
    errors: Vec<ErrorItem>,
}

impl Checker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn error_count(&self) -> usize {
        self.errors.len()
    }

    /// Record an error at the current path plus an optional field.
    pub fn push(&mut self, field: Option<&str>, kind: &str, msg: impl Into<String>) {
        let mut loc = self.path.clone();
        if let Some(f) = field {
            loc.push(LocItem::Field(f.to_owned()));
        }
        self.errors.push(ErrorItem {
            kind: kind.to_owned(),
            loc,
            msg: msg.into(),
        });
    }

    /// Model-level `ValueError` (Pydantic reports it with the model's own location).
    pub fn value_error(&mut self, msg: impl AsRef<str>) {
        self.push(
            None,
            "value_error",
            format!("Value error, {}", msg.as_ref()),
        );
    }

    /// Field-level `ValueError` (from an `AfterValidator` on that field).
    pub fn field_value_error(&mut self, field: &str, msg: impl AsRef<str>) {
        self.push(
            Some(field),
            "value_error",
            format!("Value error, {}", msg.as_ref()),
        );
    }

    /// Run `f` with `field` appended to the path.
    pub fn at(&mut self, field: &str, f: impl FnOnce(&mut Self)) {
        self.path.push(LocItem::Field(field.to_owned()));
        f(self);
        self.path.pop();
    }

    /// Run `f` with a list index appended to the path.
    pub fn at_index(&mut self, index: usize, f: impl FnOnce(&mut Self)) {
        self.path.push(LocItem::Index(index));
        f(self);
        self.path.pop();
    }

    pub fn nested<T: Validate + ?Sized>(&mut self, field: &str, value: &T) {
        self.at(field, |c| value.check(c));
    }

    pub fn nested_opt<T: Validate>(&mut self, field: &str, value: &Option<T>) {
        if let Some(v) = value {
            self.nested(field, v);
        }
    }

    pub fn each<T: Validate>(&mut self, field: &str, items: &[T]) {
        self.path.push(LocItem::Field(field.to_owned()));
        for (i, item) in items.iter().enumerate() {
            self.path.push(LocItem::Index(i));
            item.check(self);
            self.path.pop();
        }
        self.path.pop();
    }

    /// Run model-level rules only when the model's fields produced no errors since `mark`
    /// (Pydantic does not run `mode="after"` validators on an invalid model).
    pub fn model_rules(&mut self, mark: usize, f: impl FnOnce(&mut Self)) {
        if self.errors.len() == mark {
            f(self);
        }
    }

    pub fn ge<N: PartialOrd + fmt::Display>(&mut self, field: &str, value: N, min: N) {
        if !matches!(
            value.partial_cmp(&min),
            Some(Ordering::Greater | Ordering::Equal)
        ) {
            self.push(
                Some(field),
                "greater_than_equal",
                format!("Input should be greater than or equal to {min}"),
            );
        }
    }

    pub fn gt<N: PartialOrd + fmt::Display>(&mut self, field: &str, value: N, min: N) {
        if !matches!(value.partial_cmp(&min), Some(Ordering::Greater)) {
            self.push(
                Some(field),
                "greater_than",
                format!("Input should be greater than {min}"),
            );
        }
    }

    pub fn le<N: PartialOrd + fmt::Display>(&mut self, field: &str, value: N, max: N) {
        if !matches!(
            value.partial_cmp(&max),
            Some(Ordering::Less | Ordering::Equal)
        ) {
            self.push(
                Some(field),
                "less_than_equal",
                format!("Input should be less than or equal to {max}"),
            );
        }
    }

    pub fn ge_opt<N: PartialOrd + fmt::Display + Copy>(
        &mut self,
        field: &str,
        value: Option<N>,
        min: N,
    ) {
        if let Some(v) = value {
            self.ge(field, v, min);
        }
    }

    pub fn gt_opt<N: PartialOrd + fmt::Display + Copy>(
        &mut self,
        field: &str,
        value: Option<N>,
        min: N,
    ) {
        if let Some(v) = value {
            self.gt(field, v, min);
        }
    }

    pub fn le_opt<N: PartialOrd + fmt::Display + Copy>(
        &mut self,
        field: &str,
        value: Option<N>,
        max: N,
    ) {
        if let Some(v) = value {
            self.le(field, v, max);
        }
    }

    /// Pydantic `min_length` on `str` counts code points.
    pub fn min_len(&mut self, field: &str, value: &str, min: usize) {
        if value.chars().count() < min {
            self.push(
                Some(field),
                "string_too_short",
                format!(
                    "String should have at least {min} character{}",
                    if min == 1 { "" } else { "s" }
                ),
            );
        }
    }

    pub fn max_len(&mut self, field: &str, value: &str, max: usize) {
        if value.chars().count() > max {
            self.push(
                Some(field),
                "string_too_long",
                format!(
                    "String should have at most {max} character{}",
                    if max == 1 { "" } else { "s" }
                ),
            );
        }
    }

    pub fn finish(self, title: &str) -> Result<(), ValidationError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationError {
                title: title.to_owned(),
                errors: self.errors,
            })
        }
    }
}

/// Field constraints and model validators of one contract.
pub trait Validate {
    fn check(&self, c: &mut Checker);
}

impl<T: Validate> Validate for Vec<T> {
    fn check(&self, c: &mut Checker) {
        for (i, item) in self.iter().enumerate() {
            c.path.push(LocItem::Index(i));
            item.check(c);
            c.path.pop();
        }
    }
}

/// A named, validated wire contract.
pub trait Contract: Serialize + DeserializeOwned + Validate {
    /// The contract name (matches `packages/contracts/schemas/<NAME>.json`).
    const NAME: &'static str;

    /// Check every field constraint and model rule.
    fn validate(&self) -> Result<(), ValidationError> {
        let mut c = Checker::new();
        self.check(&mut c);
        c.finish(Self::NAME)
    }

    /// Deserialize (unknown fields rejected) and validate.
    fn from_json_value(value: serde_json::Value) -> Result<Self, ValidationError> {
        let v: Self =
            serde_path_to_error::deserialize(value).map_err(|e| from_serde_error(Self::NAME, e))?;
        v.validate()?;
        Ok(v)
    }

    /// Deserialize from JSON text (unknown fields rejected) and validate.
    fn from_json_str(s: &str) -> Result<Self, ValidationError> {
        let mut de = serde_json::Deserializer::from_str(s);
        let v: Self = serde_path_to_error::deserialize(&mut de)
            .map_err(|e| from_serde_error(Self::NAME, e))?;
        de.end().map_err(|e| ValidationError {
            title: Self::NAME.to_owned(),
            errors: vec![ErrorItem {
                kind: "json_invalid".into(),
                loc: Vec::new(),
                msg: format!("Invalid JSON: {e}"),
            }],
        })?;
        v.validate()?;
        Ok(v)
    }
}

/// Convert a serde error (with its path) into a Pydantic-shaped [`ValidationError`].
///
/// serde stops at the first error, so this always yields exactly one item (Pydantic reports all).
pub fn from_serde_error(
    title: &str,
    err: serde_path_to_error::Error<serde_json::Error>,
) -> ValidationError {
    let mut loc: Vec<LocItem> = Vec::new();
    for seg in err.path().iter() {
        match seg {
            serde_path_to_error::Segment::Seq { index } => loc.push(LocItem::Index(*index)),
            serde_path_to_error::Segment::Map { key } => loc.push(LocItem::Field(key.clone())),
            serde_path_to_error::Segment::Enum { variant } => {
                loc.push(LocItem::Field(variant.clone()))
            }
            serde_path_to_error::Segment::Unknown => {}
        }
    }
    let inner = err.into_inner();
    let raw = inner.to_string();
    // serde_json appends " at line L column C" for text input; keep the message itself.
    let text = match raw.find(" at line ") {
        Some(i) if inner.line() > 0 => raw[..i].to_owned(),
        _ => raw,
    };
    let backticked = |s: &str| -> Option<String> {
        let start = s.find('`')? + 1;
        let end = s[start..].find('`')? + start;
        Some(s[start..end].to_owned())
    };
    let (kind, msg) = if text.starts_with("missing field") {
        if let Some(f) = backticked(&text) {
            loc.push(LocItem::Field(f));
        }
        ("missing", "Field required".to_owned())
    } else if text.starts_with("unknown field") {
        // serde_path_to_error already ends the path with the unknown key.
        if let Some(f) = backticked(&text) {
            if loc.last() != Some(&LocItem::Field(f.clone())) {
                loc.push(LocItem::Field(f));
            }
        }
        (
            "extra_forbidden",
            "Extra inputs are not permitted".to_owned(),
        )
    } else if text.starts_with("unknown variant") {
        ("enum", enum_message(&text).unwrap_or(text))
    } else if text.starts_with("Value error, ") {
        ("value_error", text)
    } else if inner.is_syntax() || inner.is_eof() {
        ("json_invalid", format!("Invalid JSON: {text}"))
    } else {
        ("invalid_input", text)
    };
    ValidationError {
        title: title.to_owned(),
        errors: vec![ErrorItem {
            kind: kind.to_owned(),
            loc,
            msg,
        }],
    }
}

/// Rewrite serde's "unknown variant `X`, expected one of `A`, `B`, `C`" as Pydantic's
/// "Input should be 'A', 'B' or 'C'".
fn enum_message(text: &str) -> Option<String> {
    let expected = &text[text.find("expected")?..];
    let names: Vec<&str> = expected.split('`').skip(1).step_by(2).collect();
    let (last, rest) = names.split_last()?;
    let quoted: Vec<String> = rest.iter().map(|n| format!("'{n}'")).collect();
    Some(if quoted.is_empty() {
        format!("Input should be '{last}'")
    } else {
        format!("Input should be {} or '{last}'", quoted.join(", "))
    })
}
