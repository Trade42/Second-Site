//! Live provider adapters against an in-process mock HTTP server (no real provider is called).
//!
//! `tests-golden/server/provider_requests.json` was recorded by running Python's own live
//! adapters (`gen_providers.py`, httpx `MockTransport`): for each scenario, the scripted
//! responses, every request Python sent (method, path, decoded query pairs in order, the auth
//! headers, the JSON body) and the outcome. Here the same responses are served over a loopback
//! socket and the Rust adapter must send the same requests and reach the same outcome.
//!
//! Compared: request count, method, path, query pairs (decoded, in order), the `Authorization`,
//! `X-API-KEY` and `X-App-Token` headers (present or absent alike), `Content-Type` and the body
//! of POSTs, and the result or the error's provider, kind, `retryable` and message. Not
//! compared: scheme and host (the mock listens on 127.0.0.1), other headers (`User-Agent`
//! differs), and the message of a body that is not JSON (Python prints the `JSONDecodeError`,
//! Rust serde's error; both are BAD_RESPONSE).

mod common;

use base64::Engine as _;
use serde_json::{json, Map, Value};
use ss_domain::integrations::base::{ProviderError, RetryPolicy};
use ss_domain::integrations::la_permits::{LaPermitProvider, RawPermit};
use ss_server::integrations::matterport::{suite_matches, MatterportProvider};
use ss_server::integrations::vexcel::{
    summarize_property, VexcelProvider, ORTHO_PIXELS, ORTHO_SIZE_M,
};
use ss_server::integrations::{LiveLaPermitProvider, LiveMatterportProvider, LiveVexcelProvider};

use common::mock_http::{Canned, MockServer};
use common::*;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn canned(r: &Value) -> Canned {
    let status = r["status"].as_u64().unwrap() as u16;
    let explicit = r
        .get("content_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(j) = r.get("json") {
        Canned {
            status,
            content_type: explicit.or_else(|| Some("application/json".into())),
            body: serde_json::to_vec(j).unwrap(),
        }
    } else if let Some(b) = r.get("body_b64") {
        Canned {
            status,
            content_type: explicit,
            body: B64.decode(b.as_str().unwrap()).unwrap(),
        }
    } else {
        Canned {
            status,
            content_type: explicit,
            body: Vec::new(),
        }
    }
}

fn policy(args: &Value) -> RetryPolicy {
    RetryPolicy {
        attempts: args["policy"]["attempts"].as_u64().unwrap() as u32,
        backoff_s: args["policy"]["backoff_s"].as_f64().unwrap(),
        timeout_s: 5.0,
    }
}

/// The path part of Python's base URL (`https://vx.test/v2` -> `/v2`).
fn base_path(base: &str) -> &str {
    let rest = base.split_once("://").map(|(_, r)| r).unwrap_or(base);
    rest.find('/').map(|i| &rest[i..]).unwrap_or("")
}

fn opt_str(v: &Value) -> Option<&str> {
    v.as_str()
}

fn render_permits(out: Vec<RawPermit>) -> Value {
    Value::Array(
        out.into_iter()
            .map(|p| {
                json!({
                    "dataset": p.dataset,
                    "data_origin": p.data_origin.as_str(),
                    "source_url": p.source_url,
                    "record": Value::Object(p.record),
                })
            })
            .collect(),
    )
}

fn error_json(e: &ProviderError) -> Value {
    json!({"provider": e.provider, "kind": e.kind.as_str(), "message": e.message, "retryable": e.retryable()})
}

/// Numbers compared as floats where Python keeps the provider's int (`areaFloor: 12`).
fn floats(v: &Value) -> Value {
    match v {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(a) => Value::Array(a.iter().map(floats).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), floats(v))).collect()),
        other => other.clone(),
    }
}

fn run(
    s: &Value,
) -> (
    Result<Value, ProviderError>,
    Vec<common::mock_http::Recorded>,
    String,
) {
    let responses: Vec<Canned> = s["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(canned)
        .collect();
    let server = MockServer::start(responses);
    let base = format!(
        "{}{}",
        server.base,
        base_path(s["base_url"].as_str().unwrap())
    );
    let args = &s["args"];
    let call = &s["call_args"];
    let method = s["method"].as_str().unwrap();
    let result: Result<Value, ProviderError> = match s["kind"].as_str().unwrap() {
        "socrata" => {
            let p = LiveLaPermitProvider::with_policy(
                &base,
                args["electrical"].as_str().unwrap(),
                args["mechanical"].as_str().unwrap(),
                opt_str(&args["app_token"]),
                policy(args),
            );
            match method {
                "search_by_address_prefix" => p.search_by_address_prefix(
                    call[0].as_str().unwrap(),
                    opt_str(&call[1]),
                    call[2].as_str().unwrap(),
                    opt_str(&call[3]),
                ),
                _ => p.get_permit(call[0].as_str().unwrap()),
            }
            .map(render_permits)
        }
        "matterport" => {
            let p = LiveMatterportProvider::with_policy(
                &base,
                opt_str(&args["id"]),
                opt_str(&args["secret"]),
                policy(args),
            );
            match method {
                "model_summaries" => {
                    let ids: Vec<String> = call[0]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_owned())
                        .collect();
                    p.model_summaries(&ids).map(|out| {
                        let mut m = Map::new();
                        for (k, v) in out {
                            m.insert(
                                k,
                                json!({"name": v.name, "area_sqm": v.area_sqm, "floor_count": v.floor_count}),
                            );
                        }
                        Value::Object(m)
                    })
                }
                _ => p
                    .get_floor_geometry(call[0].as_str().unwrap())
                    .map(|d| d.unwrap_or(Value::Null)),
            }
        }
        _ => {
            let p = LiveVexcelProvider::with_policy(&base, opt_str(&args["key"]), policy(args));
            let (lat, lon) = (call[0].as_f64().unwrap(), call[1].as_f64().unwrap());
            match method {
                "ortho_image" => p
                    .ortho_image(lat, lon, ORTHO_SIZE_M, ORTHO_PIXELS)
                    .map(|o| o.map_or(Value::Null, |b| json!(B64.encode(b)))),
                "property_elements" => p
                    .property_elements(lat, lon)
                    .map(|o| o.map_or(Value::Null, Value::Object)),
                _ => p
                    .capture_dates(lat, lon)
                    .map(|v| Value::Array(v.into_iter().map(Value::Object).collect())),
            }
        }
    };
    let requests = server.requests();
    (result, requests, server.base.clone())
}

#[test]
fn live_adapters_send_what_python_sends() {
    let golden = golden("provider_requests.json");
    let scenarios = golden.as_array().unwrap();
    assert_eq!(scenarios.len(), 22);
    let mut problems = Vec::new();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let (result, requests, rust_base) = run(s);
        let want = s["requests"].as_array().unwrap();
        if requests.len() != want.len() {
            problems.push(format!(
                "{name}: {} requests, Python sent {}",
                requests.len(),
                want.len()
            ));
            continue;
        }
        for (i, (got, w)) in requests.iter().zip(want).enumerate() {
            let q: Vec<Value> = got.query.iter().map(|(k, v)| json!([k, v])).collect();
            if got.method != w["method"]
                || got.path != w["path"].as_str().unwrap()
                || Value::Array(q.clone()) != w["query"]
            {
                problems.push(format!(
                    "{name}[{i}]: {} {} {:?} != python {} {} {}",
                    got.method, got.path, q, w["method"], w["path"], w["query"]
                ));
            }
            for h in ["authorization", "x-api-key", "x-app-token"] {
                let g = got.headers.get(h).map(String::as_str);
                let p = w["headers"].get(h).and_then(Value::as_str);
                if g != p {
                    problems.push(format!("{name}[{i}]: header {h} {g:?} != python {p:?}"));
                }
            }
            if let Some(body) = w.get("json") {
                let g: Value = serde_json::from_slice(&got.body).unwrap_or(Value::Null);
                if &g != body {
                    problems.push(format!("{name}[{i}]: body {g} != python {body}"));
                }
                let ct = got.headers.get("content-type").map(String::as_str);
                if ct != w["headers"]["content-type"].as_str() {
                    problems.push(format!(
                        "{name}[{i}]: content-type {ct:?} != python {}",
                        w["headers"]["content-type"]
                    ));
                }
            }
        }
        let py_base = s["base_url"].as_str().unwrap();
        let py_origin = &py_base[..py_base.len() - base_path(py_base).len()];
        match (&result, &s["result"]) {
            (Ok(v), w) if w.get("ok").is_some() => {
                let text = serde_json::to_string(&w["ok"])
                    .unwrap()
                    .replace(py_origin, &rust_base);
                let want: Value = serde_json::from_str(&text).unwrap();
                let (got, want) = if s["method"] == "model_summaries" {
                    (floats(v), floats(&want))
                } else {
                    (v.clone(), want)
                };
                if got != want {
                    problems.push(format!("{name}: result {v} != python {want}"));
                }
            }
            (Err(e), w) if w.get("error").is_some() => {
                let mut got = error_json(e);
                let mut want = w["error"].clone();
                if name == "socrata_body_not_json" {
                    got["message"] = Value::Null;
                    want["message"] = Value::Null;
                }
                if got != want {
                    problems.push(format!("{name}: error {got} != python {want}"));
                }
            }
            (r, w) => problems.push(format!("{name}: {r:?} != python {w}")),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn vexcel_answers_are_cached() {
    let jpeg = b"\xff\xd8 jpeg".to_vec();
    let server = MockServer::start(vec![Canned {
        status: 200,
        content_type: Some("image/jpeg".into()),
        body: jpeg.clone(),
    }]);
    let p = LiveVexcelProvider::with_policy(
        &format!("{}/v2", server.base),
        Some("vk"),
        RetryPolicy {
            attempts: 2,
            backoff_s: 0.0,
            timeout_s: 5.0,
        },
    );
    for _ in 0..3 {
        assert_eq!(
            p.ortho_image(34.05, -118.25, 80, 600).unwrap(),
            Some(jpeg.clone())
        );
    }
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn transport_failures_are_classified() {
    // Nothing listens on this port: UNAVAILABLE, retried, and no URL in the message.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let p = LiveVexcelProvider::with_policy(
        &format!("http://127.0.0.1:{port}/v2"),
        Some("vk-secret-value"),
        RetryPolicy {
            attempts: 2,
            backoff_s: 0.0,
            timeout_s: 2.0,
        },
    );
    let e = p.property_elements(34.05, -118.25).unwrap_err();
    assert_eq!(e.kind.as_str(), "UNAVAILABLE", "{e}");
    assert!(e.retryable());
    assert!(
        !e.to_string().contains("127.0.0.1") && !e.to_string().contains("vk-secret"),
        "{e}"
    );
    // A server that accepts but never answers: TIMEOUT.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let hold = std::thread::spawn(move || {
        let conns: Vec<_> = l.incoming().take(2).filter_map(Result::ok).collect();
        std::thread::sleep(std::time::Duration::from_secs(3));
        drop(conns);
    });
    let p = LiveMatterportProvider::with_policy(
        &format!("http://127.0.0.1:{port}/graph"),
        Some("tid"),
        Some("tsecret"),
        RetryPolicy {
            attempts: 2,
            backoff_s: 0.0,
            timeout_s: 0.5,
        },
    );
    let e = p.model_summaries(&["AAAA1111".to_owned()]).unwrap_err();
    assert_eq!(e.kind.as_str(), "TIMEOUT", "{e}");
    assert!(!e.to_string().contains("tsecret"));
    hold.join().unwrap();
}

/// `test_suite_matching`.
#[test]
fn suite_matching() {
    let m = |n: &str, s: &str| suite_matches(Some(n), Some(s));
    assert!(m("Suite 110B", "110 B"));
    assert!(m("Suite 787", "787") && m("STE 300 - Medical", "300"));
    assert!(m("Unit 12", "Unit 12") && m("#4", "4"));
    assert!(!m("2BR,3BA - 2,280 SF", "2"));
    assert!(!m("Suite 1100", "110") && !m("Lobby", "Retail"));
}

/// `test_vexcel_summary_and_client` (the summary half; the client half is in the vectors).
#[test]
fn vexcel_summary() {
    let props: Map<String, Value> = serde_json::from_value(json!({
        "structures": [{
            "primary-structure": true, "footprint.area": 812.4, "roof-characteristics.condition": 4.0,
            "roof-characteristics.material.class": "membrane", "roof-characteristics.material.score": 0.8,
            "roof-objects.ac-unit.count": 6, "roof-defects.structural-damage.detected": false,
            "roof-defects.discoloration.types.water-pooling.detected": true, "structure-distance": 0.0
        }],
        "metadata.ortho-date-estimate": "2025-12-01T10:00:00",
        "metadata.collection": "us-ca-losangeles-2025",
        "request-information.inside-parcel": true
    }))
    .unwrap();
    let s = summarize_property(&props);
    assert_eq!(s["footprint_area_sqm"], json!(812.4));
    assert_eq!(s["rooftop_ac_units"], json!(6));
    assert!(s["image_date"].as_str().unwrap().starts_with("2025-12"));
    assert_eq!(s.len(), 16);
    assert_eq!(s["solar"], Value::Null);
}

/// `floor_geometry_from_model` against Python's output on the same models
/// (`tests-golden/server/matterport_geometry.json`). Compared strictly, numbers with their type,
/// except `floor_boundary`, compared as a cyclic sequence (same vertices in the same clockwise
/// order; GEOS's starting vertex depends on its internal edge order).
#[test]
fn floor_geometry_matches_python() {
    use ss_server::integrations::matterport_geometry::floor_geometry_from_model;
    let cases = golden("matterport_geometry.json");
    let cases = cases.as_array().unwrap();
    assert_eq!(cases.len(), 9);
    let rotate_to_min = |b: &Value| -> Value {
        let mut pts: Vec<Value> = b.as_array().unwrap().clone();
        let key = |p: &Value| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap());
        let start = (0..pts.len())
            .min_by(|&i, &j| {
                let (a, b) = (key(&pts[i]), key(&pts[j]));
                a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1))
            })
            .unwrap();
        pts.rotate_left(start);
        Value::Array(pts)
    };
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let got = floor_geometry_from_model(c["model"].as_object().unwrap()).unwrap();
        let want = &c["result"];
        match got {
            None => assert!(
                want.is_null(),
                "{name}: Rust found no geometry, Python {want}"
            ),
            Some(mut g) => {
                assert!(
                    !want.is_null(),
                    "{name}: Python found no geometry, Rust {g}"
                );
                let mut w = want.clone();
                g["floor_boundary"] = rotate_to_min(&g["floor_boundary"]);
                w["floor_boundary"] = rotate_to_min(&w["floor_boundary"]);
                assert_eq!(g, w, "{name}");
            }
        }
    }
}

/// F5 of the part 2 review, kept as a documented deviation (`matterport_geometry` module docs):
/// on a rotated plan whose rooms are separate boxes sharing parts of walls, GEOS leaves the rooms
/// apart and Python returns the largest single room; Rust returns the whole suite. The plan and
/// Python's answer come from `gen_geometry_rotated.py`.
#[test]
fn rotated_multi_room_plan_keeps_the_whole_suite() {
    use ss_server::integrations::matterport_geometry::floor_geometry_from_model;
    let g = golden("matterport_geometry_rotated.json");
    let area = |ring: &Value| -> f64 {
        let pts: Vec<(f64, f64)> = ring
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect();
        let n = pts.len();
        (0..n)
            .map(|i| pts[i].0 * pts[(i + 1) % n].1 - pts[(i + 1) % n].0 * pts[i].1)
            .sum::<f64>()
            .abs()
            / 2.0
    };
    let rooms: Vec<f64> = g["room_areas_sqm"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    let suite: f64 = rooms.iter().sum();
    let largest = rooms.iter().copied().fold(0.0, f64::max);

    // What Python (GEOS) returns: one room, the largest.
    let py = &g["python_result"];
    assert_eq!(py["data_origin"], "LIVE");
    let py_area = area(&py["floor_boundary"]);
    assert!((py_area - largest).abs() < 0.01, "python {py_area}");
    assert_eq!(py["floor_boundary"].as_array().unwrap().len(), 4);
    // Shapely's own check: buffered by 1e-7 the rooms are one polygon of the suite's area.
    assert!((g["union_of_rooms_buffered_1e_7_sqm"].as_f64().unwrap() - suite).abs() < 1e-3);

    // What Rust returns: the whole suite's outline, every room inside it.
    let got = floor_geometry_from_model(g["model"].as_object().unwrap())
        .unwrap()
        .expect("Rust finds the suite");
    let rust_area = area(&got["floor_boundary"]);
    assert!(
        (rust_area - suite).abs() < 0.01,
        "rust {rust_area}, suite {suite}"
    );
    assert!(rust_area > py_area + 40.0);
    // The other fields follow Python's shape: same keys, same provenance labels.
    let keys = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    assert_eq!(keys(&got), keys(py));
    for k in ["geometry_source", "data_origin", "source_id", "scale_verified"] {
        assert_eq!(got[k], py[k], "{k}");
    }
    // The walls between the rooms are interior walls of the suite here.
    assert!(!got["fixed_walls"].as_array().unwrap().is_empty());
}
