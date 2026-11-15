//! A property search makes its provider requests concurrently, bounded, with the same result.
//!
//! Live adapters (Matterport, Vexcel, LA permits) talk to a loopback mock server that answers
//! every request after a fixed delay and counts the requests in flight. A 20-listing search at
//! the default bound must finish in well under the sequential time, never have more than the
//! bound in flight, and store the same properties, give the same task result and emit the same
//! events, in the same order, as the same search run with the bound set to 1. On both stores.

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use ss_domain::contracts::{PropertySearchRequest, ProviderStatusEntry};
use ss_domain::integrations::base::{ProviderError, RetryPolicy};
use ss_domain::integrations::costar::{CoStarProvider, FixtureCoStarProvider, RawListing};
use ss_server::integrations::{
    Integrations, LiveLaPermitProvider, LiveMatterportProvider, LiveVexcelProvider,
};
use ss_server::workflows::property_analysis::SearchTimingSink;
use ss_server::workflows::property_analysis::{SearchConcurrency, PROVIDER_CONCURRENCY}; // SINK

use common::*;

const DELAY: Duration = Duration::from_millis(100);
const LISTINGS: usize = 20;
/// The listing pool a 20-result search asks for (`limit * 3`, at most 50): 50 Matterport ids,
/// three batched summary requests.
const POOL: usize = 50;

/// Answer delay per provider request.
#[derive(Debug, Clone, Copy)]
struct Delays {
    permits: Duration,
    vexcel: Duration,
    matterport: Duration,
}

impl Delays {
    const UNIFORM: Delays = Delays {
        permits: DELAY,
        vexcel: DELAY,
        matterport: DELAY,
    };
    /// Unequal latencies close to what the real providers can take (estimates, not measured):
    /// a Socrata `like` query on a large dataset 400 ms (each permit call is two of them, one
    /// after the other), a Vexcel extract 250 ms, a batched Matterport GraphQL query of 20 models
    /// 1.5 s.
    const REALISTIC: Delays = Delays {
        permits: Duration::from_millis(400),
        vexcel: Duration::from_millis(250),
        matterport: Duration::from_millis(1500),
    };
}

/// Per provider: first request received, last answer sent (relative to [`SlowServer::reset`]),
/// request count, most in flight.
#[derive(Debug, Default, Clone, Copy)]
struct Span {
    first_ms: Option<u128>,
    last_ms: u128,
    requests: usize,
    in_flight: usize,
    max_in_flight: usize,
}

type Spans = Arc<Mutex<(Instant, BTreeMap<&'static str, Span>)>>;

/// A threaded loopback HTTP server: one thread per connection, each answer after its provider's
/// delay.
struct SlowServer {
    base: String,
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    max_in_flight: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    spans: Spans,
}

impl SlowServer {
    fn start() -> SlowServer {
        SlowServer::with_delays(Delays::UNIFORM)
    }

    fn with_delays(delays: Delays) -> SlowServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        let spans: Spans = Arc::new(Mutex::new((Instant::now(), BTreeMap::new())));
        let (s, i, m, t, sp) = (
            stop.clone(),
            in_flight.clone(),
            max_in_flight.clone(),
            total.clone(),
            spans.clone(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (i, m, t, sp) = (i.clone(), m.clone(), t.clone(), sp.clone());
                std::thread::spawn(move || serve(stream, &i, &m, &t, &sp, delays));
            }
        });
        SlowServer {
            base: format!("http://{addr}"),
            addr,
            stop,
            max_in_flight,
            total,
            spans,
        }
    }

    fn reset(&self) {
        self.max_in_flight.store(0, Ordering::SeqCst);
        self.total.store(0, Ordering::SeqCst);
        *self.spans.lock().unwrap() = (Instant::now(), BTreeMap::new());
    }

    /// `provider: requests, first..last ms, max in flight` per provider since the last reset.
    /// The most requests one provider had in flight at once since the last reset.
    fn max_per_provider(&self) -> usize {
        let spans = self.spans.lock().unwrap();
        spans.1.values().map(|s| s.max_in_flight).max().unwrap_or(0)
    }

    fn spans(&self) -> String {
        let spans = self.spans.lock().unwrap();
        spans
            .1
            .iter()
            .map(|(k, s)| {
                format!(
                    "{k}: {} requests, {}..{} ms, at most {} in flight",
                    s.requests,
                    s.first_ms.unwrap_or(0),
                    s.last_ms,
                    s.max_in_flight
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl Drop for SlowServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

fn provider_of(path: &str) -> &'static str {
    if path.contains("/resource/") {
        "permits"
    } else if path.ends_with("/property/extract") || path.ends_with("/ortho/collections") {
        "vexcel"
    } else {
        "matterport"
    }
}

fn serve(
    mut stream: TcpStream,
    in_flight: &AtomicUsize,
    max: &AtomicUsize,
    total: &AtomicUsize,
    spans: &Mutex<(Instant, BTreeMap<&'static str, Span>)>,
    delays: Delays,
) {
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
    let provider = provider_of(target.split('?').next().unwrap_or(""));
    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    max.fetch_max(now, Ordering::SeqCst);
    total.fetch_add(1, Ordering::SeqCst);
    {
        let mut g = spans.lock().unwrap();
        let at = g.0.elapsed().as_millis();
        let s = g.1.entry(provider).or_default();
        s.first_ms.get_or_insert(at);
        s.requests += 1;
        s.in_flight += 1;
        s.max_in_flight = s.max_in_flight.max(s.in_flight);
    }
    std::thread::sleep(match provider {
        "permits" => delays.permits,
        "vexcel" => delays.vexcel,
        _ => delays.matterport,
    });
    let answer = answer(&target, &body);
    in_flight.fetch_sub(1, Ordering::SeqCst);
    {
        let mut g = spans.lock().unwrap();
        let at = g.0.elapsed().as_millis();
        let s = g.1.entry(provider).or_default();
        s.in_flight -= 1;
        s.last_ms = s.last_ms.max(at);
    }
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

/// Up to `pool` distinct listings built from the first fixture listing, each with one Matterport
/// model, coordinates and suite 100.
struct ManyListings {
    base: RawListing,
    pool: usize,
    /// The second listing repeats the first one's address.
    dup: bool,
    /// The first listing's model id is malformed (the Matterport adapter refuses it).
    bad_first: bool,
}

impl CoStarProvider for ManyListings {
    fn mode(&self) -> &str {
        "fixture"
    }
    fn search(&self, request: &PropertySearchRequest) -> Result<Vec<RawListing>, ProviderError> {
        let n = self.pool.min(usize::try_from(request.limit).unwrap_or(0));
        Ok((0..n)
            .map(|i| {
                let mut r = self.base.clone();
                r.provider_listing_id = format!("speed-{i}");
                let house = if self.dup && i == 1 { 5000 } else { 5000 + i };
                r.address.insert(
                    "street_address".into(),
                    json!(format!("{house} E Alhambra Ave")),
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
                let id = if self.bad_first && i == 0 {
                    "bad id!".to_owned()
                } else {
                    format!("MODEL{i:04}")
                };
                asset.insert("asset_id".into(), json!(id));
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
    install_listings(t, server, false, POOL, false);
}

fn install_listings(t: &TestApp, server: &SlowServer, bad_first: bool, pool: usize, dup: bool) {
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
        costar: Arc::new(ManyListings {
            base: first,
            pool,
            dup,
            bad_first,
        }),
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

/// The stored properties in the search's own order (`ids`, the task result's `property_ids`;
/// the list route's order is the store's).
fn props_view(props: &Value, ids: &[Value]) -> Vec<Value> {
    let by_id: std::collections::HashMap<&str, &Value> = props
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["property_id"].as_str().unwrap(), p))
        .collect();
    let unique: std::collections::HashSet<&str> = ids.iter().filter_map(Value::as_str).collect();
    assert_eq!(
        by_id.len(),
        unique.len(),
        "one stored property per result id"
    );
    ids.iter()
        .map(|id| by_id[id.as_str().unwrap()])
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
    let pid = t.create_project(tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    search_in(t, tok, bound, &pid, LISTINGS).await
}

/// One search at `bound` in the project `pid` with the installed adapters; `expect` results.
async fn search_in(
    t: &TestApp,
    tok: &str,
    bound: usize,
    pid: &str,
    expect: usize,
) -> (Outcome, Duration) {
    t.app.extensions.insert(Arc::new(SearchConcurrency(bound)));
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
    assert_eq!(ids.len(), expect, "{task}");
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
            properties: props_view(&props, ids),
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
    // The bound is per provider: at bound 1 each provider has one request in flight, but the
    // permit fetches overlap the Vexcel requests.
    assert_eq!(server.max_per_provider(), 1, "{}", server.spans());
    server.reset();

    let (concurrent, con_time) = run_search(&t, &tok, &server, PROVIDER_CONCURRENCY).await;
    let requests = server.total.load(Ordering::SeqCst);
    let max = server.max_per_provider();
    eprintln!(
        "search_speed: bound 1: {seq_requests} requests in {seq_time:?}; bound {PROVIDER_CONCURRENCY}: \
         {requests} requests in {con_time:?}, at most {max} in flight per provider ({})",
        server.spans()
    );
    assert_eq!(requests, seq_requests, "same provider requests");
    assert!(max <= PROVIDER_CONCURRENCY, "{max} in flight");
    assert!(max > 1, "requests overlapped");
    // Compared with the sequential run of the same test, not with an absolute time, so that a
    // loaded machine (which slows both runs) does not make it fail: measured about 6x, asserted 2x.
    assert!(
        con_time * 2 < seq_time,
        "{con_time:?} is not well under the sequential {seq_time:?}"
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

/// Results come back in input order even when later items finish first, at most `bound` run at
/// once, and `claim` sees the items in input order.
#[test]
fn bounded_map_keeps_input_order() {
    use ss_server::workflows::property_analysis::bounded_map;
    let running = AtomicUsize::new(0);
    let max = AtomicUsize::new(0);
    let claimed = std::sync::Mutex::new(Vec::new());
    let out = bounded_map(
        (0..24u64).collect(),
        PROVIDER_CONCURRENCY,
        |i| {
            claimed.lock().unwrap().push(i);
            i
        },
        |i| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            max.fetch_max(now, Ordering::SeqCst);
            // Later items answer sooner.
            std::thread::sleep(Duration::from_millis(5 * (24 - i)));
            running.fetch_sub(1, Ordering::SeqCst);
            i * 10
        },
    );
    assert_eq!(out, (0..24u64).map(|i| i * 10).collect::<Vec<_>>());
    assert_eq!(*claimed.lock().unwrap(), (0..24u64).collect::<Vec<_>>());
    assert!(max.load(Ordering::SeqCst) <= PROVIDER_CONCURRENCY);
}

both_backends!(one_bad_matterport_id_costs_only_its_listing, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let server = SlowServer::start();
    install_listings(&t, &server, true, LISTINGS, false);
    let pid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({"city": "Los Angeles", "state": "CA", "limit": LISTINGS}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 202, "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    let props = props.as_array().expect("properties").clone();
    assert_eq!(props.len(), LISTINGS);
    let lost: Vec<&str> = props
        .iter()
        .filter(|p| {
            p["missing_information"]
                .to_string()
                .contains("Matterport scans not retrieved")
        })
        .map(|p| {
            p["property_identity"]["normalized_address"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(lost[0].starts_with("5000 "), "{lost:?}");
});

// Not an assertion of speed: the search with unequal, realistic provider latencies
// (`Delays::REALISTIC`), the request spans the server saw per provider, and the search's own
// timing line. Run with `--nocapture` to read them.
both_backends!(realistic_latencies_timing_line, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let server = SlowServer::with_delays(Delays::REALISTIC);
    let sink = Arc::new(SearchTimingSink::default()); // SINK
    t.app.extensions.insert(sink.clone()); // SINK
    server.reset();
    let (outcome, took) = run_search(&t, &tok, &server, PROVIDER_CONCURRENCY).await;
    assert_eq!(outcome.properties.len(), LISTINGS);
    assert!(
        server.max_per_provider() <= PROVIDER_CONCURRENCY,
        "{}",
        server.spans()
    );
    eprintln!(
        "search_speed realistic: {} requests in {took:?}; {}",
        server.total.load(Ordering::SeqCst),
        server.spans()
    );
    let lines = sink.0.lock().unwrap().clone(); // SINK
    assert_eq!(lines.len(), 1, "{lines:?}"); // SINK
    let line = &lines[0]; // SINK
    eprintln!("search timing {line}"); // SINK
                                       // Numbers and phase names only. // SINK
    let text = line.to_string(); // SINK
    assert!(
        !text.contains("ALHAMBRA") && !text.contains("http"),
        "{text}"
    ); // SINK
    assert_eq!(line["calls"]["matterport"]["calls"], 3, "{text}"); // SINK
    assert_eq!(line["calls"]["permits"]["calls"], LISTINGS, "{text}"); // SINK
    assert_eq!(line["counts"]["pool"], POOL, "{text}"); // SINK
});

// A search whose addresses are already stored in the project (and one whose two first listings
// share an address): the permit fetches, started before the results are stored, use the stored
// identities, nothing is fetched twice, and the outcome is the sequential one.
both_backends!(repeated_search_uses_stored_identities, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let server = SlowServer::start();
    let sink = Arc::new(SearchTimingSink::default()); // SINK
    t.app.extensions.insert(sink.clone()); // SINK
    let mut outcomes = Vec::new();
    for bound in [1, PROVIDER_CONCURRENCY] {
        install_listings(&t, &server, false, LISTINGS, true);
        let pid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let (first, _) = search_in(&t, &tok, bound, &pid, LISTINGS).await;
        server.reset();
        let (again, _) = search_in(&t, &tok, bound, &pid, LISTINGS).await;
        // The second search asks the permit provider once per result, as before.
        assert_eq!(
            server.spans().matches("permits: 40 requests").count(),
            1,
            "{}",
            server.spans()
        );
        outcomes.push((first, again));
    }
    assert_eq!(outcomes[0], outcomes[1]);
    let lines = sink.0.lock().unwrap().clone(); // SINK
    assert_eq!(lines.len(), 4); // SINK
    for line in &lines {
        // SINK
        assert_eq!(line["counts"]["permit_refetches"], 0, "{line}"); // SINK
    } // SINK
    assert_eq!(lines[3]["counts"]["new"], 0, "{}", lines[3]); // SINK
});
