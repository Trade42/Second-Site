//! Test helpers: repository paths, JSON Schema conformance.

#![allow(dead_code)]

use std::path::PathBuf;

use serde_json::Value;

/// The repository root, resolved from this crate (as Python resolves it from its source file).
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .expect("repository root")
}

pub fn read_json(rel: &str) -> Value {
    let path = repo_root().join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

pub fn golden(name: &str) -> Value {
    read_json(&format!("apps/backend/tests-golden/{name}"))
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

/// The schema must reject `instance` (used to prove the conformance check has teeth).
pub fn assert_rejected_by_schema(name: &str, instance: &Value) {
    let schema = read_json(&format!("packages/contracts/schemas/{name}.json"));
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|e| panic!("schema {name}: {e}"));
    assert!(
        !validator.is_valid(instance),
        "{name} schema unexpectedly accepted {instance:#}"
    );
}
