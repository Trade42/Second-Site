//! Wire and row parity of the call flow with Python (server part 4): replays
//! `tests-golden/server/comms_slice.json` (written by `gen_comms_slice.py`, which drove the Python
//! app through outreach, approval, calls, the simulator with and without a handoff, Telnyx
//! webhooks for both legs, the handoff response and the list routes) against this service, on the
//! in-memory store and on PostgreSQL, and compares every status, every response body, the
//! project's events and the stored call-flow rows.
//!
//! Ignored on both sides, and only these: generated ids (UUIDs, also inside strings such as
//! `fixture-call-<uuid>` and idempotency keys; each becomes `<id:N>` numbered by first
//! appearance, so the links between records are still compared), RFC 3339 timestamps, the
//! approval's `action_payload_hash` (it hashes the ids; the replay checks Rust's against Rust's
//! own `payload_hash` of the payload instead), opaque `ref_` join-token references, and the date
//! inside "on a call on YYYY-MM-DD" (the vectors were generated on another day).
//!
//! Excluded from the Python side, and only these: the communication rows and the
//! `communication.owner_notified` events that Python's `notify_owner` writes inline. In this
//! service the Python service writes them when Rust asks it to text the owner; the replay
//! answers that request with a recording fake (asserted: the same three texts per call flow), so
//! Rust writes neither.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use regex::Regex;
use serde_json::{json, Value};
use ss_domain::contracts::{OwnerChannel, OwnerChannelStatus};
use ss_domain::integrations::telnyx::{client_state, encode_client_state};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::store::comms::OWNER_CHANNELS;
use ss_server::store::schema;
use ss_server::store::{Query, SqlValue};
use uuid::Uuid;

use common::{golden, TestApp, TestDb};

const NOTIFY_PURPOSES: [&str; 3] = ["approval_request", "handoff_request", "call_summary"];

struct Norm {
    ids: HashMap<String, usize>,
    uuid: Regex,
    ts: Regex,
    reference: Regex,
    hash: Regex,
    date: Regex,
}

impl Norm {
    fn new() -> Self {
        Norm {
            ids: HashMap::new(),
            uuid: Regex::new(
                r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
            )
            .unwrap(),
            ts: Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$")
                .unwrap(),
            reference: Regex::new(r"^ref_[A-Za-z0-9_-]{16,}$").unwrap(),
            hash: Regex::new(r"^[0-9a-f]{64}$").unwrap(),
            date: Regex::new(r"on a call on \d{4}-\d{2}-\d{2}").unwrap(),
        }
    }

    fn string(&mut self, s: &str) -> String {
        // A Telnyx `client_state` (base64 JSON with a call id): compare what it says.
        if s.len() >= 16 && s.len().is_multiple_of(4) {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s) {
                if let Ok(v @ Value::Object(_)) = serde_json::from_slice::<Value>(&bytes) {
                    if v.get("call_id").is_some() {
                        let inner = self.value(&v);
                        return format!("<client_state {inner}>");
                    }
                }
            }
        }
        if self.ts.is_match(s) {
            return "<ts>".into();
        }
        if self.reference.is_match(s) {
            return "<ref>".into();
        }
        if self.hash.is_match(s) {
            return "<hash>".into();
        }
        let s = self.date.replace_all(s, "on a call on <date>").into_owned();
        let found: Vec<String> = self
            .uuid
            .find_iter(&s)
            .map(|m| m.as_str().to_lowercase())
            .collect();
        let mut out = s.clone();
        for id in found {
            let n = self.ids.len();
            let k = *self.ids.entry(id.clone()).or_insert(n);
            out = out.replacen(&id, &format!("<id:{k}>"), 1);
        }
        out
    }

    fn value(&mut self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.string(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(m) => {
                let mut out = serde_json::Map::new();
                for (k, x) in m {
                    let key = self.string(k);
                    out.insert(key, self.value(x));
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
}

/// The first place two documents differ (path, Python, Rust).
fn first_diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                match (x.get(k), y.get(k)) {
                    (Some(p), Some(q)) => {
                        if let Some(d) = first_diff(p, q, &format!("{path}.{k}")) {
                            return Some(d);
                        }
                    }
                    (p, q) => return Some(format!("{path}.{k}: Python {p:?} Rust {q:?}")),
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                if let Some(d) = first_diff(p, q, &format!("{path}[{i}]")) {
                    return Some(d);
                }
            }
            (x.len() != y.len())
                .then(|| format!("{path}: length Python {} Rust {}", x.len(), y.len()))
        }
        _ => (a != b).then(|| format!("{path}: Python {a} Rust {b}")),
    }
}

/// `a.b[k=v].c` into a JSON document (the generator's `dig`).
fn dig<'v>(doc: &'v Value, path: &str) -> &'v Value {
    let sel = Regex::new(r"^(\w+)\[(\w+)=(\w+)\]$").unwrap();
    let mut cur = doc;
    for part in path.split('.') {
        cur = if let Some(c) = sel.captures(part) {
            cur[&c[1]]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x[&c[2]] == c[3])
                .unwrap()
        } else if let Ok(i) = part.parse::<usize>() {
            &cur[i]
        } else {
            &cur[part]
        };
    }
    cur
}

fn ref_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn resolve(text: &str, refs: &HashMap<String, String>) -> String {
    let cs = Regex::new(r"\{cs:(\w+):(\w+)\}").unwrap();
    let mut out = cs
        .replace_all(text, |c: &regex::Captures| {
            encode_client_state(&client_state(&refs[&c[1]], &c[2]))
        })
        .into_owned();
    for (k, v) in refs {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    assert!(
        !Regex::new(r"\{[a-z][a-z0-9_:]*\}").unwrap().is_match(&out),
        "unresolved reference in {out}"
    );
    out
}

fn sign(body: &[u8], seed: u8) -> (String, String) {
    let ts = chrono::Utc::now().timestamp().to_string();
    let mut msg = format!("{ts}|").into_bytes();
    msg.extend_from_slice(body);
    let sig = SigningKey::from_bytes(&[seed; 32]).sign(&msg);
    (
        base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
        ts,
    )
}

/// Sort every list of permit records by permit number. In-memory comparison only: rows written
/// in one transaction share `created_at`; PostgreSQL returns such ties in physical order on both
/// backends (compared strictly), the in-memory store in insertion order (as `wire_parity.rs`).
fn sort_permit_lists(v: &mut Value) {
    match v {
        Value::Array(a) => {
            for x in a.iter_mut() {
                sort_permit_lists(x);
            }
            if !a.is_empty() && a.iter().all(|x| x.get("permit_number").is_some()) {
                a.sort_by_key(|x| x["permit_number"].as_str().unwrap_or("").to_owned());
            }
        }
        Value::Object(m) => {
            for (_, x) in m.iter_mut() {
                sort_permit_lists(x);
            }
        }
        _ => {}
    }
}

/// Python's side of a list step without what `notify_owner` wrote.
fn without_notify_records(name: &str, v: &Value) -> Value {
    match (name, v) {
        ("events", Value::Object(page)) => {
            let events: Vec<Value> = page["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["event_type"] != "communication.owner_notified")
                .cloned()
                .collect();
            json!({"events": events})
        }
        (n, Value::Array(items)) if n.starts_with("communications") => Value::Array(
            items
                .iter()
                .filter(|c| !is_notify_comm(c))
                .cloned()
                .collect(),
        ),
        _ => v.clone(),
    }
}

fn is_notify_comm(c: &Value) -> bool {
    NOTIFY_PURPOSES.contains(&c["purpose"].as_str().unwrap_or(""))
        && c["channel"].as_str().unwrap_or("").starts_with("PHOTON")
}

async fn rust_rows(t: &TestApp) -> Value {
    let mut tx = t.app.store.begin().await.unwrap();
    let mut out = serde_json::Map::new();
    for (table, cols) in [
        (
            &schema::COMMUNICATIONS,
            &[
                "id",
                "project_id",
                "channel",
                "state",
                "data",
                "record_version",
            ][..],
        ),
        (
            &schema::CALL_SESSIONS,
            &[
                "id",
                "project_id",
                "provider_call_id",
                "state",
                "data",
                "record_version",
            ][..],
        ),
        (
            &schema::OWNER_HANDOFF_REQUESTS,
            &[
                "id",
                "project_id",
                "call_id",
                "status",
                "data",
                "record_version",
            ][..],
        ),
        (
            &schema::PROCESSED_WEBHOOK_EVENTS,
            &["provider", "provider_event_id", "verified", "payload"][..],
        ),
    ] {
        let rows = tx
            .select(table, &Query::new().order_asc("created_at"))
            .await
            .unwrap();
        let docs: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut m = serde_json::Map::new();
                for c in cols {
                    let v = match r.get(c).unwrap() {
                        SqlValue::Null => Value::Null,
                        SqlValue::Uuid(u) => Value::String(u.to_string()),
                        SqlValue::Text(s) => Value::String(s.clone()),
                        SqlValue::Int(i) => json!(i),
                        SqlValue::Bool(b) => json!(b),
                        SqlValue::Json(j) => j.clone(),
                        other => Value::String(other.to_string()),
                    };
                    m.insert((*c).to_owned(), v);
                }
                Value::Object(m)
            })
            .collect();
        out.insert(table.name.to_owned(), Value::Array(docs));
    }
    tx.commit().await.unwrap();
    Value::Object(out)
}

async fn replay(db: &TestDb) {
    let g = golden("comms_slice.json");
    let pk = base64::engine::general_purpose::STANDARD.encode(
        SigningKey::from_bytes(&[7u8; 32])
            .verifying_key()
            .to_bytes(),
    );
    let t = TestApp::new(db, &[("TELNYX_PUBLIC_KEY", pk.as_str())]);
    let fake = Arc::new(RecordingNotifier::new());
    t.app
        .extensions
        .insert(Arc::new(OwnerNotify::new(fake.clone())));
    let (token, user) = t.dev_token().await;
    // What Python's Photon code stored when the owner texted the agent.
    let ch = &g["owner_channel"];
    let mut tx = t.app.store.begin().await.unwrap();
    OWNER_CHANNELS
        .add(
            &mut *tx,
            &OwnerChannel {
                owner_channel_id: Uuid::new_v4(),
                owner_id: user,
                platform: ch["platform"].as_str().unwrap().into(),
                handle: ch["handle"].as_str().unwrap().into(),
                status: OwnerChannelStatus::Linked,
                space_id: ch["space_id"].as_str().map(str::to_owned),
                agent_user_id: ch["agent_user_id"].as_str().map(str::to_owned),
                last_inbound_message_id: ch["last_inbound_message_id"].as_str().map(str::to_owned),
                linked_at: Some(ss_core::UtcDateTime::now()),
                updated_at: ss_core::UtcDateTime::now(),
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut refs: HashMap<String, String> = HashMap::new();
    refs.insert(
        "unknown".into(),
        "00000000-0000-4000-8000-000000000001".into(),
    );
    let (mut np, mut nr) = (Norm::new(), Norm::new());
    let mut mismatches: Vec<String> = Vec::new();
    let steps = g["steps"].as_array().unwrap();
    for s in steps {
        let name = s["name"].as_str().unwrap();
        let path = resolve(s["path"].as_str().unwrap(), &refs);
        let body = s["body_text"].as_str().map(|b| resolve(b, &refs));
        let kind = s["kind"].as_str().unwrap();
        let method: Method = s["method"].as_str().unwrap().parse().unwrap();
        let mut b = Request::builder().method(method).uri(&path);
        if s["auth"] == true {
            b = b.header("authorization", format!("Bearer {token}"));
        }
        if kind.starts_with("webhook") {
            let seed = if kind == "webhook_bad_signature" {
                9
            } else {
                7
            };
            let (sig, ts) = sign(body.as_deref().unwrap().as_bytes(), seed);
            b = b
                .header("telnyx-signature-ed25519", sig)
                .header("telnyx-timestamp", ts);
        }
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let req = b
            .body(body.clone().map_or_else(Body::empty, Body::from))
            .unwrap();
        let r = t.send(req).await;
        // Captures and the property references, as the generator takes them.
        if let Some(cap) = s["capture"].as_object() {
            for (k, p) in cap {
                refs.insert(k.clone(), ref_str(dig(&r.body, p.as_str().unwrap())));
            }
        }
        if name == "properties" {
            let props = r.body.as_array().unwrap();
            let addr = |p: &Value| {
                p["property_identity"]["normalized_address"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            let pick = |want: bool| {
                props
                    .iter()
                    .find(|p| addr(p).contains("HOLLYWOOD") == want)
                    .unwrap()["property_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            refs.insert("hollywood".into(), pick(true));
            refs.insert("other_property".into(), pick(false));
        }
        // Rust's approval hashes are Rust's own payload_hash of the approved payload.
        if let Some(appr) = r.body.get("approval").filter(|a| a.is_object()) {
            let payload = appr["action_payload"].as_object().unwrap();
            assert_eq!(
                appr["action_payload_hash"],
                ss_server::contracts::payload_hash(payload),
                "{name}"
            );
        }
        if s["status"].as_u64().unwrap() as u16 != r.status.as_u16() {
            mismatches.push(format!(
                "{name}: status Python {} Rust {} ({})",
                s["status"], r.status, r.text
            ));
        }
        let py = s["response"].clone();
        let py = without_notify_records(name, &py);
        let mut rust = r.body.clone();
        if name == "events" {
            rust = json!({"events": rust["events"]});
        }
        if name.starts_with("communications") {
            assert!(
                !rust.as_array().unwrap().iter().any(is_notify_comm),
                "Rust wrote a notify record although the Python service answered"
            );
        }
        let (mut py, mut rust) = (py, rust);
        if db.kind == common::Kind::Memory {
            sort_permit_lists(&mut py);
            sort_permit_lists(&mut rust);
        }
        let (a, b) = (np.value(&py), nr.value(&rust));
        if let Some(d) = first_diff(&a, &b, "") {
            mismatches.push(format!("{name}: {d}"));
        }
    }
    // The owner was asked to be texted exactly where Python texted: one approval request per
    // outreach, one handoff request per escalation, one summary per finished call.
    t.app.owner_notify().idle().await;
    let purposes: Vec<String> = fake.requests().into_iter().map(|r| r.purpose).collect();
    let py_notify: Vec<String> = g["rows"]["communications"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| is_notify_comm(&c["data"]))
        .map(|c| c["data"]["purpose"].as_str().unwrap().to_owned())
        .collect();
    let mut a = purposes.clone();
    let mut b = py_notify.clone();
    a.sort();
    b.sort();
    assert_eq!(
        a, b,
        "notify purposes: Rust asked {purposes:?}, Python texted {py_notify:?}"
    );

    // Stored rows, with the same exclusions.
    let mut py_rows = g["rows"].clone();
    let comms: Vec<Value> = py_rows["communications"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| !is_notify_comm(&c["data"]))
        .cloned()
        .collect();
    py_rows["communications"] = Value::Array(comms);
    // Photon's own dedupe rows (the owner's linking text, polled) are Python's.
    let telnyx: Vec<Value> = py_rows["processed_webhook_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["provider"] == "telnyx")
        .cloned()
        .collect();
    py_rows["processed_webhook_events"] = Value::Array(telnyx);
    let rust = rust_rows(&t).await;
    for table in [
        "communications",
        "call_sessions",
        "owner_handoff_requests",
        "processed_webhook_events",
    ] {
        let (a, b) = (np.value(&py_rows[table]), nr.value(&rust[table]));
        let (pa, pb) = (a.as_array().unwrap(), b.as_array().unwrap());
        if pa.len() != pb.len() {
            mismatches.push(format!(
                "rows {table}: Python {} Rust {}",
                pa.len(),
                pb.len()
            ));
            continue;
        }
        for (i, (x, y)) in pa.iter().zip(pb).enumerate() {
            if let Some(d) = first_diff(x, y, "") {
                mismatches.push(format!("rows {table}[{i}]: {d}"));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} comparisons differ:\n{}",
        mismatches.len(),
        steps.len() + 4,
        mismatches.join("\n")
    );
    assert_eq!(
        np.ids.len(),
        nr.ids.len(),
        "the same number of distinct ids"
    );
}

both_backends!(call_flow_matches_python, |db| {
    replay(db).await;
});
