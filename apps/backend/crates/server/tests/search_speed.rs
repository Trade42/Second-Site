//! A property search makes its provider requests concurrently, bounded, with the same result.
//!
//! Live adapters (Matterport, Vexcel, LA permits) talk to a loopback mock server that answers
//! every request after a fixed delay and counts the requests in flight. A 20-listing search at
//! the default bound must finish in well under the sequential time, never have more than the
//! bound in flight, and store the same properties, give the same task result and emit the same
//! events, in the same order, as the same search run with the bound set to 1. On both stores.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use ss_domain::contracts::{PropertySearchRequest, ProviderStatusEntry};
use ss_domain::integrations::base::{ProviderError, RetryPolicy};
use ss_domain::integrations::costar::{CoStarProvider, FixtureCoStarProvider, RawListing};
use ss_server::integrations::{
    Integrations, LiveLaPermitProvider, LiveMatterportProvider, LiveVexcelProvider,
};
use ss_server::workflows::property_analysis::{SearchConcurrency, PROVIDER_CONCURRENCY};

use common::*;

const DELAY: Duration = Duration::from_millis(100);
const LISTINGS: usize = 20;

/// A threaded loopback HTTP server: one thread per connection, each answer after [`DELAY`].
struct SlowServer {
    base: String,
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    max_in_flight: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
}

impl SlowServer {
    fn start() -> SlowServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        let (s, i, m, t) = (
            stop.clone(),
            in_flight.clone(),
            max_in_flight.clone(),
            total.clone(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (i, m, t) = (i.clone(), m.clone(), t.clone());
                std::thread::spawn(move || serve(stream, &i, &m, &t));
            }
        });
        SlowServer {
            base: format!("http://{addr}"),
            addr,
            stop,
            max_in_flight,
            total,
        }
    }

    fn reset(&self) {
        self.max_in_flight.store(0, Ordering::SeqCst);
        self.total.store(0, Ordering::SeqCst);
    }
}

impl Drop for SlowServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(mut stream: TcpStream, in_flight: &AtomicUsize, max: &AtomicUsize, total: &AtomicUsize) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let target = head.split(' ').nth(1).unwrap_or("").to_owned();
    let len: usize = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < len {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    max.fetch_max(now, Ordering::SeqCst);
    total.fetch_add(1, Ordering::SeqCst);
    std::thread::sleep(DELAY);
    let answer = answer(&target, &body);
    in_flight.fetch_sub(1, Ordering::SeqCst);
    let text = answer.to_string();
    let out = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.flush();
}

/// Per provider path: permits none, Vexcel no structure but an urban capture date, Matterport a
/// scan per model (even models name the listed suite, odd ones another space).
fn answer(target: &str, body: &[u8]) -> Value {
    let path = target.split('?').next().unwrap_or("");
    if path.contains("/resource/") {
        return json!([]);
    }
    if path.ends_with("/property/extract") {
        return json!({"type": "FeatureCollection", "features": []});
    }
    if path.ends_with("/ortho/collections") {
        return json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {"layer": "urban", "last-capture-date": "2025-06-01T00:00:00Z"}}
        ]});
    }
    // Matterport GraphQL: `m{j}: model(id: "ID") { ... }`.
    let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let query = req["query"].as_str().unwrap_or("");
    let re = regex::Regex::new(r#"m(\d+): model\(id: "([^"]+)"\)"#).unwrap();
    let mut data = serde_json::Map::new();
    for c in re.captures_iter(query) {
        let id = c[2].to_owned();
        let n: usize = id.trim_start_matches("MODEL").parse().unwrap_or(0);
        let name = if n.is_multiple_of(2) {
            "Suite 100"
        } else {
            "Lobby"
        };
        data.insert(
            format!("m{}", &c[1]),
            json!({"id": id, "name": name, "dimensions": {"areaFloor": 140.0 + n as f64}, "floors": [{"id": "f1"}]}),
        );
    }
    json!({"data": data})
}

/// [`LISTINGS`] distinct listings built from the first fixture listing, each with one Matterport
/// model, coordinates and suite 100.
struct ManyListings {
    base: RawListing,
}

impl CoStarProvider for ManyListings {
    fn mode(&self) -> &str {
        "fixture"
    }
    fn search(&self, request: &PropertySearchRequest) -> Result<Vec<RawListing>, ProviderError> {
        let n = LISTINGS.min(usize::try_from(request.limit).unwrap_or(0));
        Ok((0..n)
            .map(|i| {
                let mut r = self.base.clone();
                r.provider_listing_id = format!("speed-{i}");
                r.address.insert(
                    "street_address".into(),
                    json!(format!("{} E Alhambra Ave", 5000 + i)),
                );
                r.address.insert("unit_or_suite".into(), json!("100"));
                r.address
                    .insert("parcel_id".into(), json!(format!("52200310{i:02}")));
                r.address
                    .insert("latitude".into(), json!(34.07 + i as f64 * 0.001));
                r.address
                    .insert("longitude".into(), json!(-118.16 - i as f64 * 0.001));
                let mut asset = serde_json::Map::new();
                asset.insert("provider".into(), json!("matterport"));
                asset.insert("asset_id".into(), json!(format!("MODEL{i:04}")));
                asset.insert("asset_type".into(), json!("matterport_model"));
                r.interior_assets = vec![asset];
                r
            })
            .collect())
    }
    fn status(&self) -> ProviderStatusEntry {
        unreachable!("not used")
    }
}

fn install(t: &TestApp, server: &SlowServer) {
    let policy = RetryPolicy {
        attempts: 1,
        backoff_s: 0.0,
        timeout_s: 10.0,
    };
    let base = Integrations::from_settings(&t.app.settings);
    let fixture = FixtureCoStarProvider::from_path(
        t.app
            .settings
            .fixtures_dir
            .join("properties")
            .join("la_candidates.json"),
    );
    let first = fixture
        .search(&PropertySearchRequest {
            limit: 50,
            ..Default::default()
        })
        .unwrap()
        .remove(0);
    t.app.extensions.insert(Arc::new(Integrations {
        costar: Arc::new(ManyListings { base: first }),
        permits: Arc::new(LiveLaPermitProvider::with_policy(
            &server.base,
            "elec-test",
            "mech-test",
            None,
            policy,
        )),
        matterport: Arc::new(LiveMatterportProvider::with_policy(
            &format!("{}/graph", server.base),
            Some("test-id"),
            Some("test-secret"),
            policy,
        )),
        vexcel: Arc::new(LiveVexcelProvider::with_policy(
            &server.base,
            Some("test-key"),
            policy,
        )),
        ..base
    }));
}

/// What a run leaves behind, without the fresh ids and timestamps.
#[derive(Debug, PartialEq)]
struct Outcome {
    properties: Vec<Value>,
    task: Value,
    events: Vec<String>,
}

fn props_view(props: &Value) -> Vec<Value> {
    props
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let manifest: Vec<Value> = p["source_manifest"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| {
                    json!([
                        m["provider"],
                        m["status"],
                        m["record_count"],
                        m["note"],
                        m["dataset"]
                    ])
                })
                .collect();
            let interior: Vec<Value> = p["interior_assets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| {
                    json!([
                        a["asset_id"],
                        a["label"],
                        a["area_sqm"],
                        a["matches_listing_unit"],
                        a["has_measured_geometry"],
                        a["data_origin"]
                    ])
                })
                .collect();
            let exterior: Vec<Value> = p["exterior_assets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| json!([a["provider"], a["asset_id"], a["captured_at"]]))
                .collect();
            json!({
                "address": p["property_identity"]["normalized_address"],
                "manifest": manifest,
                "missing": p["missing_information"],
                "interior": interior,
                "exterior": exterior,
                "observations": p["building_observations"].as_array().unwrap().len(),
                "permits": p["permit_records"],
                "assessments": p["infrastructure_assessments"].as_array().unwrap().iter()
                    .map(|a| json!([a["requirement_id"], a["status"]])).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// One search at `bound`, with fresh adapters (the Vexcel adapter caches its answers).
async fn run_search(
    t: &TestApp,
    tok: &str,
    server: &SlowServer,
    bound: usize,
) -> (Outcome, Duration) {
    install(t, server);
    t.app.extensions.insert(Arc::new(SearchConcurrency(bound)));
    let pid = t.create_project(tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let started = Instant::now();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(tok),
            &json!({"city": "Los Angeles", "state": "CA", "limit": LISTINGS}),
        )
        .await;
    let took = started.elapsed();
    assert_eq!(r.status.as_u16(), 202, "{}", r.text);
    let task = &r.body["task"];
    assert_eq!(task["status"], "COMPLETED", "{task}");
    let out = &task["output_reference"];
    let ids = out["property_ids"].as_array().unwrap();
    assert_eq!(ids.len(), LISTINGS, "{task}");
    let mut task_view = out.as_object().unwrap().clone();
    task_view.insert("property_ids".into(), json!(ids.len()));
    task_view.remove("ranking_id");
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(tok))
        .await
        .body;
    let events = t
        .get(&format!("/api/v1/projects/{pid}/events"), Some(tok))
        .await
        .body["events"]
        .clone();
    let events = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap().to_owned())
        .collect();
    (
        Outcome {
            properties: props_view(&props),
            task: Value::Object(task_view),
            events,
        },
        took,
    )
}

both_backends!(concurrent_search_is_faster_and_identical, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let server = SlowServer::start();

    let (sequential, seq_time) = run_search(&t, &tok, &server, 1).await;
    let seq_requests = server.total.load(Ordering::SeqCst);
    assert_eq!(server.max_in_flight.load(Ordering::SeqCst), 1);
    server.reset();

    let (concurrent, con_time) = run_search(&t, &tok, &server, PROVIDER_CONCURRENCY).await;
    let requests = server.total.load(Ordering::SeqCst);
    let max = server.max_in_flight.load(Ordering::SeqCst);
    eprintln!(
        "search_speed: bound 1: {seq_requests} requests in {seq_time:?}; bound {PROVIDER_CONCURRENCY}: \
         {requests} requests in {con_time:?}, at most {max} in flight"
    );
    assert_eq!(requests, seq_requests, "same provider requests");
    assert!(max <= PROVIDER_CONCURRENCY, "{max} in flight");
    assert!(max > 1, "requests overlapped");
    let sequential_cost = DELAY * u32::try_from(requests).unwrap();
    assert!(
        con_time < sequential_cost / 3,
        "{con_time:?} is not well under {sequential_cost:?}"
    );
    assert_eq!(concurrent, sequential);
    // The search really used the live adapters' answers.
    let first = &concurrent.properties[0];
    assert!(
        first["manifest"].to_string().contains("matterport"),
        "{first}"
    );
    assert!(first["manifest"].to_string().contains("vexcel"), "{first}");
    assert!(
        first["manifest"].to_string().contains("la_open_data"),
        "{first}"
    );
});
