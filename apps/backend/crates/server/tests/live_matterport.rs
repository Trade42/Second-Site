//! Opt-in live proof of the Matterport adapter against real models (the sponsor's public sample
//! model `3QuftVC2agD` and showcase model `SxQL3iGyoDo`).
//!
//! Never runs by default: the test is `#[ignore]`, and even with `--ignored` it returns early
//! unless `LIVE_MATTERPORT=1` is set and the Matterport credentials are configured. Credentials
//! come only from the crate's settings loader, reading the `.env` files exactly as the service does
//! when it runs from `apps/backend` (`.env`, then `../../.env`, then the process environment). The
//! test never prints a setting, a header or a request body; it prints model ids, counts, labels,
//! the adapter's error text and the shape (keys, `errors` entries) of the real answer.
//!
//! Run (from `apps/backend`):
//! `LIVE_MATTERPORT=1 cargo test -p ss-server --test live_matterport -- --ignored --nocapture`.
//! `LIVE_MATTERPORT_STEPS` (default `2,3,4`) picks the steps; `LIVE_MATTERPORT_SUITE` sets the
//! synthetic listing's suite in step 4 (default: none). The test makes at most 8 Matterport
//! requests (each one is counted in the output); it downloads no assets.
//!
//! Step 4 pairs a real scan with a synthetic listing (labelled `FIXTURE_SYNTHETIC`, "Not a CoStar
//! record") for this test only, on the in-memory store: nothing is stored anywhere else.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};
use ss_domain::design::geometry::framing::normalize_plan;
use ss_server::config::{ProviderMode, Settings};
use ss_server::integrations::http::HttpClient;
use ss_server::integrations::matterport::{
    default_policy, suite_matches, MatterportProvider, GEOMETRY_QUERY, SUMMARY_FIELDS,
};
use ss_server::integrations::Integrations;
use ss_server::services::design::site_geometry_from_doc;
use ss_server::store::repo::GEOMETRIES;
use ss_server::store::Query;
use uuid::Uuid;

use common::*;

const SAMPLE: &str = "3QuftVC2agD";
const SHOWCASE: &str = "SxQL3iGyoDo";
const INVENTED: &str = "zzzzzzzzzzz";

/// Matterport requests made by this test (adapter calls and shape probes). GraphQL errors are not
/// retried by the adapter; a transport error could add one retry per call, which is reported.
static REQUESTS: AtomicUsize = AtomicUsize::new(0);
const MAX_REQUESTS: usize = 8;

fn count(what: &str) {
    let n = REQUESTS.fetch_add(1, Ordering::SeqCst) + 1;
    assert!(n <= MAX_REQUESTS, "request budget exceeded at {what}");
    println!("[request {n}] {what}");
}

/// `apps/backend`, where the service runs; `../../.env` from there is the repository `.env`.
fn service_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The service's settings with Matterport switched to live (the mode only; credentials and URL
/// come from the loader). `None` (and a message) when the test should not run.
fn live_settings() -> Option<Settings> {
    if std::env::var("LIVE_MATTERPORT").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set LIVE_MATTERPORT=1 to call the real Matterport API");
        return None;
    }
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.push(("MATTERPORT_PROVIDER_MODE".into(), "live".into()));
    let s = Settings::from_sources(&service_dir(), env).expect("settings load");
    assert_eq!(s.matterport_provider_mode, ProviderMode::Live);
    let set =
        |v: &Option<ss_server::config::Secret>| v.as_ref().is_some_and(|x| !x.expose().is_empty());
    if !(set(&s.matterport_api_key) && set(&s.matterport_api_token)) {
        eprintln!("SKIPPED: MATTERPORT_API_KEY / MATTERPORT_API_TOKEN are not configured");
        return None;
    }
    Some(s)
}

/// One raw POST of `payload` with the adapter's own header and policy, for the answer's shape only
/// (the adapter keeps just `data` or an error message). Returns the parsed body.
fn raw_shape(s: &Settings, payload: &Value, what: &str) -> Value {
    let id = s
        .matterport_api_key
        .as_ref()
        .map(|v| v.expose())
        .unwrap_or("");
    let secret = s
        .matterport_api_token
        .as_ref()
        .map(|v| v.expose())
        .unwrap_or("");
    use base64::Engine as _;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    let client = HttpClient::new(
        "matterport",
        default_policy(),
        vec![("Authorization".to_owned(), format!("Basic {basic}"))],
    );
    count(what);
    match client.post_json_once(&s.matterport_graph_url, payload) {
        Ok(r) => {
            println!(
                "  raw: HTTP {} content-type {:?}, {} bytes",
                r.status,
                r.content_type,
                r.body.len()
            );
            r.json("matterport").unwrap_or(Value::Null)
        }
        Err(e) => {
            println!("  raw: transport/HTTP error: {e}");
            Value::Null
        }
    }
}

/// Keys, `data` (null or which aliases are null) and every `errors` entry's message, path,
/// extensions and locations.
fn describe_shape(body: &Value) {
    let Value::Object(b) = body else {
        println!("  shape: body is not an object");
        return;
    };
    println!("  shape: top-level keys {:?}", b.keys().collect::<Vec<_>>());
    match b.get("data") {
        None => println!("  shape: no `data` key"),
        Some(Value::Null) => println!("  shape: data = null"),
        Some(Value::Object(d)) => {
            for (k, v) in d {
                let what = match v {
                    Value::Null => "null".to_owned(),
                    Value::Object(m) => {
                        format!("object with keys {:?}", m.keys().collect::<Vec<_>>())
                    }
                    other => format!("{other}"),
                };
                println!("  shape: data.{k} = {what}");
            }
        }
        Some(other) => println!("  shape: data = {other}"),
    }
    if let Some(Value::Array(errs)) = b.get("errors") {
        for (i, e) in errs.iter().enumerate() {
            println!(
                "  shape: errors[{i}]: message={} path={} extensions={} locations={}",
                e.get("message").unwrap_or(&Value::Null),
                e.get("path").unwrap_or(&Value::Null),
                e.get("extensions").unwrap_or(&Value::Null),
                e.get("locations").unwrap_or(&Value::Null),
            );
        }
    }
}

fn summaries_query(ids: &[&str]) -> String {
    let parts: Vec<String> = ids
        .iter()
        .enumerate()
        .map(|(j, m)| format!("m{j}: model(id: \"{m}\") {{ {SUMMARY_FIELDS} }}"))
        .collect();
    format!("query {{{}}}", parts.join(" "))
}

/// Bounding box `(width, depth)` and shoelace area of a ring of `[x, y]` points.
fn ring_stats(ring: &Value) -> (usize, f64, f64, f64) {
    let pts: Vec<(f64, f64)> = ring
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
                .collect()
        })
        .unwrap_or_default();
    if pts.is_empty() {
        return (0, 0.0, 0.0, 0.0);
    }
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    let mut a = 0.0;
    for (i, p) in pts.iter().enumerate() {
        let q = pts[(i + 1) % pts.len()];
        a += p.0 * q.1 - q.0 * p.1;
        x0 = x0.min(p.0);
        x1 = x1.max(p.0);
        y0 = y0.min(p.1);
        y1 = y1.max(p.1);
    }
    (pts.len(), x1 - x0, y1 - y0, (a / 2.0).abs())
}

fn len(doc: &Map<String, Value>, k: &str) -> usize {
    doc.get(k).and_then(Value::as_array).map_or(0, Vec::len)
}

fn describe_doc(label: &str, doc: &Map<String, Value>) {
    println!("  {label}: keys {:?}", doc.keys().collect::<Vec<_>>());
    for k in [
        "source_id",
        "geometry_source",
        "data_origin",
        "units",
        "coordinate_reference",
        "scale_verified",
        "measurement_uncertainty_m",
        "model_name",
        "floor_label",
        "floor_count",
        "notes",
    ] {
        if let Some(v) = doc.get(k) {
            println!("  {label}.{k} = {v}");
        }
    }
    let (n, w, d, a) = ring_stats(doc.get("floor_boundary").unwrap_or(&Value::Null));
    println!("  {label}.floor_boundary: {n} points, bbox {w:.2} x {d:.2}, area {a:.2}");
    for k in [
        "fixed_walls",
        "fixed_obstacles",
        "entrances",
        "exits",
        "windows",
        "fixture_locations",
        "interior_doors",
        "rooms",
    ] {
        if doc.contains_key(k) {
            println!("  {label}.{k}: {} entries", len(doc, k));
        }
    }
    if let Some(Value::Array(rooms)) = doc.get("rooms") {
        for r in rooms.iter().take(30) {
            println!("    room {r}");
        }
    }
}

/// Step 3: the adapter's document, then `prepare_site`'s own conversion and the contract check.
/// Returns `true` when a valid `SiteGeometry` came out.
fn geometry_step(mp: &dyn MatterportProvider, s: &Settings, id: &str) -> Option<bool> {
    count(&format!("get_floor_geometry({id})"));
    let got = mp.get_floor_geometry(id);
    let doc = match got {
        Err(e) => {
            println!(
                "STEP 3 {id}: adapter ERROR kind={} message={}",
                e.kind, e.message
            );
            let body = raw_shape(
                s,
                &json!({"query": GEOMETRY_QUERY, "variables": {"id": id}}),
                &format!("raw ModelGeometry({id}) for the answer's shape"),
            );
            describe_shape(&body);
            return None;
        }
        Ok(None) => {
            println!("STEP 3 {id}: adapter answered None (no usable floor)");
            return Some(false);
        }
        Ok(Some(doc)) => doc,
    };
    let Value::Object(obj) = doc else {
        println!("STEP 3 {id}: document is not an object: {doc}");
        return Some(false);
    };
    println!("STEP 3 {id}: adapter answered Some(document)");
    describe_doc("doc", &obj);
    let live = obj.get("data_origin").and_then(Value::as_str) == Some("LIVE");
    let obj = if live {
        match normalize_plan(&Value::Object(obj)) {
            Ok(Value::Object(m)) => m,
            Ok(other) => {
                println!("STEP 3 {id}: normalize_plan returned a non-object: {other}");
                return Some(false);
            }
            Err(e) => {
                println!("STEP 3 {id}: normalize_plan ERROR {e:?}");
                return Some(false);
            }
        }
    } else {
        obj
    };
    describe_doc("normalized", &obj);
    match site_geometry_from_doc(Uuid::new_v4(), id, &obj, live) {
        Ok(g) => {
            let v = serde_json::to_value(&g).expect("SiteGeometry serializes");
            assert_conforms("SiteGeometry", &v);
            let (n, w, d, a) = ring_stats(&v["floor_boundary"]);
            println!(
                "STEP 3 {id}: SiteGeometry VALID (contract checked): data_origin={} geometry_source={} units={} scale_verified={} uncertainty={} boundary {n} pts {w:.2} x {d:.2} m, {a:.2} m2; walls {} windows {} entrances {} exits {}",
                v["data_origin"], v["geometry_source"], v["units"], v["scale_verified"],
                v["measurement_uncertainty_m"],
                v["fixed_walls"].as_array().map_or(0, Vec::len),
                v["windows"].as_array().map_or(0, Vec::len),
                v["entrances"].as_array().map_or(0, Vec::len),
                v["exits"].as_array().map_or(0, Vec::len),
            );
            Some(true)
        }
        Err(e) => {
            println!("STEP 3 {id}: site_geometry_from_doc ERROR {e:?}");
            Some(false)
        }
    }
}

/// A scratch fixtures directory: the repository fixtures plus one synthetic listing whose interior
/// asset is the real model `id`.
fn scratch_fixtures(id: &str, suite: Option<&str>) -> PathBuf {
    let src = repo_root().join("fixtures");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("live-matterport-{}", Uuid::new_v4()));
    for sub in ["permits", "geometry", "properties"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
        for e in std::fs::read_dir(src.join(sub)).unwrap() {
            let p = e.unwrap().path();
            if p.is_file() {
                std::fs::copy(&p, dir.join(sub).join(p.file_name().unwrap())).unwrap();
            }
        }
    }
    let mut doc: Value = serde_json::from_str(
        &std::fs::read_to_string(src.join("properties/la_candidates.json")).unwrap(),
    )
    .unwrap();
    let mut listing = doc["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["fixture_key"] == "la-hollywood-6922-302")
        .expect("Hollywood fixture listing")
        .clone();
    listing["fixture_key"] = json!("live-matterport-proof");
    listing["unit_or_suite"] = suite.map_or(Value::Null, |s| json!(s));
    listing["interior_assets"] = json!([{
        "provider": "matterport",
        "asset_id": id,
        "asset_type": "matterport_model",
        "has_measured_geometry": true,
        "note": "Real Matterport sample model paired with this synthetic listing for a live test only."
    }]);
    doc["properties"] = json!([listing]);
    std::fs::write(
        dir.join("properties/la_candidates.json"),
        serde_json::to_string_pretty(&doc).unwrap(),
    )
    .unwrap();
    dir
}

async fn layout_step(mp: Arc<dyn MatterportProvider>, id: &str) {
    let suite = std::env::var("LIVE_MATTERPORT_SUITE")
        .ok()
        .filter(|s| !s.is_empty());
    let fixtures = scratch_fixtures(id, suite.as_deref());
    let db = TestDb::new(Kind::Memory).await.expect("memory store");
    let t = TestApp::new(&db, &[("FIXTURES_DIR", fixtures.to_str().unwrap())]);
    // `LIVE_MATTERPORT_SEARCH=fixture`: the search runs with the fixture Matterport provider, so
    // the listing keeps its own `has_measured_geometry: true` (enrichment only changes assets when
    // the provider is live); the live provider is put in only for the layout. Test-only pairing.
    let fixture_search = std::env::var("LIVE_MATTERPORT_SEARCH").as_deref() == Ok("fixture");
    let mut integrations = Integrations::from_settings(&t.app.settings);
    if !fixture_search {
        integrations.matterport = mp.clone();
    }
    t.app.extensions.insert(Arc::new(integrations));
    let (tok, _) = t.dev_token().await;
    let pid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    if fixture_search {
        println!("STEP 4 search with the FIXTURE Matterport provider (no request)");
    } else {
        count(&format!(
            "property search: enrich_interior -> model_summaries([{id}])"
        ));
    }
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    println!("STEP 4 search task status {}", r.body["task"]["status"]);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    let prop = props
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    println!(
        "STEP 4 listing: suite={} listing provider={} disclaimer={} manifest origins={:?}",
        prop["property_identity"]["unit_or_suite"],
        prop["commercial_listing"]["provider"],
        prop["commercial_listing"]["disclaimer"],
        prop["source_manifest"]
            .as_array()
            .map(|a| a.iter().map(|m| m.to_string()).collect::<Vec<_>>()),
    );
    for a in prop["interior_assets"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        println!("STEP 4 interior asset after enrichment: {a}");
    }
    println!(
        "STEP 4 missing_information: {}",
        prop["missing_information"]
    );
    if let Some(s) = &suite {
        let name = prop["interior_assets"][0]["label"].as_str();
        println!(
            "STEP 4 suite_matches({name:?}, {s:?}) = {}",
            suite_matches(name, Some(s))
        );
    }
    let property_id = prop["property_id"].as_str().unwrap_or_default().to_owned();
    if fixture_search {
        let mut integrations = Integrations::from_settings(&t.app.settings);
        integrations.matterport = mp;
        t.app.extensions.insert(Arc::new(integrations));
        println!("STEP 4 live Matterport provider put in for the layout");
    }
    println!(
        "STEP 4 layout: get_floor_geometry({id}) is answered from the adapter's cache (no request)"
    );
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/layouts/generate"),
            Some(&tok),
            &json!({
                "property_id": property_id,
                "room_requirements": [
                    {"room_type": "treatment", "count": 2, "min_area_sqm": 8, "min_width_m": 2.4},
                    {"room_type": "reception", "count": 1, "min_area_sqm": 8, "min_width_m": 2.4},
                ],
                "candidate_count": 1,
            }),
        )
        .await;
    println!(
        "STEP 4 generate: HTTP {} task status {} error {} output {}",
        r.status,
        r.body["task"]["status"],
        r.body["task"]["error"],
        r.body["task"]["output_reference"]
    );
    if r.body["task"]["status"] != "COMPLETED" {
        println!(
            "STEP 4 generate body: {}",
            r.text.chars().take(1500).collect::<String>()
        );
    }
    let lv = t
        .get(&format!("/api/v1/projects/{pid}/layouts"), Some(&tok))
        .await
        .body;
    for l in lv.as_array().cloned().unwrap_or_default() {
        println!(
            "STEP 4 layout v{}: approval_status={} valid={} rooms={} disclaimer={} issues={}",
            l["version_number"],
            l["approval_status"],
            l["validation_report"]["valid"],
            l["room_polygons"].as_array().map_or(0, Vec::len),
            l["validation_report"]["disclaimer"],
            l["validation_report"]["issues"],
        );
        let mut slim = l.clone();
        if let Value::Object(m) = &mut slim {
            m.remove("room_polygons");
            m.remove("circulation_paths");
        }
        println!(
            "STEP 4 layout keys {:?}",
            l.as_object().map(|m| m.keys().collect::<Vec<_>>())
        );
        println!(
            "STEP 4 layout (rooms and paths left out): {}",
            slim.to_string().chars().take(3000).collect::<String>()
        );
        for k in [
            "data_origin",
            "geometry_source",
            "scale_verified",
            "labels",
            "assumptions",
        ] {
            if let Some(v) = l.get(k).or_else(|| l["validation_report"].get(k)) {
                println!("STEP 4 layout.{k} = {v}");
            }
        }
    }
    let mut tx = t.app.store.begin().await.unwrap();
    for g in GEOMETRIES.list(&mut *tx, Query::new()).await.unwrap() {
        let v = serde_json::to_value(&g).unwrap();
        let (n, w, d, a) = ring_stats(&v["floor_boundary"]);
        println!(
            "STEP 4 stored geometry: source_id={} data_origin={} geometry_source={} scale_verified={} uncertainty={} boundary {n} pts {w:.2} x {d:.2} m {a:.2} m2",
            v["source_id"], v["data_origin"], v["geometry_source"], v["scale_verified"], v["measurement_uncertainty_m"]
        );
    }
    tx.rollback().await.unwrap();
    db.finish().await;
    let _ = std::fs::remove_dir_all(&fixtures);
}

#[tokio::test]
#[ignore = "calls the real Matterport API; run with LIVE_MATTERPORT=1 and --ignored"]
async fn live_matterport_proof() {
    let Some(s) = live_settings() else { return };
    let steps = std::env::var("LIVE_MATTERPORT_STEPS").unwrap_or_else(|_| "2,3,4".into());
    let want = |n: &str| steps.split(',').any(|x| x.trim() == n);
    // The provider exactly as the service builds it.
    let mp: Arc<dyn MatterportProvider> = Integrations::from_settings(&s).matterport;
    assert_eq!(mp.mode(), "live");
    let st = mp.status();
    println!(
        "status: {:?} credentials_present={} note={}",
        st.status, st.credentials_present, st.note
    );

    if want("2") {
        let ids: Vec<String> = vec![SAMPLE.into(), SHOWCASE.into()];
        count("model_summaries([sample, showcase])");
        match mp.model_summaries(&ids) {
            Ok(m) => {
                println!("STEP 2a: {} of 2 summaries", m.len());
                for (k, v) in &m {
                    println!(
                        "  {k}: name={:?} area_sqm={:?} floor_count={}",
                        v.name, v.area_sqm, v.floor_count
                    );
                }
            }
            Err(e) => println!(
                "STEP 2a: adapter ERROR kind={} message={}",
                e.kind, e.message
            ),
        }
        let ids3: Vec<String> = vec![SAMPLE.into(), SHOWCASE.into(), INVENTED.into()];
        count("model_summaries([sample, showcase, invented])");
        match mp.model_summaries(&ids3) {
            Ok(m) => println!(
                "STEP 2b: {} of 3 summaries; keys {:?}",
                m.len(),
                m.keys().collect::<Vec<_>>()
            ),
            Err(e) => println!(
                "STEP 2b: adapter ERROR kind={} message={}",
                e.kind, e.message
            ),
        }
        let body = raw_shape(
            &s,
            &json!({"query": summaries_query(&[SAMPLE, SHOWCASE, INVENTED]), "variables": {}}),
            "raw summaries([sample, showcase, invented]) for the answer's shape",
        );
        describe_shape(&body);
    }

    let mut usable: Option<&str> = None;
    if want("3") {
        for id in [SAMPLE, SHOWCASE] {
            if geometry_step(mp.as_ref(), &s, id) == Some(true) {
                usable = Some(id);
                break;
            }
        }
    }
    if want("4") {
        let id = match usable {
            Some(id) => id,
            None if !want("3") => SAMPLE,
            None => {
                println!("STEP 4: not reached (no usable geometry in step 3)");
                println!("requests: {}", REQUESTS.load(Ordering::SeqCst));
                return;
            }
        };
        if !want("3") {
            // Fill the cache the layout reads from (counted; a step-4-only rerun).
            count(&format!(
                "get_floor_geometry({id}) to fill the adapter cache"
            ));
            let _ = mp.get_floor_geometry(id);
        }
        layout_step(mp.clone(), id).await;
    }
    println!("requests: {}", REQUESTS.load(Ordering::SeqCst));
}
