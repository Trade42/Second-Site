//! Test helpers: repository paths, golden files, JSON Schema conformance, normalization.

#![allow(dead_code)]

pub mod analysis;
pub mod labels;

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Map, Value};

pub const PROJECT_ID: &str = "6b1727f7-0000-4000-8000-0000000000a1";
pub const PROPERTY_ID: &str = "6b1727f7-0000-4000-8000-0000000000b1";

/// The repository root, resolved from this crate (as Python resolves it from its source file).
pub fn repo_root() -> PathBuf {
    ss_domain::integrations::base::repo_root()
}

pub fn read_json(rel: &str) -> Value {
    let path = repo_root().join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

pub fn golden(name: &str) -> Value {
    read_json(&format!("apps/backend/tests-golden/{name}"))
}

pub fn golden_dir(name: &str) -> PathBuf {
    repo_root().join("apps/backend/tests-golden").join(name)
}

pub fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).expect("serialize")
}

/// Validate `instance` against `packages/contracts/schemas/<name>.json` (formats checked).
pub fn assert_conforms(name: &str, instance: &Value) {
    let schema = read_json(&format!("packages/contracts/schemas/{name}.json"));
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|e| panic!("schema {name}: {e}"));
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{name} does not conform to its JSON Schema: {errors:#?}\n{instance:#}"
    );
}

pub fn assert_rejected_by_schema(name: &str, instance: &Value) {
    let schema = read_json(&format!("packages/contracts/schemas/{name}.json"));
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|e| panic!("schema {name}: {e}"));
    assert!(
        !validator.is_valid(instance),
        "{name} schema accepted {instance:#}"
    );
}

/// Mirror of `normalize()` in `tests-golden/gen_domain.py`: freshly generated ids
/// (`evidence_id`, `permit_record_id`) become `<id>`, non-null `retrieved_at` becomes
/// `<timestamp>`, `source_evidence_ids` become positions in the evidence list, and the generated
/// property id becomes `<property_id>` wherever it appears.
pub fn normalize(doc: &Value, property_id: Option<&str>, evidence_ids: &[String]) -> Value {
    let order: HashMap<&str, usize> = evidence_ids
        .iter()
        .enumerate()
        .map(|(i, e)| (e.as_str(), i))
        .collect();
    fn walk(
        v: &Value,
        key: Option<&str>,
        pid: Option<&str>,
        order: &HashMap<&str, usize>,
    ) -> Value {
        match key {
            Some("evidence_id" | "permit_record_id") => return Value::String("<id>".into()),
            Some("retrieved_at") if !v.is_null() => return Value::String("<timestamp>".into()),
            Some("source_evidence_ids") => {
                return Value::Array(
                    v.as_array()
                        .map(|a| {
                            a.iter()
                                .map(|x| match x.as_str().and_then(|s| order.get(s)) {
                                    Some(i) => Value::from(*i),
                                    None => Value::String("<unknown evidence>".into()),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                )
            }
            _ => {}
        }
        match v {
            Value::Object(m) => {
                let mut out = Map::new();
                for (k, x) in m {
                    out.insert(k.clone(), walk(x, Some(k), pid, order));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| walk(x, None, pid, order)).collect()),
            Value::String(s) if Some(s.as_str()) == pid => Value::String("<property_id>".into()),
            other => other.clone(),
        }
    }
    walk(doc, None, property_id, &order)
}

/// Assert equality and print the first differing path on failure.
pub fn assert_json_eq(ours: &Value, theirs: &Value, ctx: &str) {
    if ours != theirs {
        let path = first_diff(ours, theirs, String::new()).unwrap_or_default();
        panic!("{ctx}: JSON differs at {path}\nRust:   {ours:#}\nPython: {theirs:#}");
    }
}

fn first_diff(a: &Value, b: &Value, path: String) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                match (x.get(k), y.get(k)) {
                    (Some(p), Some(q)) => {
                        if let Some(d) = first_diff(p, q, format!("{path}.{k}")) {
                            return Some(d);
                        }
                    }
                    _ => return Some(format!("{path}.{k} (missing on one side)")),
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!("{path} (length {} vs {})", x.len(), y.len()));
            }
            x.iter()
                .zip(y)
                .enumerate()
                .find_map(|(i, (p, q))| first_diff(p, q, format!("{path}[{i}]")))
        }
        _ if a == b => None,
        _ => Some(format!("{path}: {a} vs {b}")),
    }
}
