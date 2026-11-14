//! Port of `apps/api/app/core/logging.py`: one JSON object per line on stdout, with the request id
//! of the current request and redaction of secret-looking field names.
//!
//! Line format (as Python's `JsonFormatter`, `json.dumps` defaults):
//! `{"ts": "...+00:00", "level": "INFO", "logger": "app.request", "msg": "request", "request_id": "...", <fields>}`

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

use serde_json::Value;

use crate::pyjson;

tokio::task_local! {
    /// The `x-request-id` of the request being served (Python's `request_id_var`).
    pub static REQUEST_ID: String;
}

/// The current request id, if called inside a request.
pub fn current_request_id() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 10,
    Info = 20,
    Warning = 30,
    Error = 40,
    Critical = 50,
}

impl Level {
    pub fn name(&self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
            Level::Critical => "CRITICAL",
        }
    }

    fn parse(s: &str) -> Level {
        match s.trim().to_ascii_uppercase().as_str() {
            "DEBUG" => Level::Debug,
            "WARNING" | "WARN" => Level::Warning,
            "ERROR" => Level::Error,
            "CRITICAL" | "FATAL" => Level::Critical,
            _ => Level::Info,
        }
    }
}

static MIN_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// `configure_logging(level)`.
pub fn configure(level: &str) {
    MIN_LEVEL.store(Level::parse(level) as u8, Ordering::Relaxed);
}

const REDACT: [&str; 5] = ["token", "secret", "password", "api_key", "authorization"];

/// Render one log line (without the trailing newline).
pub fn render(level: Level, logger: &str, msg: &str, fields: &[(&str, Value)]) -> String {
    let mut map: Vec<(String, Value)> = vec![
        (
            "ts".into(),
            Value::String(
                chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.6f+00:00")
                    .to_string(),
            ),
        ),
        ("level".into(), Value::String(level.name().into())),
        ("logger".into(), Value::String(logger.into())),
        ("msg".into(), Value::String(msg.into())),
        (
            "request_id".into(),
            current_request_id().map_or(Value::Null, Value::String),
        ),
    ];
    for (k, v) in fields {
        let lower = k.to_ascii_lowercase();
        let v = if REDACT.iter().any(|r| lower.contains(r)) {
            Value::String("[REDACTED]".into())
        } else {
            v.clone()
        };
        match map.iter_mut().find(|(name, _)| name == k) {
            Some(slot) => slot.1 = v,
            None => map.push(((*k).to_owned(), v)),
        }
    }
    let mut out = String::from("{");
    for (i, (k, v)) in map.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&pyjson::dumps(k).unwrap_or_else(|_| "\"?\"".into()));
        out.push_str(": ");
        out.push_str(&pyjson::dumps(v).unwrap_or_else(|_| "null".into()));
    }
    out.push('}');
    out
}

/// `log(logger, level, msg, **fields)`.
pub fn log(level: Level, logger: &str, msg: &str, fields: &[(&str, Value)]) {
    if (level as u8) < MIN_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let line = render(level, logger, msg, fields);
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
}

pub fn info(logger: &str, msg: &str, fields: &[(&str, Value)]) {
    log(Level::Info, logger, msg, fields);
}

pub fn warning(logger: &str, msg: &str, fields: &[(&str, Value)]) {
    log(Level::Warning, logger, msg, fields);
}

pub fn error(logger: &str, msg: &str, fields: &[(&str, Value)]) {
    log(Level::Error, logger, msg, fields);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_secret_fields_and_keeps_order() {
        let line = render(
            Level::Info,
            "app.request",
            "request",
            &[
                ("method", json!("GET")),
                ("Authorization", json!("Bearer abc")),
                ("api_key_id", json!("k")),
                ("ms", json!(1.5)),
            ],
        );
        assert!(line.contains(r#""logger": "app.request", "msg": "request", "request_id": null, "method": "GET", "Authorization": "[REDACTED]", "api_key_id": "[REDACTED]", "ms": 1.5}"#), "{line}");
        assert!(!line.contains("abc"));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["level"], "INFO");
    }
}
