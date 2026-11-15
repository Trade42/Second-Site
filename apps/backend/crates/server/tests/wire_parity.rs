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
//! Since part 3 the sequence continues into the design studio (generation, list, geometry, bill of
//! materials, revisions and their error cases, then the project's events), and
//! `design_rows.json` holds the rows Python stored in the design tables. The Rust layout solver
//! satisfies the same integer model and validator as Python's OR-Tools solver but is not
//! coordinate-identical to it (domain part 3 report), so on both sides these solver-dependent
//! fields are replaced by `<solver>` before the comparison, and only these:
//! - `room_polygons[].polygon` and `room_polygons[].area_sqm` of a design version;
//! - `finish_specifications[].area_sqm` (one floor finish per room, sized to the room);
//! - the `quantity` of a bill's `flooring_lvp_sqm` lines (`round(finish area * 1.1, 1)`) and the
//!   bill's `estimated_total` (it sums them);
//! - the `estimated_total` of the `bom.generated` event's payload (the same total);
//! - the same values in the stored rows: `rooms.polygon`, `rooms.area_sqm` and the `data`
//!   documents.
//!
//! Everything else of a layout is compared strictly: room ids, types and order, the validation
//! report (validity, issues, disclaimer), approval status, version numbers and parents, the
//! geometry, the furniture lines of the bill, task outputs, events and their payloads. The
//! `project.profile.confirmed` event's idempotency key is also replaced on both sides: the shared
//! plan mandates a stable SHA-256 digest where Python used its per-process `hash()`.
//!
//! Since part 5 the sequence also reads `GET /api/v1/projects/{project}` before and after the
//! first search (the supervisor's `get_project_state` tool validates that answer strictly), and,
//! for each property after the search, the evidence list and its validation. Python has no HTTP
//! route for those two; the script computed their expected answers with Python's
//! `EvidenceService.for_property` and the supervisor tool `validate_evidence` over the rows the
//! same Python run wrote, and the replay compares this service's routes with them.
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
use uuid::Uuid;

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

const SOLVER: &str = "<solver>";

fn mask(v: &mut Value, key: &str) {
    if let Some(x) = v.get_mut(key) {
        *x = Value::String(SOLVER.into());
    }
}

/// Replace the solver-dependent fields (see the module comment) wherever they occur.
fn mask_solver_fields(v: &mut Value) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(mask_solver_fields),
        Value::Object(m) => {
            if let Some(Value::Array(rooms)) = m.get_mut("room_polygons") {
                for r in rooms {
                    mask(r, "polygon");
                    mask(r, "area_sqm");
                }
            }
            if let Some(Value::Array(finishes)) = m.get_mut("finish_specifications") {
                for f in finishes {
                    mask(f, "area_sqm");
                }
            }
            if m.contains_key("bom_id") && m.contains_key("items") {
                if let Some(x) = m.get_mut("estimated_total") {
                    *x = Value::String(SOLVER.into());
                }
                if let Some(Value::Array(items)) = m.get_mut("items") {
                    for i in items {
                        if i["item_type"] == "flooring_lvp_sqm" {
                            mask(i, "quantity");
                        }
                    }
                }
            }
            // A stored `rooms` row.
            if m.contains_key("room_key") && m.contains_key("polygon") {
                m.insert("polygon".into(), Value::String(SOLVER.into()));
                m.insert("area_sqm".into(), Value::String(SOLVER.into()));
            }
            // `bom.generated` carries the bill's total.
            if m.get("event_type") == Some(&Value::String("bom.generated".into())) {
                if let Some(p) = m.get_mut("payload") {
                    mask(p, "estimated_total");
                }
            }
            if m.get("event_type") == Some(&Value::String("project.profile.confirmed".into())) {
                m.insert(
                    "idempotency_key".into(),
                    Value::String("<profile-confirmed-key>".into()),
                );
            }
            for (_, x) in m.iter_mut() {
                mask_solver_fields(x);
            }
        }
        _ => {}
    }
}

/// Body strings that are exactly one `{reference}` are resolved, as the Python script does.
fn resolve_body(v: &Value, refs: &HashMap<String, String>) -> Value {
    match v {
        Value::String(s) if s.starts_with('{') && s.ends_with('}') => {
            let key = &s[1..s.len() - 1];
            match refs.get(key) {
                Some(r) if !key.contains(['{', '}']) => Value::String(r.clone()),
                _ => v.clone(),
            }
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve_body(x, refs)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), resolve_body(x, refs)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The design rows of `design_rows.json`, read from this backend's store the same way: every
/// column but the timestamps, in the same order.
async fn stored_design_rows(t: &TestApp) -> Value {
    use ss_server::store::{schema, Query, Row, SqlValue};
    fn json_of(v: &SqlValue) -> Value {
        match v {
            SqlValue::Null => Value::Null,
            SqlValue::Uuid(u) => Value::String(u.to_string()),
            SqlValue::Text(s) => Value::String(s.clone()),
            SqlValue::Int(i) => Value::from(*i),
            SqlValue::Bool(b) => Value::Bool(*b),
            SqlValue::Float(f) => serde_json::json!(f),
            SqlValue::Json(j) => j.clone(),
            SqlValue::Timestamp(ts) => Value::String(ts.to_rfc3339()),
        }
    }
    fn pick(row: &Row, cols: &[&str]) -> Value {
        let mut m = Map::new();
        for c in cols {
            m.insert((*c).into(), json_of(row.get(c).expect("column")));
        }
        Value::Object(m)
    }
    let mut tx = t.app.store.begin().await.unwrap();
    let q = Query::new().order_asc("created_at");
    let geometries = tx.select(&schema::SITE_GEOMETRIES, &q).await.unwrap();
    let mut designs = tx.select(&schema::DESIGN_VERSIONS, &q).await.unwrap();
    let mut rooms = tx.select(&schema::ROOMS, &q).await.unwrap();
    let mut boms = tx.select(&schema::BILLS_OF_MATERIALS, &q).await.unwrap();
    tx.rollback().await.unwrap();
    let version_of = |dv: &Uuid, designs: &[Row]| -> i64 {
        designs
            .iter()
            .find(|d| d.uuid("id").unwrap() == *dv)
            .map(|d| d.int("version_number").unwrap())
            .unwrap()
    };
    designs.sort_by_key(|d| {
        (
            d.uuid("property_id").unwrap(),
            d.int("version_number").unwrap(),
        )
    });
    rooms.sort_by_key(|r| {
        (
            version_of(&r.uuid("design_version_id").unwrap(), &designs),
            r.text("room_key").unwrap(),
        )
    });
    // Stable: bills of one version keep `created_at` order.
    boms.sort_by_key(|b| version_of(&b.uuid("design_version_id").unwrap(), &designs));
    serde_json::json!({
        "site_geometries": geometries.iter().map(|r| pick(r, &["id", "property_id", "scale_verified", "data", "schema_version", "record_version"])).collect::<Vec<_>>(),
        "design_versions": designs.iter().map(|r| pick(r, &["id", "project_id", "property_id", "parent_version_id", "version_number", "approval_status", "valid", "data", "schema_version", "record_version"])).collect::<Vec<_>>(),
        "rooms": rooms.iter().map(|r| pick(r, &["id", "design_version_id", "room_key", "room_type", "area_sqm", "polygon"])).collect::<Vec<_>>(),
        "bills_of_materials": boms.iter().map(|r| pick(r, &["id", "project_id", "design_version_id", "status", "data", "schema_version", "record_version"])).collect::<Vec<_>>(),
    })
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
    // 27 property-slice steps, 23 design-studio steps (part 3), and since part 5 the project
    // before and after the first search plus the two evidence routes of each property, whose
    // expected answers Python computed in-process (their "oracle" field says how).
    assert_eq!(steps.len(), 56);
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
        let body = (!s["body"].is_null()).then(|| resolve_body(&s["body"], &refs));
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
            status_mismatches.push(format!(
                "{name}: python {status} != rust {} ({})",
                r.status, r.text
            ));
        }
        // The references the Python script records, recorded the same way.
        match name {
            "dev_token" => token = r.body["access_token"].as_str().map(str::to_owned),
            "create_project" => {
                refs.insert(
                    "project".into(),
                    r.body["project"]["project_id"].as_str().unwrap().into(),
                );
            }
            "create_project_without_profile" => {
                refs.insert(
                    "project_bare".into(),
                    r.body["project"]["project_id"].as_str().unwrap().into(),
                );
            }
            "search" => {
                refs.insert(
                    "status_url".into(),
                    r.body["status_url"].as_str().unwrap().into(),
                );
            }
            "generate layouts" => {
                refs.insert(
                    "layout_status_url".into(),
                    r.body["status_url"].as_str().unwrap().into(),
                );
            }
            "layouts" => {
                for lv in r.body.as_array().unwrap() {
                    refs.insert(
                        format!("layout:{}", lv["version_number"]),
                        lv["design_version_id"].as_str().unwrap().into(),
                    );
                }
                refs.insert(
                    "geometry".into(),
                    r.body[0]["geometry_id"].as_str().unwrap().into(),
                );
            }
            "properties" => {
                for p in r.body.as_array().unwrap() {
                    refs.insert(
                        format!(
                            "property:{}",
                            p["property_identity"]["street_address"].as_str().unwrap()
                        ),
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
    for v in py_all.iter_mut().chain(rs_all.iter_mut()) {
        mask_solver_fields(v);
    }
    // The stored design rows, last (so their ids are numbered after the responses').
    let mut py_rows = common::golden("design_rows.json");
    let mut rs_rows = stored_design_rows(&t).await;
    mask_solver_fields(&mut py_rows);
    mask_solver_fields(&mut rs_rows);
    for table in [
        "site_geometries",
        "design_versions",
        "rooms",
        "bills_of_materials",
    ] {
        py_all.push(py_rows[table].clone());
        rs_all.push(rs_rows[table].clone());
        names.push(format!("stored rows: {table}"));
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

/// Rows Python wrote load in Rust: every `data` document of `design_rows.json` parses through the
/// Rust contract (validated), serializes back to the same document, and gives the same indexed
/// columns as the row Python stored (`registry.py`'s column lambdas).
#[test]
fn python_design_rows_load_in_rust() {
    use ss_domain::contracts::{BillOfMaterials, DesignVersion, SiteGeometry};
    use ss_server::store::repo::Document;
    use ss_server::store::SqlValue;

    fn check<C: Document>(row: &Value, cols: &[&str]) {
        let doc = C::from_json_value(row["data"].clone())
            .unwrap_or_else(|e| panic!("{}: {e:?}", C::NAME));
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            row["data"],
            "{}",
            C::NAME
        );
        assert_eq!(doc.id().to_string(), row["id"].as_str().unwrap());
        let mine = doc.columns();
        for c in cols {
            let want = &row[*c];
            let got = match mine.get(c).unwrap_or(&SqlValue::Null) {
                SqlValue::Null => Value::Null,
                SqlValue::Uuid(u) => Value::String(u.to_string()),
                SqlValue::Text(s) => Value::String(s.clone()),
                SqlValue::Int(i) => Value::from(*i),
                SqlValue::Bool(b) => Value::Bool(*b),
                other => panic!("{c}: {other:?}"),
            };
            assert_eq!(&got, want, "{}.{c}", C::NAME);
        }
        assert_eq!(row["schema_version"], "1.0");
    }
    let rows = golden("design_rows.json");
    for r in rows["site_geometries"].as_array().unwrap() {
        check::<SiteGeometry>(r, &["property_id", "scale_verified"]);
    }
    let designs = rows["design_versions"].as_array().unwrap();
    assert_eq!(designs.len(), 4);
    for r in designs {
        check::<DesignVersion>(
            r,
            &[
                "project_id",
                "property_id",
                "parent_version_id",
                "version_number",
                "approval_status",
                "valid",
            ],
        );
    }
    for r in rows["bills_of_materials"].as_array().unwrap() {
        check::<BillOfMaterials>(r, &["project_id", "design_version_id", "status"]);
    }
    // Python's `rooms` rows mirror the room polygons of their design version.
    for room in rows["rooms"].as_array().unwrap() {
        let dv = designs
            .iter()
            .find(|d| d["id"] == room["design_version_id"])
            .unwrap();
        let poly = dv["data"]["room_polygons"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["room_id"] == room["room_key"])
            .unwrap();
        assert_eq!(room["room_type"], poly["room_type"]);
        assert_eq!(room["area_sqm"], poly["area_sqm"]);
        assert_eq!(room["polygon"], poly["polygon"]);
    }
}
