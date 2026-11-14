//! Mirror of `normalize()`, `expand()` and `uuids_in()` in
//! `tests-golden/domain/gen_domain2.py` (part 2 golden vectors).
//!
//! Freshly generated UUIDs become `<id:N>`, numbered by first appearance while walking the
//! document with object keys sorted (inside strings and object keys too, e.g. `work:<uuid>`);
//! UUIDs in `fixed` are kept. Non-null `calculated_at`, `retrieved_at` and `last_analyzed_at`
//! become `<timestamp>`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap()
});

pub const TS_KEYS: &[&str] = &["calculated_at", "retrieved_at", "last_analyzed_at"];

/// Every UUID string found anywhere in `doc` (`uuids_in`).
pub fn uuids_in(doc: &Value) -> HashSet<String> {
    let text = serde_json::to_string(doc).unwrap();
    UUID_RE
        .find_iter(&text)
        .map(|m| m.as_str().to_owned())
        .collect()
}

pub fn normalize_labels(doc: &Value, fixed: &HashSet<String>) -> Value {
    struct Labeler<'a> {
        fixed: &'a HashSet<String>,
        labels: HashMap<String, String>,
    }
    impl Labeler<'_> {
        fn sub(&mut self, s: &str) -> String {
            let mut out = String::with_capacity(s.len());
            let mut last = 0;
            for m in UUID_RE.find_iter(s) {
                out.push_str(&s[last..m.start()]);
                let u = m.as_str();
                if self.fixed.contains(u) {
                    out.push_str(u);
                } else {
                    let n = self.labels.len();
                    let label = self
                        .labels
                        .entry(u.to_owned())
                        .or_insert_with(|| format!("<id:{n}>"));
                    out.push_str(label);
                }
                last = m.end();
            }
            out.push_str(&s[last..]);
            out
        }

        fn walk(&mut self, v: &Value, key: Option<&str>) -> Value {
            if key.is_some_and(|k| TS_KEYS.contains(&k)) && !v.is_null() {
                return Value::String("<timestamp>".into());
            }
            match v {
                Value::Object(m) => {
                    let mut keys: Vec<&String> = m.keys().collect();
                    keys.sort();
                    let mut out = Map::new();
                    for k in keys {
                        let nk = self.sub(k);
                        let nv = self.walk(&m[k], Some(k));
                        out.insert(nk, nv);
                    }
                    Value::Object(out)
                }
                Value::Array(a) => Value::Array(a.iter().map(|x| self.walk(x, None)).collect()),
                Value::String(s) => Value::String(self.sub(s)),
                other => other.clone(),
            }
        }
    }
    Labeler {
        fixed,
        labels: HashMap::new(),
    }
    .walk(doc, None)
}

/// `expand()`: resolve `{"$base": name}`, `{"$base", "$set"}` and `{"$base", "$extra"}`.
pub fn expand(v: &Value, bases: &Value) -> Value {
    match v {
        Value::Object(m) if m.contains_key("$base") => {
            let name = m["$base"].as_str().unwrap();
            let mut b = bases[name].clone();
            assert!(!b.is_null(), "unknown base {name}");
            if let Some(set) = m.get("$set").and_then(Value::as_object) {
                let obj = b.as_object_mut().unwrap();
                for (k, x) in set {
                    obj.insert(k.clone(), x.clone());
                }
            }
            if let Some(extra) = m.get("$extra").and_then(Value::as_array) {
                b.as_array_mut().unwrap().extend(extra.iter().cloned());
            }
            b
        }
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), expand(x, bases)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| expand(x, bases)).collect()),
        other => other.clone(),
    }
}
