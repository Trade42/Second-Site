//! Request parsing with FastAPI's semantics: `current_user` (`api/deps.py`), path / query / body
//! validation reported as 422 `VALIDATION_ERROR` with FastAPI-shaped `details.errors`
//! (`type`, `loc` starting with `path` / `query` / `body`, `msg`).
//!
//! Order of checks follows FastAPI: authentication, then path parameters, then the owned-project
//! lookup, then query parameters and the body.

use std::collections::HashMap;

use axum::extract::{FromRequestParts, Path, Query};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use serde_json::{json, Value};
use ss_core::{Contract, LocItem, ValidationError};
use uuid::Uuid;

use crate::app::AppState;
use crate::error::AppError;
use crate::security;

/// Python `str.strip()` whitespace (Unicode White_Space plus the \x1c-\x1f separators).
fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// Header bytes as Starlette sees them (latin-1).
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| char::from(*b)).collect()
}

/// `current_user`: the authenticated user id (401 otherwise).
pub fn bearer_user(headers: &HeaderMap, secret: &str) -> Result<Uuid, AppError> {
    let Some(raw) = headers.get(AUTHORIZATION) else {
        return Err(AppError::unauthorized("missing bearer token"));
    };
    let value = latin1(raw.as_bytes());
    if !value.to_lowercase().starts_with("bearer ") {
        return Err(AppError::unauthorized("missing bearer token"));
    }
    let token = value
        .split_once(' ')
        .map(|(_, t)| py_strip(t))
        .unwrap_or("");
    security::verify_token(secret, token, "access")
}

/// The authenticated user (`Depends(current_user)`).
#[derive(Debug, Clone, Copy)]
pub struct AuthUser(pub Uuid);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        bearer_user(&parts.headers, state.settings.auth_secret.expose()).map(AuthUser)
    }
}

/// Raw path parameters; typed with [`PathParams::uuid`].
#[derive(Debug, Clone)]
pub struct PathParams(HashMap<String, String>);

impl FromRequestParts<AppState> for PathParams {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        match Path::<HashMap<String, String>>::from_request_parts(parts, state).await {
            Ok(Path(map)) => Ok(PathParams(map)),
            Err(e) => Err(AppError::request_validation(
                "value_error",
                vec![json!("path")],
                e.body_text(),
            )),
        }
    }
}

fn uuid_error(msg: &str) -> String {
    format!("Input should be a valid UUID, {msg}")
}

impl PathParams {
    /// A `str` path parameter.
    pub fn raw(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }

    /// A UUID path parameter (pydantic-core parses UUIDs with the same `uuid` crate).
    pub fn uuid(&self, name: &str) -> Result<Uuid, AppError> {
        let raw = self.0.get(name).map(String::as_str).unwrap_or("");
        Uuid::parse_str(raw).map_err(|e| {
            AppError::RequestValidation(vec![json!({
                "type": "uuid_parsing",
                "loc": ["path", name],
                "msg": uuid_error(&e.to_string()),
                "input": raw,
            })])
        })
    }
}

/// Raw query parameters (the last occurrence of a repeated name wins, as Starlette's `.get`).
#[derive(Debug, Clone, Default)]
pub struct QueryParams(HashMap<String, String>);

impl FromRequestParts<AppState> for QueryParams {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        match Query::<Vec<(String, String)>>::from_request_parts(parts, state).await {
            Ok(Query(pairs)) => Ok(QueryParams(pairs.into_iter().collect())),
            Err(e) => Err(AppError::request_validation(
                "value_error",
                vec![json!("query")],
                e.body_text(),
            )),
        }
    }
}

/// Bounds of an integer query parameter.
#[derive(Debug, Clone, Copy)]
pub struct IntBounds {
    pub default: i64,
    pub ge: Option<i64>,
    pub le: Option<i64>,
}

impl QueryParams {
    /// `Query(default, ge=, le=)` for an `int`. Returns every violation of this parameter.
    pub fn int(&self, name: &str, b: IntBounds) -> Result<i64, Vec<Value>> {
        let Some(raw) = self.0.get(name) else {
            return Ok(b.default);
        };
        let t = raw.trim();
        let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
        let parsed = if !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()) {
            t.parse::<i64>().ok()
        } else {
            None
        };
        let Some(v) = parsed else {
            return Err(vec![json!({
                "type": "int_parsing",
                "loc": ["query", name],
                "msg": "Input should be a valid integer, unable to parse string as an integer",
                "input": raw,
            })]);
        };
        if let Some(ge) = b.ge {
            if v < ge {
                return Err(vec![json!({
                    "type": "greater_than_equal",
                    "loc": ["query", name],
                    "msg": format!("Input should be greater than or equal to {ge}"),
                    "input": raw,
                    "ctx": {"ge": ge},
                })]);
            }
        }
        if let Some(le) = b.le {
            if v > le {
                return Err(vec![json!({
                    "type": "less_than_equal",
                    "loc": ["query", name],
                    "msg": format!("Input should be less than or equal to {le}"),
                    "input": raw,
                    "ctx": {"le": le},
                })]);
            }
        }
        Ok(v)
    }
}

/// Collect several parameter results into one 422 (FastAPI reports them together).
pub fn collect_query_errors<const N: usize>(
    results: [Result<i64, Vec<Value>>; N],
) -> Result<[i64; N], AppError> {
    let mut errors = Vec::new();
    let mut out = [0i64; N];
    for (i, r) in results.into_iter().enumerate() {
        match r {
            Ok(v) => out[i] = v,
            Err(e) => errors.extend(e),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(AppError::RequestValidation(errors))
    }
}

/// What a body parameter is declared as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    /// A Pydantic model.
    Model,
    /// `body: dict`.
    Dict,
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(ct) = headers.get(CONTENT_TYPE) else {
        // FastAPI parses a body without a content type as JSON.
        return true;
    };
    let ct = latin1(ct.as_bytes()).to_ascii_lowercase();
    let mime = ct.split(';').next().unwrap_or("").trim().to_owned();
    match mime.split_once('/') {
        Some(("application", sub)) => sub == "json" || sub.ends_with("+json"),
        _ => false,
    }
}

/// The character offset of serde_json's (line, column) in `text` (Python's `JSONDecodeError.pos`).
fn char_offset(text: &str, line: usize, column: usize) -> usize {
    let mut offset = 0;
    for (i, l) in text.split('\n').enumerate() {
        if i + 1 == line {
            return offset + column.saturating_sub(1).min(l.chars().count());
        }
        offset += l.chars().count() + 1;
    }
    offset
}

fn type_error(kind: BodyKind, input: Value) -> AppError {
    let (t, msg) = match kind {
        BodyKind::Model => (
            "model_attributes_type",
            "Input should be a valid dictionary or object to extract fields from",
        ),
        BodyKind::Dict => ("dict_type", "Input should be a valid dictionary"),
    };
    AppError::RequestValidation(vec![
        json!({"type": t, "loc": ["body"], "msg": msg, "input": input}),
    ])
}

/// A request body after FastAPI's first step: decoded as JSON when the content type says JSON,
/// before any dependency (authentication, owned project) runs.
#[derive(Debug, Clone, PartialEq)]
pub enum EarlyBody {
    Empty,
    /// Not declared as JSON: kept as text (validation reports it as the wrong type).
    Raw(String),
    Json(Value),
}

/// Decode the body as FastAPI does before resolving dependencies: invalid JSON is a 422
/// `json_invalid` even for an unauthenticated request.
pub fn early_json_body(headers: &HeaderMap, body: &[u8]) -> Result<EarlyBody, AppError> {
    if body.is_empty() {
        return Ok(EarlyBody::Empty);
    }
    if !is_json_content_type(headers) {
        return Ok(EarlyBody::Raw(String::from_utf8_lossy(body).into_owned()));
    }
    match serde_json::from_slice(body) {
        Ok(v) => Ok(EarlyBody::Json(v)),
        Err(e) => {
            let text = String::from_utf8_lossy(body);
            let pos = char_offset(&text, e.line(), e.column());
            Err(AppError::RequestValidation(vec![json!({
                "type": "json_invalid",
                "loc": ["body", pos],
                "msg": "JSON decode error",
                "input": {},
                "ctx": {"error": e.to_string()},
            })]))
        }
    }
}

impl EarlyBody {
    fn object(self, kind: BodyKind) -> Result<serde_json::Map<String, Value>, AppError> {
        match self {
            EarlyBody::Empty => Err(AppError::RequestValidation(vec![json!({
                "type": "missing", "loc": ["body"], "msg": "Field required", "input": null,
            })])),
            EarlyBody::Raw(text) => Err(type_error(kind, Value::String(text))),
            EarlyBody::Json(Value::Object(m)) => Ok(m),
            EarlyBody::Json(other) => Err(type_error(kind, other)),
        }
    }

    /// The body as the JSON object a Pydantic model body is validated from (422 otherwise).
    pub fn into_object(self) -> Result<serde_json::Map<String, Value>, AppError> {
        self.object(BodyKind::Model)
    }
}

/// Parse the request body into a JSON object, as FastAPI does before model validation.
fn body_object(
    headers: &HeaderMap,
    body: &[u8],
    kind: BodyKind,
) -> Result<serde_json::Map<String, Value>, AppError> {
    early_json_body(headers, body)?.object(kind)
}

/// Request-validation errors from a contract's validation error (`loc` prefixed with `body`).
pub fn body_errors(e: ValidationError) -> AppError {
    let errors = e
        .errors
        .into_iter()
        .map(|item| {
            let mut loc = vec![json!("body")];
            loc.extend(item.loc.into_iter().map(|l| match l {
                LocItem::Field(f) => json!(f),
                LocItem::Index(i) => json!(i),
            }));
            json!({"type": item.kind, "loc": loc, "msg": item.msg})
        })
        .collect();
    AppError::RequestValidation(errors)
}

/// `body_errors` plus what FastAPI also reports for each error (review item R6 of part 1, for
/// the routes that use this): `input`, the offending value (the enclosing object for a missing
/// field), and for the numeric bound errors `ctx` with the bound, read back from the message
/// Pydantic formats from the same literal (`Input should be less than or equal to 50` ->
/// `{"le": 50}`).
pub fn body_errors_with_input(
    e: ValidationError,
    body: &serde_json::Map<String, Value>,
) -> AppError {
    let AppError::RequestValidation(mut errors) = body_errors(e) else {
        return AppError::internal("unexpected validation error shape");
    };
    let root = Value::Object(body.clone());
    for err in &mut errors {
        let Some(obj) = err.as_object_mut() else {
            continue;
        };
        let loc: Vec<Value> = obj
            .get("loc")
            .and_then(Value::as_array)
            .map(|a| a.iter().skip(1).cloned().collect())
            .unwrap_or_default();
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut cur = Some(&root);
        let mut parent = Some(&root);
        for l in &loc {
            parent = cur;
            cur = match (cur, l) {
                (Some(Value::Object(m)), Value::String(k)) => m.get(k),
                (Some(Value::Array(a)), Value::Number(n)) => {
                    n.as_u64().and_then(|i| a.get(i as usize))
                }
                _ => None,
            };
        }
        let input = if kind == "missing" { parent } else { cur };
        if let Some(v) = input {
            obj.insert("input".into(), v.clone());
        }
        let bound = match kind.as_str() {
            "greater_than" => Some("gt"),
            "greater_than_equal" => Some("ge"),
            "less_than" => Some("lt"),
            "less_than_equal" => Some("le"),
            _ => None,
        };
        if let Some(key) = bound {
            let lit = obj
                .get("msg")
                .and_then(Value::as_str)
                .and_then(|m| m.rsplit(' ').next())
                .unwrap_or("")
                .to_owned();
            let n = if lit.contains(['.', 'e', 'E']) {
                lit.parse::<f64>().ok().map(Value::from)
            } else {
                lit.parse::<i64>().ok().map(Value::from)
            };
            if let Some(n) = n {
                obj.insert("ctx".into(), json!({ key: n }));
            }
        }
    }
    AppError::RequestValidation(errors)
}

/// A body declared as a Pydantic model.
pub fn json_body<T: Contract>(headers: &HeaderMap, body: &[u8]) -> Result<T, AppError> {
    let obj = body_object(headers, body, BodyKind::Model)?;
    T::from_json_value(Value::Object(obj)).map_err(body_errors)
}

/// A body declared as `dict`.
pub fn dict_body(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<serde_json::Map<String, Value>, AppError> {
    body_object(headers, body, BodyKind::Dict)
}

/// Python truthiness of a JSON value (`x or default`).
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets() {
        assert_eq!(char_offset("{bad", 1, 2), 1);
        assert_eq!(char_offset("{\n  x", 2, 3), 4);
    }

    #[test]
    fn content_types() {
        let mut h = HeaderMap::new();
        assert!(is_json_content_type(&h));
        h.insert(
            CONTENT_TYPE,
            "application/json; charset=utf-8".parse().unwrap(),
        );
        assert!(is_json_content_type(&h));
        h.insert(
            CONTENT_TYPE,
            "application/merge-patch+json".parse().unwrap(),
        );
        assert!(is_json_content_type(&h));
        h.insert(CONTENT_TYPE, "text/plain".parse().unwrap());
        assert!(!is_json_content_type(&h));
    }

    #[test]
    fn truthiness() {
        assert!(!py_truthy(&json!(null)));
        assert!(!py_truthy(&json!("")));
        assert!(!py_truthy(&json!(0)));
        assert!(py_truthy(&json!("x")));
    }
}
