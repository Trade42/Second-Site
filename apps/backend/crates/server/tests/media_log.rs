//! The request log never holds a signed media link, and stored bytes are served with `nosniff`.
//!
//! The log line is checked end to end: `request_log_line` re-runs this test binary with
//! `--nocapture` on `log_child` (which sends the requests) and reads the child's stdout, where the
//! service writes its log lines.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use ss_server::http::loggable_path;
use ss_server::security::sign_media_key;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer};
use common::*;

const CHILD_ENV: &str = "SS_MEDIA_LOG_CHILD";

fn objects_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir()
        .join(format!("ss-media-log-{}", Uuid::new_v4().simple()))
        .join("objects");
    std::fs::create_dir_all(d.join("p")).unwrap();
    d
}

fn cleanup(objects: &std::path::Path) {
    if let Some(parent) = objects.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn loggable_path_redacts_only_signed_media() {
    for (path, want) in [
        (
            "/api/v1/media/public/abc.def",
            "/api/v1/media/public/[redacted]",
        ),
        ("/api/v1/media/public/", "/api/v1/media/public/[redacted]"),
        (
            "/api/v1/media/public/a/b/c",
            "/api/v1/media/public/[redacted]",
        ),
    ] {
        assert_eq!(loggable_path(path), want, "{path}");
    }
    // Every other path, byte for byte.
    for path in [
        "/health",
        "/",
        "",
        "/api/v1/media/public",
        "/api/v1/media/publicx/abc",
        "/API/v1/media/public/abc",
        "/x/api/v1/media/public/abc",
        "/api/v1/media/vexcel/ortho/1b4e28ba-2fa1-11d2-883f-0016d3cca427",
        "/api/v1/media/matterport/a1/photo/p.jpg",
        "/api/v1/voice/stream/bridge-1",
        "/record",
        "/record/upload",
        "/api/v1/projects/%2Fapi%2Fv1/media",
        "/api/v1/projects/ünï",
    ] {
        assert_eq!(loggable_path(path), path);
    }
}

/// Sends a signed media request and a plain one; only does anything inside the child process.
#[tokio::test]
async fn log_child() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    let db = TestDb::new(Kind::Memory).await.unwrap();
    let dir = objects_dir();
    std::fs::write(dir.join("p/intro.wav"), b"RIFF").unwrap();
    let dir_s = dir.to_string_lossy().into_owned();
    let t = TestApp::new(&db, &[("OBJECT_STORAGE_LOCAL_DIR", dir_s.as_str())]);
    let token = sign_media_key(SECRET, "p/intro.wav", 900);
    // On stderr, so that stdout holds only what the service writes.
    eprintln!("CHILD_TOKEN={token}");
    let r = t.get(&format!("/api/v1/media/public/{token}"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = t.get("/api/v1/media/public/not-a-token", None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = t.get("/health", None).await;
    assert_eq!(r.status, StatusCode::OK);
    cleanup(&dir);
    db.finish().await;
}

#[test]
fn request_log_line() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "log_child", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "child failed:\n{stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let token = stderr
        .split("CHILD_TOKEN=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("child printed no token:\n{stdout}\n{stderr}"))
        .to_owned();
    let body = token.split('.').next().unwrap();
    assert!(body.len() > 8, "{token}");
    let logs: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("\"app.request\""))
        .collect();
    assert_eq!(logs.len(), 3, "{stdout}");
    for line in &logs {
        assert!(!line.contains(&token), "{line}");
        assert!(!line.contains(body), "{line}");
        assert!(!line.contains("not-a-token"), "{line}");
        for field in [
            "\"method\": \"GET\"",
            "\"status\": ",
            "\"request_id\": ",
            "\"ms\": ",
        ] {
            assert!(line.contains(field), "{field} missing: {line}");
        }
    }
    let redacted = "\"path\": \"/api/v1/media/public/[redacted]\"";
    assert!(logs[0].contains(redacted) && logs[0].contains("\"status\": 200"));
    assert!(logs[1].contains(redacted) && logs[1].contains("\"status\": 401"));
    assert!(logs[2].contains("\"path\": \"/health\""), "{}", logs[2]);
    // Nothing the child wrote to stdout holds the token.
    assert!(!stdout.contains(&token), "{stdout}");
}

async fn get_with_origin(t: &TestApp, path: &str) -> Resp {
    t.send(
        Request::builder()
            .uri(path)
            .header("origin", "http://localhost:5173")
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

fn header_names(r: &Resp) -> Vec<String> {
    let mut n: Vec<String> = r.headers.keys().map(|k| k.as_str().to_owned()).collect();
    n.sort();
    n
}

both_backends!(nosniff_on_stored_bytes, |db| {
    let dir = objects_dir();
    std::fs::write(dir.join("p/intro.wav"), b"RIFF").unwrap();
    let dir_s = dir.to_string_lossy().into_owned();
    let t = TestApp::new(db, &[("OBJECT_STORAGE_LOCAL_DIR", dir_s.as_str())]);

    // The signed route.
    let token = sign_media_key(SECRET, "p/intro.wav", 900);
    let r = get_with_origin(&t, &format!("/api/v1/media/public/{token}")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
    assert_eq!(r.headers["content-type"], "audio/wav");
    assert_eq!(r.headers["cache-control"], "no-store");
    assert_eq!(r.text, "RIFF");
    let cors = r.headers.get("access-control-allow-origin").cloned();
    // An error answer carries no stored bytes and is unchanged (no nosniff added there).
    let e = get_with_origin(&t, "/api/v1/media/public/junk").await;
    assert_problem(&e, 401, "UNAUTHORIZED");
    assert!(e.headers.get("x-content-type-options").is_none());
    assert_eq!(e.headers.get("access-control-allow-origin").cloned(), cors);
    let mut want = header_names(&e);
    want.retain(|n| n != "content-type" && n != "content-length");
    let mut got = header_names(&r);
    got.retain(|n| {
        ![
            "content-type",
            "content-length",
            "cache-control",
            "x-content-type-options",
        ]
        .contains(&n.as_str())
    });
    assert_eq!(
        got, want,
        "only content type, cache and nosniff differ from the error answer"
    );

    // The Vexcel ortho route, live against a local mock (no real provider).
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
    let prop_id = props[0]["property_id"].as_str().unwrap().to_owned();
    let jpeg = b"\xff\xd8\xff\xe0 jpeg".to_vec();
    let server = MockServer::start(vec![Canned {
        status: 200,
        content_type: Some("image/jpeg".into()),
        body: jpeg.clone(),
    }]);
    let base = format!("{}/v2", server.base);
    let live = TestApp::new(
        db,
        &[
            ("VEXCEL_PROVIDER_MODE", "live"),
            ("VEXCEL_BASE_URL", base.as_str()),
            ("VEXCEL_API_KEY", "vk-test"),
        ],
    );
    let r = get_with_origin(&live, &format!("/api/v1/media/vexcel/ortho/{prop_id}")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
    assert_eq!(r.headers["content-type"], "image/jpeg");
    assert_eq!(r.headers["cache-control"], "private, max-age=86400");
    assert_eq!(r.headers.get("access-control-allow-origin").cloned(), cors);
    cleanup(&dir);
});
