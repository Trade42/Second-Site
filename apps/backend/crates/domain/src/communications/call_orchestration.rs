//! The pure part of `apps/api/app/services/call_orchestration/__init__.py`: [`call_payload`],
//! the canonical payload the owner approves for `PLACE_CALL`. The orchestrator itself (database,
//! carrier, events) belongs to the server.

use indexmap::IndexMap;
use serde_json::{Map, Value};
use uuid::Uuid;

/// `call_payload(project_id, recipient, property_id)`:
/// `{"project_id": str, "property_id": str | null, "recipient": {...}}`.
///
/// This is the approval payload of a call, so it must hash to Python's
/// `payload_hash(call_payload(...))`: `sha256(json.dumps(payload, sort_keys=True,
/// separators=(",", ":"), default=str))`, which the server's `payload_hash` computes. UUIDs are
/// lower-case hyphenated strings (`str(UUID)`); the recipient's values are kept as given.
pub fn call_payload(
    project_id: Uuid,
    recipient: &IndexMap<String, String>,
    property_id: Option<Uuid>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("project_id".into(), Value::from(project_id.to_string()));
    m.insert(
        "property_id".into(),
        property_id.map_or(Value::Null, |p| Value::from(p.to_string())),
    );
    m.insert(
        "recipient".into(),
        Value::Object(
            recipient
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
                .collect(),
        ),
    );
    m
}
