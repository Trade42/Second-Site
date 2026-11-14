//! Wire parity with the Python API on the demo path. `tests-golden/server/property_slice.json`
//! holds the raw responses of the Python app (FastAPI TestClient, fixture providers, PostgreSQL),
//! produced by `tests-golden/server/gen_property_slice.py`. This test replays the same requests
//! in-process and compares status codes and JSON bodies.
//!
//! Both sides go through the same normalization, and only these fields are ignored:
//! - every UUID (fresh `uuid4` on both sides), in values, object keys and inside strings such as
//!   `status_url` or the ortho URL, is replaced by `<id:N>`, numbered by first appearance over the
//!   whole sequence (objects walked in key order), so the links between records are still
//!   compared;
//! - every RFC 3339 timestamp string becomes `<ts>` (`created_at`, `updated_at`, `retrieved_at`,
//!   `calculated_at`, `last_analyzed_at`, ...);
//! - `access_token` becomes `<token>` (it embeds a fresh user id and expiry).
//!
//! Everything else is compared strictly: JSON numbers keep their type (`5.0` is not `5`), lists
//! keep their order. Object key order is not compared (JSON objects are unordered; `jsonb` does
//! not keep it either).
//!
//! One store-dependent order: rows written in one transaction share `created_at`, and
//! `ORDER BY created_at` returns such ties in the database's physical order. Python on
//! PostgreSQL and this backend on PostgreSQL write the same rows and get the same order (compared
//! strictly). The in-memory store returns ties in insertion order, which differs for the permit
//! records of one property, so on the in-memory store permit lists are compared sorted by
//! permit number (on both sides, before normalization).

mod common;

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::http::Method;
use regex::Regex;
use serde_json::{Map, Value};

use common::*;

static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap()
});
static TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,9})?(Z|[+-]\d{2}:\d{2})?$").unwrap()
});

#[derive(Default)]
struct Normalizer {
    ids: HashMap<String, usize>,
}

impl Normalizer {
    fn text(&mut self, s: &str) -> String {
        if TS.is_match(s) {
            return "<ts>".into();
        }
        let ids = &mut self.ids;
        UUID.replace_all(s, |c: &regex::Captures| {
            let n = ids.len();
            let id = *ids.entry(c[0].to_owned()).or_insert(n);
            format!("<id:{id}>")
        })
        .into_owned()
    }

    fn value(&mut self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    let nv = if k == "access_token" {
                        Value::String("<token>".into())
                    } else {
                        self.value(&m[k])
                    };
                    let nk = self.text(k);
                    out.insert(nk, nv);
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
}

/// The first differences between two normalized values (`path: python != rust`).
fn diff(path: &str, py: &Value, rs: &Value, out: &mut Vec<String>) {
    if out.len() >= 40 {
        return;
    }
    match (py, rs) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                match (a.get(k), b.get(k)) {
                    (Some(x), Some(y)) => diff(&format!("{path}.{k}"), x, y, out),
                    (x, y) => out.push(format!("{path}.{k}: python {x:?} != rust {y:?}")),
                }
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                diff(&format!("{path}[{i}]"), x, y, out);
            }
        }
        (x, y) if x != y => out.push(format!("{path}: python {x} != rust {y}")),
        _ => {}
    }
}

/// Sort every list of permit records by permit number (in-memory comparison only, see above).
fn sort_permit_lists(v: &mut Value) {
    match v {
        Value::Array(a) => {
            for x in a.iter_mut() {
                sort_permit_lists(x);
            }
            if !a.is_empty() && a.iter().all(|x| x.get("permit_number").is_some()) {
                a.sort_by(|x, y| {
                    let k = |v: &Value| v["permit_number"].as_str().unwrap_or("").to_owned();
                    k(x).cmp(&k(y))
                });
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

fn resolve(path: &str, refs: &HashMap<String, String>) -> String {
    let mut p = path.to_owned();
    for (k, v) in refs {
        p = p.replace(&format!("{{{k}}}"), v);
    }
    assert!(!p.contains('{'), "unresolved reference in {path}");
    p
}

both_backends!(property_slice_matches_python, |db| {
    let golden = golden("property_slice.json");
    let steps = golden.as_array().expect("steps");
    assert_eq!(steps.len(), 27);
    let t = TestApp::new(db, &[]);
    let mut refs: HashMap<String, String> = HashMap::new();
    let mut token: Option<String> = None;
    let (mut py_all, mut rs_all, mut names) = (Vec::new(), Vec::new(), Vec::new());
    let mut status_mismatches = Vec::new();
    for s in steps {
        let name = s["name"].as_str().unwrap();
        let method: Method = s["method"].as_str().unwrap().parse().unwrap();
        let path = resolve(s["path"].as_str().unwrap(), &refs);
        let auth = s["auth"].as_bool().unwrap();
        let body = (!s["body"].is_null()).then(|| s["body"].clone());
        let r = t
            .call(
                method,
                &path,
                if auth { token.as_deref() } else { None },
                body.as_ref(),
            )
            .await;
        let status = s["status"].as_u64().unwrap() as u16;
        if r.status.as_u16() != status {
            status_mismatches.push(format!("{name}: python {status} != rust {} ({})", r.status, r.text));
        }
        // The references the Python script records, recorded the same way.
        match name {
            "dev_token" => token = r.body["access_token"].as_str().map(str::to_owned),
            "create_project" => {
                refs.insert("project".into(), r.body["project"]["project_id"].as_str().unwrap().into());
            }
            "create_project_without_profile" => {
                refs.insert("project_bare".into(), r.body["project"]["project_id"].as_str().unwrap().into());
            }
            "search" => {
                refs.insert("status_url".into(), r.body["status_url"].as_str().unwrap().into());
            }
            "properties" => {
                for p in r.body.as_array().unwrap() {
                    refs.insert(
                        format!("property:{}", p["property_identity"]["street_address"].as_str().unwrap()),
                        p["property_id"].as_str().unwrap().into(),
                    );
                }
            }
            _ => {}
        }
        py_all.push(s["response"].clone());
        rs_all.push(r.body.clone());
        names.push(name.to_owned());
    }
    assert!(status_mismatches.is_empty(), "{status_mismatches:#?}");
    let mut n_py = Normalizer::default();
    let mut n_rs = Normalizer::default();
    let mut problems = Vec::new();
    if t.kind == Kind::Memory {
        for v in py_all.iter_mut().chain(rs_all.iter_mut()) {
            sort_permit_lists(v);
        }
    }
    for ((name, py), rs) in names.iter().zip(&py_all).zip(&rs_all) {
        let a = n_py.value(py);
        let b = n_rs.value(rs);
        let mut d = Vec::new();
        diff("", &a, &b, &mut d);
        if !d.is_empty() {
            problems.push(format!("step {name}:\n  {}", d.join("\n  ")));
        }
    }
    assert!(
        problems.is_empty(),
        "{} of {} steps differ from Python:\n{}",
        problems.len(),
        names.len(),
        problems.join("\n")
    );
    // The same UUIDs were linked the same way on both sides.
    assert_eq!(n_py.ids.len(), n_rs.ids.len());
});
