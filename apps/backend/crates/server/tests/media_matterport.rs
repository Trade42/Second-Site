//! `GET /api/v1/media/matterport/{asset_id}/…` (a deliberate addition, not a port), on both
//! stores. The crate's `MockServer` stands in for both the GraphQL endpoint (`/graph`) and the
//! CDN (`/cdn/…`): the links the mock answers point back at it, with a fake signature `SIGNED-…`
//! that must never come back out of the service.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use ss_domain::contracts::InteriorAsset;
use ss_domain::integrations::base::RetryPolicy;
use ss_server::integrations::LiveMatterportProvider;
use ss_server::store::repo::PROPERTIES;
use tower::ServiceExt;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer, Recorded};
use common::*;

const MODEL: &str = "kvEQYM7yddK";
const FIXTURE_ASSET: &str = "FixtureScan01";
const MESH: &[u8] = b"v 1.0 2.0 3.0\nv 2.0 3.0 4.0\nf 1 2 1\n";
const IMAGE: &[u8] = b"\xff\xd8\xff\xe0model-still";
const PLAN_ALPHA: &[u8] = b"\x89PNG\r\n\x1a\nalpha-plan";
const PLAN_PLAIN: &[u8] = b"\x89PNG\r\n\x1a\nplain-plan";
const NOT_FOUND_MSG: &str = "Error fetching data (path: /model, code: not.found): Not Found";

fn photo_bytes(i: usize) -> Vec<u8> {
    format!("\u{ff}photo-{i}").into_bytes()
}

/// What the mock does; counters are read by the tests.
#[derive(Default)]
struct Script {
    base: Mutex<String>,
    /// 403 answers left for the mesh link (an expired signature).
    expire_mesh: AtomicUsize,
    /// GraphQL answers `not.found`.
    not_found: bool,
    /// GraphQL answers this HTTP status instead of data.
    graph_status: Option<u16>,
    /// Photo 1 is this many bytes.
    big_photo: Option<usize>,
}

fn ok(ct: &str, body: Vec<u8>) -> Canned {
    Canned {
        status: 200,
        content_type: Some(ct.into()),
        body,
    }
}

fn assets_answer(b: &str) -> Value {
    let photos: Vec<Value> = (1..=6)
        .map(|i| json!({"type": "photo2D", "status": "available", "resizeUrl": format!("{b}/cdn/p{i}.jpg?t=SIGNED-p{i}")}))
        .collect();
    json!({"data": {"model": {
        "id": MODEL,
        "name": "Suite 110B",
        "image": {"status": "available", "resizeUrl": format!("{b}/cdn/image.jpg?t=SIGNED-image")},
        "assets": {
            "meshes": [
                {"format": "obj", "resolution": "500k", "status": "available", "filename": "lod3_room0_chunk1.obj", "url": format!("{b}/cdn/chunk.obj?t=SIGNED-chunk")},
                {"format": "obj", "resolution": "50k", "status": "available", "filename": "out_res_50k.obj", "url": format!("{b}/cdn/mesh.obj?t=SIGNED-mesh")},
            ],
            "photos": photos,
            "floorplans": [
                {"format": "jpg", "flags": ["photogramy"], "width": 1845, "height": 1094, "resolution": 100.0, "origin": {"x": -15.1525, "y": -8.3295}, "status": "available", "url": format!("{b}/cdn/plan.jpg?t=SIGNED-pj")},
                {"format": "png", "flags": ["photogramy"], "width": 1700, "height": 950, "resolution": 100.0, "origin": {"x": -14.0, "y": -7.0}, "status": "available", "url": format!("{b}/cdn/plan-plain.png?t=SIGNED-pp")},
                {"format": "png", "flags": ["photogramy", "alpha"], "width": 1705, "height": 954, "resolution": 100.0, "origin": {"x": -14.4525, "y": -7.6295}, "status": "available", "url": format!("{b}/cdn/plan-alpha.png?t=SIGNED-pa&download=1&k=SIGNED-K")},
                {"format": "png", "flags": [], "width": 512, "height": 287, "resolution": 30.03, "origin": {"x": -14.0, "y": -7.0}, "status": "available", "url": format!("{b}/cdn/plan-30.png?t=SIGNED-p30")},
            ],
        },
    }}})
}

fn answer(s: &Script, r: &Recorded) -> Canned {
    let b = s.base.lock().unwrap().clone();
    if r.path == "/graph" {
        if let Some(code) = s.graph_status {
            return Canned {
                status: code,
                content_type: None,
                body: Vec::new(),
            };
        }
        let body = if s.not_found {
            json!({"errors": [{"message": NOT_FOUND_MSG, "extensions": {"code": "not.found"}}], "data": null})
        } else {
            assets_answer(&b)
        };
        return ok("application/json", serde_json::to_vec(&body).unwrap());
    }
    match r.path.as_str() {
        "/cdn/mesh.obj" => {
            let expired = {
                let left = s.expire_mesh.load(Ordering::SeqCst);
                left > 0
                    && s.expire_mesh
                        .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
            };
            if expired {
                return Canned {
                    status: 403,
                    content_type: None,
                    body: b"expired".to_vec(),
                };
            }
            ok("binary/octet-stream", MESH.to_vec())
        }
        "/cdn/image.jpg" => ok("image/jpeg", IMAGE.to_vec()),
        "/cdn/plan-alpha.png" => ok("image/png", PLAN_ALPHA.to_vec()),
        "/cdn/plan-plain.png" => ok("image/png", PLAN_PLAIN.to_vec()),
        "/cdn/p1.jpg" if s.big_photo.is_some() => {
            ok("image/jpeg", vec![b'x'; s.big_photo.unwrap()])
        }
        p if p.starts_with("/cdn/p") && p.ends_with(".jpg") => {
            let i: usize = p["/cdn/p".len()..p.len() - 4].parse().unwrap();
            ok("image/jpeg", photo_bytes(i))
        }
        _ => Canned {
            status: 404,
            content_type: None,
            body: Vec::new(),
        },
    }
}

fn mock(script: Script) -> (MockServer, Arc<Script>) {
    let s = Arc::new(script);
    let s2 = s.clone();
    let server = MockServer::start_with(move |r| answer(&s2, r));
    *s.base.lock().unwrap() = server.base.clone();
    (server, s)
}

fn quick() -> RetryPolicy {
    RetryPolicy {
        attempts: 1,
        backoff_s: 0.0,
        timeout_s: 5.0,
    }
}

struct Env {
    t: TestApp,
    dir: std::path::PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn asset(provider: &str, id: &str, origin: &str) -> InteriorAsset {
    serde_json::from_value(json!({
        "provider": provider, "asset_id": id, "asset_type": "matterport_model",
        "data_origin": origin, "has_measured_geometry": false,
    }))
    .unwrap()
}

/// A stored property with the Matterport asset `MODEL` and a fixture asset; then the live
/// adapter on `base` (or the fixture provider when `base` is `None`). `credentials` false gives
/// the live adapter without credentials.
async fn env(db: &TestDb, base: Option<&str>, credentials: bool) -> Env {
    let dir = std::env::temp_dir().join(format!("ss-mpmedia-{}", Uuid::new_v4().simple()));
    let dir_s = dir.to_string_lossy().into_owned();
    let t = TestApp::new(db, &[("OBJECT_STORAGE_LOCAL_DIR", dir_s.as_str())]);
    let (tok, _) = t.dev_token().await;
    let pid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    let id = Uuid::parse_str(props[0]["property_id"].as_str().unwrap()).unwrap();
    let mut tx = t.app.store.begin().await.unwrap();
    let mut prop = PROPERTIES.get(&mut *tx, id).await.unwrap().unwrap();
    prop.interior_assets = vec![
        asset("matterport", MODEL, "LIVE"),
        asset("fixture", FIXTURE_ASSET, "FIXTURE_SYNTHETIC"),
    ];
    PROPERTIES.upsert(&mut *tx, &prop).await.unwrap();
    tx.commit().await.unwrap();
    if let Some(base) = base {
        let mut integrations =
            ss_server::integrations::Integrations::from_settings(&t.app.settings);
        let (k, s) = if credentials {
            (Some("tid-secret"), Some("ts-secret"))
        } else {
            (None, None)
        };
        integrations.matterport = Arc::new(LiveMatterportProvider::with_policy(
            &format!("{base}/graph"),
            k,
            s,
            quick(),
        ));
        t.app.extensions.insert(Arc::new(integrations));
    }
    Env { t, dir }
}

struct Got {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Got {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn problem(&self, status: u16, code: &str) {
        assert_eq!(
            self.status.as_u16(),
            status,
            "{}",
            String::from_utf8_lossy(&self.body)
        );
        assert_eq!(self.json()["error_code"], code, "{:?}", self.json());
    }
}

async fn get(t: &TestApp, path: &str) -> Got {
    let resp = t
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/media/matterport/{path}"))
                .header("origin", "http://localhost:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Got {
        status,
        headers,
        body,
    }
}

/// Nothing the service answered carries the fake signature, a CDN path or the credentials.
fn assert_no_leak(g: &Got) {
    let text = String::from_utf8_lossy(&g.body);
    for bad in [
        "SIGNED",
        "/cdn/",
        "t=",
        "tid-secret",
        "ts-secret",
        "127.0.0.1",
    ] {
        assert!(!text.contains(bad), "{bad} in body {text}");
        for (k, v) in &g.headers {
            assert!(
                !v.to_str().unwrap_or("").contains(bad),
                "{bad} in header {k}"
            );
        }
    }
    assert!(g.headers.get("location").is_none());
}

fn count(server: &MockServer, path: &str) -> usize {
    server.requests().iter().filter(|r| r.path == path).count()
}

both_backends!(five_paths_then_the_cache, |db| {
    let (server, _s) = mock(Script::default());
    let e = env(db, Some(&server.base), true).await;

    let g = get(&e.t, &format!("{MODEL}/manifest.json")).await;
    assert_eq!(
        g.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&g.body)
    );
    assert_eq!(g.headers["content-type"], "application/json");
    assert_eq!(g.headers["cache-control"], "private, max-age=3600");
    assert_no_leak(&g);
    let mut m = g.json();
    assert!(m["retrieved_at"]
        .as_str()
        .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok()));
    m.as_object_mut().unwrap().remove("retrieved_at");
    assert_eq!(
        m,
        json!({
            "asset_id": MODEL, "provider": "matterport", "data_origin": "LIVE", "name": "Suite 110B",
            "mesh": {"available": true, "format": "obj", "resolution": "50k", "units": "m", "up_axis": "z"},
            "photos": {"count": 6},
            "image": {"available": true},
            "floorplan": {"available": true, "resolution_px_per_m": 100.0,
                          "origin": {"x": -14.4525, "y": -7.6295}, "width_px": 1705, "height_px": 954},
        })
    );

    let cases: Vec<(String, &str, Vec<u8>)> = vec![
        ("mesh.obj".into(), "model/obj", MESH.to_vec()),
        ("image.jpg".into(), "image/jpeg", IMAGE.to_vec()),
        ("floorplan.png".into(), "image/png", PLAN_ALPHA.to_vec()),
        ("photo/1.jpg".into(), "image/jpeg", photo_bytes(1)),
        ("photo/6.jpg".into(), "image/jpeg", photo_bytes(6)),
    ];
    for (p, ct, bytes) in &cases {
        let g = get(&e.t, &format!("{MODEL}/{p}")).await;
        assert_eq!(
            g.status,
            StatusCode::OK,
            "{p}: {}",
            String::from_utf8_lossy(&g.body)
        );
        assert_eq!(g.headers["content-type"], *ct, "{p}");
        assert_eq!(g.headers["cache-control"], "private, max-age=3600", "{p}");
        assert_eq!(&g.body, bytes, "{p}");
        // CORS as on every other GET route (the test settings allow http://localhost:3000).
        assert_eq!(
            g.headers["access-control-allow-origin"], "http://localhost:3000",
            "{p}"
        );
        assert_no_leak(&g);
    }
    // One assets query for all of it (the links are cached), one download per file, never the
    // 500k chunk or the other floor plans.
    let reqs = server.requests();
    assert_eq!(count(&server, "/graph"), 1);
    assert_eq!(reqs.len(), 1 + cases.len());
    for p in [
        "/cdn/chunk.obj",
        "/cdn/plan-plain.png",
        "/cdn/plan.jpg",
        "/cdn/plan-30.png",
    ] {
        assert_eq!(count(&server, p), 0, "{p}");
    }
    // Credentials on the GraphQL request, none on a download; the signed query was sent as is.
    for r in &reqs {
        if r.path == "/graph" {
            assert_eq!(r.method, "POST");
            assert!(r.headers["authorization"].starts_with("Basic "));
            let q: Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(q["variables"], json!({"id": MODEL}));
            assert!(q["query"]
                .as_str()
                .unwrap()
                .contains("resizeUrl(resolution: web)"));
        } else {
            assert_eq!(r.method, "GET");
            assert!(!r.headers.contains_key("authorization"), "{}", r.path);
            assert!(r
                .query
                .iter()
                .any(|(k, v)| k == "t" && v.starts_with("SIGNED")));
        }
    }
    let alpha = reqs
        .iter()
        .find(|r| r.path == "/cdn/plan-alpha.png")
        .unwrap();
    assert_eq!(
        alpha.query,
        vec![
            ("t".to_owned(), "SIGNED-pa".to_owned()),
            ("download".to_owned(), "1".to_owned()),
            ("k".to_owned(), "SIGNED-K".to_owned())
        ]
    );
    // Stored where the brief says.
    let stored = e.dir.join("matterport").join(MODEL);
    assert_eq!(std::fs::read(stored.join("mesh.obj")).unwrap(), MESH);

    // The second round makes no provider request at all.
    let before = server.requests().len();
    let g = get(&e.t, &format!("{MODEL}/manifest.json")).await;
    assert_eq!(g.status, StatusCode::OK);
    for (p, ct, bytes) in &cases {
        let g = get(&e.t, &format!("{MODEL}/{p}")).await;
        assert_eq!(g.status, StatusCode::OK, "{p}");
        assert_eq!(g.headers["content-type"], *ct);
        assert_eq!(&g.body, bytes);
    }
    assert_eq!(server.requests().len(), before);
    // Photo 7 of 6: the stored manifest answers without a request.
    get(&e.t, &format!("{MODEL}/photo/7.jpg"))
        .await
        .problem(404, "MEDIA_NOT_FOUND");
    assert_eq!(server.requests().len(), before);
});

both_backends!(an_expired_link_is_requeried_once, |db| {
    let script = Script::default();
    script.expire_mesh.store(1, Ordering::SeqCst);
    let (server, _s) = mock(script);
    let e = env(db, Some(&server.base), true).await;
    let g = get(&e.t, &format!("{MODEL}/mesh.obj")).await;
    assert_eq!(
        g.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&g.body)
    );
    assert_eq!(g.body, MESH);
    let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
    assert_eq!(
        paths,
        ["/graph", "/cdn/mesh.obj", "/graph", "/cdn/mesh.obj"]
    );

    // Expired twice in a row: one re-query, then the error (the CDN's 403 is AUTH).
    let script = Script::default();
    script.expire_mesh.store(2, Ordering::SeqCst);
    let (server, _s) = mock(script);
    let e = env(db, Some(&server.base), true).await;
    let g = get(&e.t, &format!("{MODEL}/mesh.obj")).await;
    g.problem(400, "PROVIDER_UNAVAILABLE");
    assert_eq!(g.json()["message"], "interior media unavailable (AUTH)");
    assert_eq!(g.json()["retryable"], false);
    assert_no_leak(&g);
    assert_eq!(server.requests().len(), 4);
    assert!(!e
        .dir
        .join("matterport")
        .join(MODEL)
        .join("mesh.obj")
        .exists());
});

both_backends!(an_unknown_model_is_geometry_unavailable, |db| {
    let (server, _s) = mock(Script {
        not_found: true,
        ..Script::default()
    });
    let e = env(db, Some(&server.base), true).await;
    for p in ["manifest.json", "mesh.obj", "photo/1.jpg"] {
        let g = get(&e.t, &format!("{MODEL}/{p}")).await;
        g.problem(404, "GEOMETRY_UNAVAILABLE");
        assert_no_leak(&g);
    }
    // The `not.found` answer is cached with the links: one query for the three.
    assert_eq!(server.requests().len(), 1);
});

both_backends!(provider_failures_are_provider_unavailable, |db| {
    for (code, kind, retryable) in [(503, "UNAVAILABLE", true), (401, "AUTH", false)] {
        let (server, _s) = mock(Script {
            graph_status: Some(code),
            ..Script::default()
        });
        let e = env(db, Some(&server.base), true).await;
        let g = get(&e.t, &format!("{MODEL}/manifest.json")).await;
        g.problem(400, "PROVIDER_UNAVAILABLE");
        assert_eq!(
            g.json()["message"],
            format!("interior media unavailable ({kind})")
        );
        assert_eq!(g.json()["retryable"], retryable, "{code}");
        assert_no_leak(&g);
        // Nothing stored: the next request asks again.
        assert!(!e
            .dir
            .join("matterport")
            .join(MODEL)
            .join("manifest.json")
            .exists());
    }
});

both_backends!(the_guard_refuses_without_a_provider_request, |db| {
    let (server, _s) = mock(Script::default());
    let e = env(db, Some(&server.base), true).await;
    for p in [
        // Syntactically a model id, but on no stored property.
        "AAAAbbbb1234/manifest.json".to_owned(),
        "AAAAbbbb1234/mesh.obj".to_owned(),
        // Stored, but the fixture provider's asset.
        format!("{FIXTURE_ASSET}/manifest.json"),
        format!("{FIXTURE_ASSET}/photo/1.jpg"),
        // Not a model id.
        "..%2F..%2Fetc/manifest.json".to_owned(),
        "%2E%2E/mesh.obj".to_owned(),
        "a%2Fb%2Fc%2Fd/mesh.obj".to_owned(),
        "abc/mesh.obj".to_owned(),
        // Photo numbers start at 1.
        format!("{MODEL}/photo/0.jpg"),
        format!("{MODEL}/photo/01x.jpg"),
        format!("{MODEL}/photo/-1.jpg"),
        format!("{MODEL}/photo/1.png"),
        format!("{MODEL}/photo/99999.jpg"),
        // Not a file of the contract (review addition).
        format!("{MODEL}/MESH.OBJ"),
        format!("{MODEL}/mesh.obj/"),
        format!("{MODEL}/photo/1.jpg/x"),
        format!("{MODEL}/textures.zip"),
        "..%2F..%2Fetc/textures.zip".to_owned(),
    ] {
        let g = get(&e.t, &p).await;
        g.problem(404, "MEDIA_NOT_FOUND");
        assert_no_leak(&g);
    }
    assert_eq!(server.requests().len(), 0);
    assert!(
        !e.dir.join("matterport").exists() || {
            // Only the model's own folder may exist, and here nothing was fetched at all.
            std::fs::read_dir(e.dir.join("matterport")).unwrap().count() == 0
        }
    );
});

both_backends!(fixture_mode_and_missing_credentials_make_no_request, |db| {
    let (server, _s) = mock(Script::default());
    // Fixture mode (the test settings' default).
    let e = env(db, None, true).await;
    for p in [
        "manifest.json",
        "mesh.obj",
        "photo/1.jpg",
        "image.jpg",
        "floorplan.png",
    ] {
        let g = get(&e.t, &format!("{MODEL}/{p}")).await;
        g.problem(404, "MEDIA_NOT_FOUND");
        assert!(
            g.json()["message"]
                .as_str()
                .unwrap()
                .starts_with("live Matterport is not switched on"),
            "{:?}",
            g.json()
        );
    }
    // Live mode without credentials.
    let e = env(db, Some(&server.base), false).await;
    let g = get(&e.t, &format!("{MODEL}/mesh.obj")).await;
    g.problem(404, "MEDIA_NOT_FOUND");
    assert!(g.json()["message"]
        .as_str()
        .unwrap()
        .starts_with("live Matterport is not switched on"));
    assert_eq!(server.requests().len(), 0);
});

both_backends!(an_oversize_file_is_refused, |db| {
    let limit = ss_server::http::routes::media_matterport::MAX_IMAGE_BYTES;
    let (server, _s) = mock(Script {
        big_photo: Some(limit + 1),
        ..Script::default()
    });
    let e = env(db, Some(&server.base), true).await;
    let g = get(&e.t, &format!("{MODEL}/photo/1.jpg")).await;
    g.problem(400, "PROVIDER_UNAVAILABLE");
    assert_eq!(
        g.json()["message"],
        "interior media unavailable (BAD_RESPONSE): the file is larger than the 8 MB limit"
    );
    assert_eq!(g.json()["retryable"], false);
    assert!(!e
        .dir
        .join("matterport")
        .join(MODEL)
        .join("photo-1.jpg")
        .exists());
    // Photo 2 is fine.
    let g = get(&e.t, &format!("{MODEL}/photo/2.jpg")).await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(g.body, photo_bytes(2));

    // Review addition: a body larger than the shared client's 64 MB buffer gets the same answer,
    // because the limit is applied while reading (the size never reaches the generic cap).
    let (server, _s) = mock(Script {
        big_photo: Some(64 * 1024 * 1024 + 1),
        ..Script::default()
    });
    let e = env(db, Some(&server.base), true).await;
    let g = get(&e.t, &format!("{MODEL}/photo/1.jpg")).await;
    assert_eq!(
        g.json()["message"],
        "interior media unavailable (BAD_RESPONSE): the file is larger than the 8 MB limit"
    );
});

both_backends!(two_first_requests_download_once, |db| {
    let (server, _s) = mock(Script::default());
    let e = env(db, Some(&server.base), true).await;
    let path = format!("{MODEL}/mesh.obj");
    let (a, b) = tokio::join!(get(&e.t, &path), get(&e.t, &path));
    for g in [a, b] {
        assert_eq!(g.status, StatusCode::OK);
        assert_eq!(g.body, MESH);
    }
    assert_eq!(count(&server, "/graph"), 1);
    assert_eq!(count(&server, "/cdn/mesh.obj"), 1);
    // No temporary file left behind.
    let names: Vec<String> = std::fs::read_dir(e.dir.join("matterport").join(MODEL))
        .unwrap()
        .map(|d| d.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["mesh.obj"]);
});
