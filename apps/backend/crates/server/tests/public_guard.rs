//! `PUBLIC_PATH_GUARD`: off, no request is treated differently; on, a request that came through a
//! tunnel (a non-loopback `Host` or any forwarding header) reaches only the carrier's routes and
//! the owner's recording link, and everything else is the ordinary 404 before CORS, before
//! authentication and before any handler. A request from this machine is never touched.

mod common;

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use serde_json::Value;
use ss_server::http::{public_guard_check, PUBLIC_SHAPES};
use ss_server::services::forward::BACKEND_MARKER;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use common::*;

const ON: (&str, &str) = ("PUBLIC_PATH_GUARD", "true");
const TUNNEL_HOST: &str = "second-site-demo.trycloudflare.com";
const LOCAL_HOST: &str = "localhost:8010";
const NOT_FOUND: &str = r#"{"detail":"Not Found"}"#;
const RID: &str = "caller-chosen-request-id";
const FORWARDING: [(&str, &str); 9] = [
    ("forwarded", "for=203.0.113.9"),
    ("x-forwarded-for", "203.0.113.9"),
    ("x-forwarded-host", "localhost:8010"),
    ("x-forwarded-proto", "https"),
    ("x-real-ip", "203.0.113.9"),
    ("cf-connecting-ip", "203.0.113.9"),
    ("cf-ray", "8f1c2d3e4f5a6b7c-LAX"),
    ("cdn-loop", "cloudflare; loops=1"),
    ("via", "1.1 edge"),
];

fn build(method: &str, path: &str, headers: &[(&str, &str)], body: &'static [u8]) -> Request<Body> {
    let mut b = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path)
        .header("x-request-id", RID);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::from(body)).unwrap()
}

async fn send(t: &TestApp, method: &str, path: &str, headers: &[(&str, &str)]) -> Resp {
    t.send(build(method, path, headers, b"")).await
}

/// A bearer token minted the way the owner's page does it: from this machine.
async fn local_token(t: &TestApp) -> String {
    let r = send(t, "POST", "/api/v1/dev/token", &[("host", LOCAL_HOST)]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    r.body["access_token"].as_str().unwrap().to_owned()
}

/// The refused answer: the ordinary 404 body, our own request id, and no header that says why.
fn assert_refused(r: &Resp, what: &str) {
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{what}: {}", r.text);
    assert_eq!(r.text, NOT_FOUND, "{what}");
    let mut names: Vec<&str> = r.headers.keys().map(|k| k.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["content-length", "content-type", "x-request-id"],
        "{what}"
    );
    assert_ne!(r.headers["x-request-id"], RID, "{what}");
}

/// Twenty requests whose answers do not depend on the clock (the request id is fixed).
type Headers<'a> = &'a [(&'a str, &'a str)];
type OwnedCase = (&'static str, String, Vec<(&'static str, String)>);

fn twenty(token: &str) -> Vec<OwnedCase> {
    let auth = || vec![("authorization", format!("Bearer {token}"))];
    let preflight = vec![
        ("origin", "http://localhost:8090".to_owned()),
        ("access-control-request-method", "POST".to_owned()),
    ];
    vec![
        ("GET", "/health".into(), vec![]),
        ("GET", "/ready".into(), vec![]),
        ("GET", "/nope".into(), vec![]),
        ("GET", "/api/v1/projects".into(), vec![]),
        ("GET", "/api/v1/projects".into(), auth()),
        ("GET", "/api/v1/dev/fixtures/project".into(), vec![]),
        ("OPTIONS", "/webhooks/telnyx".into(), preflight),
        ("POST", "/webhooks/telnyx".into(), vec![]),
        ("GET", "/webhooks/telnyx".into(), vec![]),
        ("GET", "/api/v1/voice/stream/abc".into(), vec![]),
        ("GET", "/api/v1/media/public/not-a-token".into(), vec![]),
        ("HEAD", "/api/v1/media/public/not-a-token".into(), vec![]),
        ("POST", "/record/check".into(), vec![]),
        ("POST", "/record/upload".into(), vec![]),
        ("PUT", "/record".into(), vec![]),
        ("GET", "//health".into(), vec![]),
        ("GET", "/health/".into(), vec![]),
        (
            "GET",
            "/api/v1/media/vexcel/ortho/1b4e28ba-2fa1-11d2-883f-0016d3cca427".into(),
            vec![],
        ),
        (
            "GET",
            "/api/v1/owner/intro-recording/history".into(),
            auth(),
        ),
        ("GET", "/docs".into(), vec![]),
    ]
}

async fn answers(t: &TestApp, token: &str, extra: &[(&str, &str)]) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    for (method, path, headers) in twenty(token) {
        let mut h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        h.extend_from_slice(extra);
        let r = send(t, method, &path, &h).await;
        out.push((r.status.as_u16(), r.text));
    }
    out
}

both_backends!(off_no_request_is_treated_differently, |db| {
    let t = TestApp::new(db, &[]);
    assert!(!t.app.settings.public_path_guard, "off by default");
    let (token, _) = t.dev_token().await;
    let plain = answers(&t, &token, &[]).await;
    assert_eq!(plain.len(), 20);
    let foreign = answers(
        &t,
        &token,
        &[("host", TUNNEL_HOST), ("x-forwarded-for", "203.0.113.9")],
    )
    .await;
    assert_eq!(plain, foreign);
    let local = answers(&t, &token, &[("host", LOCAL_HOST)]).await;
    assert_eq!(plain, local);
    // The answers are today's: the routes answer, and /ready says nothing about the guard.
    assert_eq!(plain[0].0, 200);
    assert_eq!(plain[2], (404, NOT_FOUND.to_owned()));
    assert_eq!(plain[3].0, 401);
    assert_eq!(plain[4].0, 200);
    assert_eq!(plain[6].0, 200, "a preflight is answered by CORS");
    assert!(!plain[1].1.contains("public_path_guard"), "{}", plain[1].1);
    // The 404 of an unknown path still echoes the caller's request id and has CORS's vary.
    let r = send(&t, "GET", "/nope", &[("host", TUNNEL_HOST)]).await;
    assert_eq!(r.headers["x-request-id"], RID);
    let r = send(&t, "GET", "/health", &[("host", TUNNEL_HOST)]).await;
    assert!(r.headers.contains_key(BACKEND_MARKER));

    // On, a request from this machine gets the same twenty answers (the owner's page, the
    // Python service, the harness), except that /ready now has the guard's line.
    let on = TestApp::new(db, &[ON]);
    let mut local_on = answers(
        &on,
        &token,
        &[("host", LOCAL_HOST), ("origin", "http://localhost:8090")],
    )
    .await;
    let ready: Value = serde_json::from_str(&local_on[1].1).unwrap();
    let off_ready: Value = serde_json::from_str(&plain[1].1).unwrap();
    let line = ready["checks"]["public_path_guard"].as_str().unwrap();
    assert_eq!(line, public_guard_check());
    for shape in PUBLIC_SHAPES {
        assert!(line.contains(shape), "{shape} missing: {line}");
    }
    let mut without = ready.clone();
    without["checks"]
        .as_object_mut()
        .unwrap()
        .remove("public_path_guard");
    assert_eq!(without, off_ready);
    local_on[1] = plain[1].clone();
    assert_eq!(plain, local_on);
});

both_backends!(on_allowed_shapes_reach_their_handlers, |db| {
    let t = TestApp::new(db, &[ON]);
    let via_header: [(&str, &str); 2] = [("host", LOCAL_HOST), ("cf-ray", "8f1c2d3e4f5a6b7c-LAX")];
    for from in [&[("host", TUNNEL_HOST)][..], &via_header[..]] {
        // The webhook: the handler's own refusal of an unsigned event (not the guard's 404).
        let r = t.send(build("POST", "/webhooks/telnyx", from, b"{}")).await;
        assert_ne!(r.status, StatusCode::NOT_FOUND, "{}", r.text);
        assert!(r.body["error_code"].is_string(), "webhook: {}", r.text);
        // The audio stream: the route's own answer (in process there is no socket to upgrade;
        // `over_a_socket` shows the 401 of a missing token).
        let r = send(&t, "GET", "/api/v1/voice/stream/abc?token=x", from).await;
        assert_ne!(r.status, StatusCode::NOT_FOUND, "{}", r.text);
        assert_ne!(r.text, NOT_FOUND);
        // The signed link: an unknown token is the handler's 401.
        for method in ["GET", "HEAD"] {
            let r = send(&t, method, "/api/v1/media/public/not-a-token", from).await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{method}");
        }
        let r = send(&t, "GET", "/api/v1/media/public/not-a-token", from).await;
        assert_eq!(r.body["message"], "invalid media token", "{}", r.text);
        // The owner's recording link.
        let r = send(&t, "GET", "/record?x=1", from).await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.text.contains("<html"), "the page");
        let r = send(&t, "POST", "/record/check", from).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.body["valid"], false);
        let r = send(&t, "POST", "/record/upload", from).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert_eq!(r.body["error_code"], "LINK_INVALID");
    }
});

both_backends!(on_everything_else_is_404, |db| {
    let t = TestApp::new(db, &[ON]);
    let token = local_token(&t).await;
    let bearer = format!("Bearer {token}");
    let long = format!("/api/v1/media/public/{}", "a".repeat(513 - 21));
    assert_eq!(long.len(), 513);
    let longest_allowed = format!("/api/v1/media/public/{}", "a".repeat(512 - 21));
    let auth: &[(&str, &str)] = &[("authorization", bearer.as_str())];
    let preflight: &[(&str, &str)] = &[
        ("origin", "http://localhost:8090"),
        ("access-control-request-method", "POST"),
    ];
    let cases: Vec<(&str, &str, Headers)> = vec![
        ("POST", "/api/v1/dev/token", &[]),
        ("GET", "/api/v1/dev/fixtures/project", &[]),
        ("GET", "/api/v1/projects", auth),
        (
            "GET",
            "/api/v1/media/vexcel/ortho/1b4e28ba-2fa1-11d2-883f-0016d3cca427",
            auth,
        ),
        ("GET", "/api/v1/owner/intro-recording/history", auth),
        ("POST", "/api/v1/owner/intro-recording/link", auth),
        ("GET", "/health", &[]),
        ("GET", "/ready", &[]),
        ("GET", "/docs", &[]),
        ("GET", "/openapi.json", &[]),
        ("GET", "/", &[]),
        ("OPTIONS", "/webhooks/telnyx", preflight),
        ("OPTIONS", "/record/upload", preflight),
        // A method the shape does not have.
        ("GET", "/webhooks/telnyx", &[]),
        ("PUT", "/webhooks/telnyx", &[]),
        ("POST", "/record", &[]),
        ("HEAD", "/record", &[]),
        ("GET", "/record/check", &[]),
        ("POST", "/api/v1/voice/stream/abc", &[]),
        ("HEAD", "/api/v1/voice/stream/abc", &[]),
        ("POST", "/api/v1/media/public/abc", &[]),
        ("DELETE", "/api/v1/media/public/abc", &[]),
        // Path tricks: refused, not normalised.
        ("POST", "//webhooks/telnyx", &[]),
        ("POST", "/webhooks//telnyx", &[]),
        ("POST", "/webhooks/telnyx/", &[]),
        ("POST", "/webhooks/./telnyx", &[]),
        ("POST", "/webhooks/../webhooks/telnyx", &[]),
        ("POST", "/record/../api/v1/dev/token", &[]),
        ("POST", "/Webhooks/Telnyx", &[]),
        ("POST", "/webhooks/telnyx%2f", &[]),
        ("POST", "/webhooks%2Ftelnyx", &[]),
        ("POST", "/webhooks/telny%78", &[]),
        ("POST", "/webhooks/telnyx;x", &[]),
        ("GET", "/record/", &[]),
        ("GET", "//record", &[]),
        ("GET", "/RECORD", &[]),
        ("GET", "/%72ecord", &[]),
        ("GET", "/record%2f", &[]),
        ("GET", "/record/..", &[]),
        ("POST", "/record%2Fcheck", &[]),
        ("POST", "/record/check/", &[]),
        ("POST", "/record/UPLOAD", &[]),
        ("GET", "/api/v1/media/public", &[]),
        ("GET", "/api/v1/media/public/", &[]),
        ("GET", "/api/v1/media/public/..", &[]),
        ("GET", "/api/v1/media/public/.", &[]),
        ("GET", "/api/v1/media/public/%2e%2e", &[]),
        ("GET", "/api/v1/media/public/%2E%2E%2Fvexcel", &[]),
        ("GET", "/api/v1/media/public/a/b", &[]),
        ("GET", "/api/v1/media/public/a%2Fb", &[]),
        ("GET", "/api/v1/media/public/abc/", &[]),
        ("GET", "/api/v1/media/public//abc", &[]),
        ("GET", "/api/v1/media/public/a%20b", &[]),
        ("GET", "/api/v1/MEDIA/public/abc", &[]),
        ("GET", "/api/v1/media/public/../vexcel/ortho/x", &[]),
        ("GET", "/api/v1/voice/stream/", &[]),
        ("GET", "/api/v1/voice/stream/a/", &[]),
        ("GET", "/api/v1/voice/stream/a/b", &[]),
        ("GET", "/api/v1/voice/stream/%61", &[]),
        ("GET", "/api/v1/voice/Stream/abc", &[]),
        ("GET", long.as_str(), &[]),
    ];
    for (i, (method, path, headers)) in cases.iter().enumerate() {
        // Through a public host name.
        let mut h = headers.to_vec();
        h.push(("host", TUNNEL_HOST));
        let r = send(&t, method, path, &h).await;
        assert_refused(&r, &format!("public host: {method} {path}"));
        // From a loopback host name with one forwarding header (each header gets its turns).
        let fwd = FORWARDING[i % FORWARDING.len()];
        let mut h = headers.to_vec();
        h.push(("host", LOCAL_HOST));
        h.push(fwd);
        let r = send(&t, method, path, &h).await;
        assert_refused(&r, &format!("{}: {method} {path}", fwd.0));
        // From this machine the same request is not the guard's: it reaches the router (whose own
        // 404 or 405 for a path that does not exist echoes the caller's request id).
        let mut h = headers.to_vec();
        h.push(("host", LOCAL_HOST));
        let r = send(&t, method, path, &h).await;
        assert_eq!(r.headers["x-request-id"], RID, "local: {method} {path}");
    }
    // The named ones do answer locally.
    for (method, path, headers, want) in [
        ("POST", "/api/v1/dev/token", &[][..], 200),
        ("GET", "/api/v1/projects", auth, 200),
        ("GET", "/api/v1/owner/intro-recording/history", auth, 200),
        ("GET", "/health", &[][..], 200),
        ("GET", "/ready", &[][..], 200),
        ("OPTIONS", "/webhooks/telnyx", preflight, 200),
    ] {
        let mut h = headers.to_vec();
        h.push(("host", LOCAL_HOST));
        let r = send(&t, method, path, &h).await;
        assert_eq!(r.status.as_u16(), want, "local {method} {path}: {}", r.text);
    }
    // A path of exactly 512 bytes is still the handler's.
    assert_eq!(longest_allowed.len(), 512);
    let r = send(&t, "GET", &longest_allowed, &[("host", TUNNEL_HOST)]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // Every forwarding header alone makes a loopback request public.
    for fwd in FORWARDING {
        let r = send(&t, "GET", "/health", &[("host", LOCAL_HOST), fwd]).await;
        assert_refused(&r, fwd.0);
    }
});

both_backends!(on_which_host_is_local, |db| {
    let t = TestApp::new(db, &[ON]);
    for host in [
        "localhost",
        "localhost:8010",
        "LocalHost:8010",
        "LOCALHOST",
        "127.0.0.1",
        "127.0.0.1:8010",
        "[::1]",
        "[::1]:8010",
        "localhost:65535",
    ] {
        let r = send(&t, "GET", "/health", &[("host", host)]).await;
        assert_eq!(r.status, StatusCode::OK, "{host}");
    }
    for host in [
        "",
        " ",
        " localhost",
        "localhost ",
        "localhost.",
        "localhost:",
        "localhost:8010x",
        "localhost:123456",
        "localhost:-1",
        "localhost:8010:8010",
        "localhost.evil.example",
        "localhost@evil.example",
        "evil@localhost",
        "evil.example",
        "evil.example:8010",
        "127.0.0.1.evil.example",
        "127.0.0.2",
        "127.1",
        "0.0.0.0",
        "::1",
        "[::1",
        "[::ffff:127.0.0.1]",
        "localhost,localhost",
        "localhost, evil.example",
        "192.168.1.10:8010",
    ] {
        let r = send(&t, "GET", "/health", &[("host", host)]).await;
        assert_refused(&r, &format!("host {host:?}"));
    }
    // No Host at all, two Hosts (even two loopback ones), and a Host that is not text.
    let r = send(&t, "GET", "/health", &[]).await;
    assert_refused(&r, "no host");
    let r = send(
        &t,
        "GET",
        "/health",
        &[("host", "localhost"), ("host", "localhost")],
    )
    .await;
    assert_refused(&r, "two hosts");
    let mut req = build("GET", "/health", &[], b"");
    req.headers_mut()
        .insert("host", HeaderValue::from_bytes(b"localhost\xff").unwrap());
    assert_refused(&t.send(req).await, "host that is not text");
    // A request target that names another host, whatever Host says.
    let r = send(
        &t,
        "GET",
        "http://evil.example/health",
        &[("host", "localhost")],
    )
    .await;
    assert_refused(&r, "absolute target");
    let r = send(
        &t,
        "GET",
        "http://localhost:8010/health",
        &[("host", "localhost:8010")],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
});

/// One raw HTTP/1 exchange with a real listener; returns the status line and the whole answer.
async fn raw(addr: std::net::SocketAddr, request: String) -> (String, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read until the head and a short body are in (an upgrade refusal or a 404 is small).
    loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), s.read(&mut chunk))
            .await
            .expect("answer within 5 s")
            .unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // Complete when the head and the body its content-length announces are in.
        let text = String::from_utf8_lossy(&buf);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())?
            });
            if length.is_some_and(|n| body.len() >= n) {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.lines().next().unwrap_or_default().to_owned();
    (status, text)
}

both_backends!(over_a_socket, |db| {
    let t = TestApp::new(db, &[ON]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = t.router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let port = addr.port();
    let close = "Connection: close\r\n\r\n";

    let (status, _) = raw(
        addr,
        format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{close}"),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    // HTTP/1.0 without a Host, two Hosts, a target naming another host, a tunnel's host name.
    for request in [
        "GET /health HTTP/1.0\r\n\r\n".to_owned(),
        format!("GET /health HTTP/1.1\r\nHost: localhost\r\nHost: localhost\r\n{close}"),
        format!("GET http://evil.example/health HTTP/1.1\r\nHost: localhost\r\n{close}"),
        format!("POST /api/v1/dev/token HTTP/1.1\r\nHost: {TUNNEL_HOST}\r\nContent-Length: 0\r\n{close}"),
        format!("POST /api/v1/dev/token HTTP/1.1\r\nHost: localhost:{port}\r\nX-Forwarded-For: 203.0.113.9\r\nContent-Length: 0\r\n{close}"),
        format!("OPTIONS /webhooks/telnyx HTTP/1.1\r\nHost: {TUNNEL_HOST}\r\nOrigin: https://evil.example\r\nAccess-Control-Request-Method: POST\r\n{close}"),
    ] {
        let (status, text) = raw(addr, request.clone()).await;
        assert_eq!(status.get(9..), Some("404 Not Found"), "{request}\n{text}");
        assert!(text.ends_with(NOT_FOUND), "{request}\n{text}");
        assert!(!text.to_ascii_lowercase().contains("access-control"), "{text}");
        assert!(!text.to_ascii_lowercase().contains(BACKEND_MARKER), "{text}");
    }
    // The carrier's WebSocket upgrade through the tunnel reaches the handler: a missing token and
    // an unknown bridge are its 401, before any upgrade.
    let upgrade = "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
    for target in [
        "/api/v1/voice/stream/abc",
        "/api/v1/voice/stream/abc?token=x",
    ] {
        let (status, text) = raw(
            addr,
            format!("GET {target} HTTP/1.1\r\nHost: {TUNNEL_HOST}\r\nX-Forwarded-For: 203.0.113.9\r\n{upgrade}"),
        )
        .await;
        assert_eq!(status, "HTTP/1.1 401 Unauthorized", "{text}");
    }
    // The same upgrade to a path trick is the guard's 404.
    let (status, _) = raw(
        addr,
        format!("GET /api/v1/voice/stream/abc/ HTTP/1.1\r\nHost: {TUNNEL_HOST}\r\n{upgrade}"),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 404 Not Found");
    server.abort();
});

// ---- The log line (the child process pattern of tests/media_log.rs) ----

const CHILD_ENV: &str = "SS_PUBLIC_GUARD_LOG_CHILD";

#[tokio::test]
async fn log_child() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    let db = TestDb::new(Kind::Memory).await.unwrap();
    let t = TestApp::new(&db, &[ON]);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/dev/token?q=QUERYMARK")
        .header("host", "HOSTMARK.example")
        .header("x-request-id", "RIDMARK")
        .header("x-forwarded-for", "203.0.113.77")
        .header("user-agent", "UAMARK")
        .header("authorization", "Bearer AUTHMARK")
        .body(Body::from("BODYMARK"))
        .unwrap();
    assert_eq!(t.send(req).await.status, StatusCode::NOT_FOUND);
    let r = send(&t, "GET", "/nope", &[("host", LOCAL_HOST)]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    db.finish().await;
}

#[test]
fn refused_request_log_line() {
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
    let logs: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("\"app.request\""))
        .collect();
    assert_eq!(logs.len(), 2, "{stdout}");
    // (`--nocapture` puts the harness's "test log_child ... " before the first line.)
    let json = &logs[0][logs[0].find('{').unwrap()..];
    let refused: Value = serde_json::from_str(json).expect("a JSON log line");
    assert_eq!(refused.as_object().unwrap().len(), 10, "{json}");
    assert_eq!(refused["method"], "POST");
    assert_eq!(refused["path"], "/api/v1/dev/token");
    assert_eq!(refused["status"], 404);
    assert_eq!(refused["public_refused"], true);
    for mark in [
        "QUERYMARK",
        "HOSTMARK",
        "RIDMARK",
        "203.0.113.77",
        "UAMARK",
        "AUTHMARK",
        "BODYMARK",
    ] {
        assert!(!stdout.contains(mark), "{mark} in the log:\n{stdout}");
    }
    // The router's own 404 for a local request has no such field and keeps the caller's id.
    assert!(!logs[1].contains("public_refused"), "{}", logs[1]);
    assert!(logs[1].contains(RID), "{}", logs[1]);
}
