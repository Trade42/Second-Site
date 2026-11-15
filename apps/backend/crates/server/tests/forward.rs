//! Forwarding to the Python service (`services/forward.rs`, `http/routes/forward.rs`), against an
//! in-process mock upstream on a loopback port that records every request exactly as it
//! arrived on the wire (request line, every header in order with duplicates, raw body bytes).
//!
//! Covered: every forwarded path and method; raw bodies (bytes that are not UTF-8, JSON with
//! unusual spacing) arrive byte-identical; `authorization`, the `webhook-*` and `telnyx-*`
//! headers and repeated headers arrive unchanged; hop-by-hop headers and `host` do not; query
//! strings survive; upstream answers (204, 302, 401, 404, 422, 500, chunked, several
//! `set-cookie`s) come back with their bodies and without their hop-by-hop and CORS headers;
//! paths and methods next to the list are not forwarded; nothing under `/internal/` is ever
//! forwarded, whatever the path tricks (in-process and over a real TCP connection); unset URL,
//! refused connection, timeouts, an answer cut off or too large give honest 5xx answers; an
//! oversized body is refused; a native route wins over a forwarded one; `/ready` reports the
//! link honestly; nothing sensitive reaches the server's log output.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use ss_server::app::AppState;
use ss_server::http::routes::forward::{forwarded, CALL_FLOW_ROUTES_UNTIL_PART_4, PYTHON_ROUTES};
use ss_server::services::forward::{install, parse_target, PythonLink, Timeouts};
use uuid::Uuid;

use common::*;

// ---- the mock upstream ----------------------------------------------------------------------

/// One request as the upstream received it.
#[derive(Debug, Clone)]
struct Captured {
    method: String,
    /// The request target exactly as sent (path and query).
    target: String,
    /// Every header line in order: (lower-case name, raw value).
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

impl Captured {
    fn all(&self, name: &str) -> Vec<&[u8]> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
            .collect()
    }

    fn one(&self, name: &str) -> Option<String> {
        let all = self.all(name);
        assert!(all.len() <= 1, "{name} sent {} times", all.len());
        all.first().map(|v| String::from_utf8_lossy(v).into_owned())
    }
}

/// How the upstream answers one request.
#[derive(Debug, Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
    /// Send the body chunked (no content-length).
    chunked: bool,
    /// Declare this content length but send `body` and close (a cut-off answer).
    declared_length: Option<usize>,
}

impl Reply {
    fn new(status: u16, body: impl Into<Vec<u8>>) -> Reply {
        Reply {
            status,
            headers: vec![],
            body: body.into(),
            delay: Duration::ZERO,
            chunked: false,
            declared_length: None,
        }
    }

    fn json(status: u16, v: Value) -> Reply {
        Reply::new(status, serde_json::to_vec(&v).unwrap())
            .header("content-type", "application/json")
    }

    fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.into(), v.into()));
        self
    }

    fn delayed(mut self, d: Duration) -> Reply {
        self.delay = d;
        self
    }
}

type Script = dyn Fn(&Captured) -> Reply + Send + Sync;

struct Upstream {
    base: String,
    addr: std::net::SocketAddr,
    captured: Arc<Mutex<Vec<Captured>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

fn read_request(stream: &mut TcpStream) -> Option<Captured> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = &buf[..end - 4];
    let mut lines = head
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let request_line = String::from_utf8_lossy(lines.next()?).into_owned();
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = Vec::new();
    for l in lines {
        let colon = l.iter().position(|&b| b == b':')?;
        let name = String::from_utf8_lossy(&l[..colon]).to_ascii_lowercase();
        let mut value = &l[colon + 1..];
        while value.first() == Some(&b' ') {
            value = &value[1..];
        }
        headers.push((name, value.to_vec()));
    }
    let len: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| std::str::from_utf8(v).ok()?.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buf[end..].to_vec();
    while body.len() < len {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    Some(Captured {
        method,
        target,
        headers,
        body,
    })
}

fn write_reply(stream: &mut TcpStream, r: &Reply) {
    std::thread::sleep(r.delay);
    let mut out = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", r.status).into_bytes();
    for (k, v) in &r.headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if r.chunked {
        out.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
        for part in r.body.chunks(7) {
            out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
    } else {
        let declared = r.declared_length.unwrap_or(r.body.len());
        if r.status != 204 && r.status != 304 {
            out.extend_from_slice(format!("Content-Length: {declared}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&r.body);
    }
    let _ = stream.write_all(&out);
    let _ = stream.flush();
}

impl Upstream {
    fn start(script: impl Fn(&Captured) -> Reply + Send + Sync + 'static) -> Upstream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let script: Arc<Script> = Arc::new(script);
        let (c2, s2) = (captured.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if s2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let (c3, script) = (c2.clone(), script.clone());
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    if let Some(req) = read_request(&mut stream) {
                        let reply = script(&req);
                        c3.lock().unwrap().push(req);
                        write_reply(&mut stream, &reply);
                    }
                });
            }
        });
        Upstream {
            base: format!("http://{addr}"),
            addr,
            captured,
            stop,
            handle: Some(handle),
        }
    }

    /// An upstream that echoes `{"method", "target"}` as JSON with 200.
    fn echo() -> Upstream {
        Upstream::start(|r| Reply::json(200, json!({"method": r.method, "target": r.target})))
    }

    fn captured(&self) -> Vec<Captured> {
        self.captured.lock().unwrap().clone()
    }

    /// Requests other than the background health checks.
    fn forwarded(&self) -> Vec<Captured> {
        self.captured()
            .into_iter()
            .filter(|c| c.target != "/health")
            .collect()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A loopback port nothing listens on.
fn closed_port_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}")
}

fn fast() -> Timeouts {
    Timeouts {
        connect: Duration::from_millis(1000),
        chat: Duration::from_millis(3000),
        default: Duration::from_millis(1000),
        health: Duration::from_millis(1000),
        health_interval: Duration::from_millis(100),
    }
}

/// The app with `PYTHON_SERVICE_URL` set to `url` (with the default timeouts).
fn app_for(db: &TestDb, url: &str) -> TestApp {
    TestApp::new(db, &[("PYTHON_SERVICE_URL", url)])
}

/// The app with a link to `url` using short timeouts.
fn fast_app_for(db: &TestDb, url: &str) -> TestApp {
    let t = app_for(db, url);
    install(
        &t.app,
        Arc::new(PythonLink::new(parse_target(Some(url)), fast())),
    );
    t
}

fn pid() -> String {
    "0b5d3c4e-8f1a-4c2b-9d3e-1f2a3b4c5d6e".into()
}

/// A concrete path for a template.
fn concrete(template: &str) -> String {
    template
        .replace("{project_id}", &pid())
        .replace("{call_id}", "11111111-2222-4333-8444-555555555555")
        .replace("{handoff_id}", "66666666-7777-4888-9999-000000000000")
}

fn request(method: &str, uri: &str, headers: &[(&str, &[u8])], body: Vec<u8>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, HeaderValue::from_bytes(v).unwrap());
    }
    b.body(Body::from(body)).unwrap()
}

// ---- what is forwarded, and how --------------------------------------------------------------

/// Send `req` through the forwarded routes alone (no native routes), with the app's middleware.
async fn send_forward_only(t: &TestApp, req: Request<Body>) -> Resp {
    let router =
        ss_server::http::with_middleware(ss_server::http::routes::forward::router(), t.app.clone());
    let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Resp {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text,
    }
}

fn listed_request(f: &ss_server::http::routes::forward::Forwarded) -> (String, Request<Body>) {
    let path = concrete(f.path);
    let body = if f.method == "GET" {
        vec![]
    } else {
        br#"{"x": 1}"#.to_vec()
    };
    let req = request(
        f.method,
        &path,
        &[("content-type", b"application/json")],
        body,
    );
    (path, req)
}

both_backends!(every_listed_route_is_forwarded, |db| {
    let up = Upstream::echo();
    let t = app_for(db, &up.base);
    // The forwarding routes on their own: every entry of both lists is forwarded as sent.
    let mut expected = Vec::new();
    for f in forwarded() {
        let (path, req) = listed_request(f);
        let r = send_forward_only(&t, req).await;
        assert_eq!(r.status, StatusCode::OK, "{} {path}: {}", f.method, r.text);
        assert_eq!(r.body, json!({"method": f.method, "target": path}));
        expected.push((f.method.to_owned(), path));
    }
    let got: Vec<(String, String)> = up
        .forwarded()
        .into_iter()
        .map(|c| (c.method, c.target))
        .collect();
    assert_eq!(got, expected);
    assert_eq!(
        expected.len(),
        PYTHON_ROUTES.len() + CALL_FLOW_ROUTES_UNTIL_PART_4.len()
    );
    // The whole service: the Python routes are always forwarded; a temporary call-flow entry is
    // forwarded unless a native route of the same path and method answers it (part 4).
    let before = up.forwarded().len();
    for f in PYTHON_ROUTES {
        let (path, req) = listed_request(f);
        let r = t.send(req).await;
        assert_eq!(
            r.body,
            json!({"method": f.method, "target": path}),
            "{}",
            r.text
        );
    }
    assert_eq!(up.forwarded().len(), before + PYTHON_ROUTES.len());
    for f in CALL_FLOW_ROUTES_UNTIL_PART_4 {
        let n = up.forwarded().len();
        let (path, req) = listed_request(f);
        let r = t.send(req).await;
        let was_forwarded = up.forwarded().len() == n + 1;
        if was_forwarded {
            assert_eq!(r.body, json!({"method": f.method, "target": path}));
        }
        eprintln!(
            "call-flow entry {} {}: {}",
            f.method,
            f.path,
            if was_forwarded {
                "forwarded"
            } else {
                "answered natively"
            }
        );
    }
});

both_backends!(bodies_and_headers_pass_unchanged, |db| {
    let up = Upstream::start(|_| Reply::new(204, ""));
    let t = app_for(db, &up.base);
    // A webhook body that is not UTF-8, with the signature headers and a token.
    let raw = vec![0xff, 0xfe, 0x00, b'{', 0x80, b'"', 0xc3, 0x28, b'\n'];
    let r = t
        .send(request(
            "POST",
            "/webhooks/photon",
            &[
                ("content-type", b"application/octet-stream"),
                ("authorization", b"Bearer abc.def"),
                ("webhook-id", b"msg_2KWPBgLlAfxdpx2AI54pPJ85f4W"),
                ("webhook-timestamp", b"1731628800"),
                (
                    "webhook-signature",
                    b"v1,K5oZfzN95Z9UVu1EsfQmfVNQhnkZ2pj9o9NDN/H/pI4= v1,second",
                ),
                ("telnyx-signature-ed25519", b"c2ln"),
                ("telnyx-timestamp", b"1731628800"),
                ("x-custom", b"one"),
                ("x-custom", b"two"),
                ("cookie", b"a=1; b=2"),
                ("user-agent", b"Svix-Webhooks/1.24"),
                ("x-forwarded-for", b"203.0.113.7"),
                // Hop-by-hop, and what is not forwarded.
                ("connection", b"keep-alive, x-hop-by-name"),
                ("x-hop-by-name", b"1"),
                ("keep-alive", b"timeout=5"),
                ("te", b"trailers"),
                ("trailer", b"x-checksum"),
                ("upgrade", b"h2c"),
                ("proxy-authorization", b"Basic cHJveHk6cHc="),
                ("proxy-connection", b"keep-alive"),
                ("host", b"rust.example.com"),
                ("expect", b"100-continue"),
                ("content-length", b"9"),
            ],
            raw.clone(),
        ))
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    let rid = r.headers["x-request-id"].to_str().unwrap().to_owned();
    let c = up.forwarded().pop().unwrap();
    assert_eq!(c.method, "POST");
    assert_eq!(c.target, "/webhooks/photon");
    assert_eq!(c.body, raw, "the raw body changed");
    for (name, value) in [
        ("content-type", "application/octet-stream"),
        ("authorization", "Bearer abc.def"),
        ("webhook-id", "msg_2KWPBgLlAfxdpx2AI54pPJ85f4W"),
        ("webhook-timestamp", "1731628800"),
        (
            "webhook-signature",
            "v1,K5oZfzN95Z9UVu1EsfQmfVNQhnkZ2pj9o9NDN/H/pI4= v1,second",
        ),
        ("telnyx-signature-ed25519", "c2ln"),
        ("telnyx-timestamp", "1731628800"),
        ("cookie", "a=1; b=2"),
        ("user-agent", "Svix-Webhooks/1.24"),
        ("x-forwarded-for", "203.0.113.7"),
        ("content-length", "9"),
    ] {
        assert_eq!(c.one(name).as_deref(), Some(value), "{name}");
    }
    assert_eq!(c.all("x-custom"), [b"one".as_slice(), b"two".as_slice()]);
    // Generated here (the client sent none), the same in the answer.
    assert_eq!(c.one("x-request-id").as_deref(), Some(rid.as_str()));
    assert!(Uuid::parse_str(&rid).is_ok());
    // `host` is the upstream's, set by the client.
    assert_eq!(c.one("host").unwrap(), up.addr.to_string());
    for gone in [
        "connection",
        "x-hop-by-name",
        "keep-alive",
        "te",
        "trailer",
        "upgrade",
        "proxy-authorization",
        "proxy-connection",
        "expect",
        "transfer-encoding",
    ] {
        assert!(c.all(gone).is_empty(), "{gone} was forwarded");
    }
    // Nothing else was added.
    let mut names: Vec<&str> = c.headers.iter().map(|(n, _)| n.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(
        names,
        [
            "authorization",
            "content-length",
            "content-type",
            "cookie",
            "host",
            "telnyx-signature-ed25519",
            "telnyx-timestamp",
            "user-agent",
            "webhook-id",
            "webhook-signature",
            "webhook-timestamp",
            "x-custom",
            "x-forwarded-for",
            "x-request-id",
        ]
    );

    // JSON with unusual spacing, to the chat; a client request id is passed on.
    let spaced = b"{ \"message\" :\"hi\" ,\n\t\"x\":[ 1,2.50 ,1e3]  }  \r\n".to_vec();
    let path = format!("/api/v1/projects/{}/chat", pid());
    let r = t
        .send(request(
            "POST",
            &path,
            &[
                ("content-type", b"application/json"),
                ("x-request-id", b"client-rid-42"),
            ],
            spaced.clone(),
        ))
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(r.headers["x-request-id"], "client-rid-42");
    let c = up.forwarded().pop().unwrap();
    assert_eq!(c.body, spaced);
    assert_eq!(c.one("x-request-id").as_deref(), Some("client-rid-42"));
    assert_eq!(c.one("content-length").unwrap(), spaced.len().to_string());

    // Query strings arrive as sent.
    for q in [
        "?x=1&y=%20a+b&z",
        "?",
        "?a=%2F..%2Finternal&b=%E2%9C%93",
        "?=&&",
    ] {
        let r = t
            .send(request(
                "GET",
                &format!("/api/v1/owner/channel{q}"),
                &[],
                vec![],
            ))
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        let c = up.forwarded().pop().unwrap();
        assert_eq!(c.target, format!("/api/v1/owner/channel{q}"));
        // A GET without a body carries no content-length.
        assert!(c.all("content-length").is_empty());
        assert!(c.body.is_empty());
    }
    // A POST without a body says so.
    let r = t
        .send(request("POST", "/api/v1/owner/channel/poll", &[], vec![]))
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        up.forwarded()
            .pop()
            .unwrap()
            .one("content-length")
            .as_deref(),
        Some("0")
    );
});

both_backends!(answers_come_back_unchanged, |db| {
    let up = Upstream::start(|r| {
        let q = r.target.split_once('?').map(|(_, q)| q).unwrap_or("");
        match q {
            "204" => Reply::new(204, ""),
            "401" => Reply::json(401, json!({"error_code": "UNAUTHORIZED", "message": "missing bearer token", "details": {}, "request_id": "py", "retryable": false}))
                .header("www-authenticate", "Bearer"),
            "404" => Reply::json(404, json!({"error_code": "NOT_FOUND", "message": "project not found", "details": {}, "request_id": null, "retryable": false})),
            "422" => Reply::json(422, json!({"error_code": "VALIDATION_ERROR", "message": "request validation failed", "details": {"errors": [{"type": "missing", "loc": ["body", "message"], "msg": "Field required"}]}, "request_id": null, "retryable": false})),
            "500" => Reply::new(500, "Internal Server Error").header("content-type", "text/plain; charset=utf-8"),
            "302" => Reply::new(302, "").header("location", "http://python.internal:8001/elsewhere"),
            "chunked" => Reply {
                chunked: true,
                ..Reply::new(200, br#"{"reply": "a chunked answer from the supervisor"}"#.to_vec())
            }
            .header("content-type", "application/json"),
            "headers" => Reply::new(200, "ok")
                .header("set-cookie", "a=1; Path=/")
                .header("set-cookie", "b=2; Path=/; HttpOnly")
                .header("access-control-allow-origin", "*")
                .header("access-control-allow-credentials", "false")
                .header("access-control-expose-headers", "x-secret")
                .header("keep-alive", "timeout=5")
                .header("x-upstream-hop", "1")
                .header("connection", "x-upstream-hop")
                .header("x-request-id", "upstream-rid")
                .header("cache-control", "no-store")
                .header("vary", "Accept-Encoding"),
            _ => Reply::new(200, ""),
        }
    });
    let t = app_for(db, &up.base);
    let get = |q: &str| request("GET", &format!("/api/v1/owner/channel?{q}"), &[], vec![]);
    let r = t.send(get("204")).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(r.text.is_empty());
    for code in [401u16, 404, 422] {
        let r = t.send(get(&code.to_string())).await;
        assert_eq!(r.status.as_u16(), code);
        assert_eq!(r.headers["content-type"], "application/json");
        assert!(r.body["message"].is_string(), "{}", r.text);
    }
    let r = t.send(get("401")).await;
    assert_eq!(r.headers["www-authenticate"], "Bearer");
    assert_eq!(
        r.body["request_id"], "py",
        "the body is Python's, unchanged"
    );
    let r = t.send(get("500")).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.text, "Internal Server Error");
    assert_eq!(r.headers["content-type"], "text/plain; charset=utf-8");
    // A redirect is returned, not followed.
    let before = up.forwarded().len();
    let r = t.send(get("302")).await;
    assert_eq!(r.status, StatusCode::FOUND);
    assert_eq!(
        r.headers["location"],
        "http://python.internal:8001/elsewhere"
    );
    assert_eq!(up.forwarded().len(), before + 1);
    // A chunked answer arrives whole, without its transfer coding.
    let r = t.send(get("chunked")).await;
    assert_eq!(
        r.body,
        json!({"reply": "a chunked answer from the supervisor"})
    );
    assert!(r.headers.get("transfer-encoding").is_none());
    // Headers: end-to-end ones kept (several set-cookies stay several), hop-by-hop and CORS
    // ones dropped; this service's CORS and request id apply.
    let r = t
        .send(request(
            "GET",
            "/api/v1/owner/channel?headers",
            &[
                ("origin", b"http://localhost:3000"),
                ("x-request-id", b"rid-7"),
            ],
            vec![],
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text, "ok");
    let cookies: Vec<&str> = r
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(cookies, ["a=1; Path=/", "b=2; Path=/; HttpOnly"]);
    assert_eq!(r.headers["cache-control"], "no-store");
    assert_eq!(r.headers["x-request-id"], "rid-7");
    assert_eq!(r.headers.get_all("x-request-id").iter().count(), 1);
    let allow: Vec<_> = r
        .headers
        .get_all("access-control-allow-origin")
        .iter()
        .collect();
    assert_eq!(allow, [HeaderValue::from_static("http://localhost:3000")]);
    assert_eq!(r.headers["access-control-allow-credentials"], "true");
    assert!(r.headers.get("access-control-expose-headers").is_none());
    for gone in ["keep-alive", "x-upstream-hop", "connection"] {
        assert!(r.headers.get(gone).is_none(), "{gone} came back");
    }
    let vary: Vec<String> = r
        .headers
        .get_all("vary")
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect();
    assert!(vary.iter().any(|v| v == "Accept-Encoding"), "{vary:?}");
});

// ---- what is not forwarded -------------------------------------------------------------------

// Paths and methods next to the list, answered here; the upstream sees none of them.
both_backends!(paths_next_to_the_list_are_not_forwarded, |db| {
    let up = Upstream::echo();
    let t = app_for(db, &up.base);
    let (tok, _) = t.dev_token().await;
    let p = pid();
    for (method, path) in [
        ("GET", "/api/v1/owner/other".to_owned()),
        ("GET", "/api/v1/owner".to_owned()),
        ("GET", "/api/v1/owner/channel/".to_owned()),
        ("GET", "/api/v1/owner/channel/extra".to_owned()),
        ("POST", "/api/v1/owner/channel/poll/now".to_owned()),
        ("POST", "/api/v1/owner/channels".to_owned()),
        ("POST", format!("/api/v1/projects/{p}/chat/extra")),
        ("POST", format!("/api/v1/projects/{p}/chats")),
        ("POST", "/webhooks/photon2".to_owned()),
        ("POST", "/webhooks/photon/x".to_owned()),
        ("POST", "/webhooks".to_owned()),
        ("POST", "/webhooks/telnyx/x".to_owned()),
        ("POST", "/webhooks/other".to_owned()),
        ("POST", format!("/api/v1/projects/{p}/calls/x/y/simulate")),
    ] {
        let r = t
            .send(request(
                method,
                &path,
                &[("authorization", format!("Bearer {tok}").as_bytes())],
                vec![],
            ))
            .await;
        assert_eq!(
            r.status,
            StatusCode::NOT_FOUND,
            "{method} {path}: {}",
            r.text
        );
        assert_eq!(r.body, json!({"detail": "Not Found"}), "{method} {path}");
    }
    // Methods the list does not name, on forwarded paths: the ordinary 405.
    for (method, path) in [
        ("DELETE", "/api/v1/owner/channel".to_owned()),
        ("POST", "/api/v1/owner/channel".to_owned()),
        ("HEAD", "/api/v1/owner/channel".to_owned()),
        ("GET", "/webhooks/photon".to_owned()),
        ("PUT", "/webhooks/photon".to_owned()),
        ("GET", "/api/v1/owner/channel/poll".to_owned()),
        ("PATCH", format!("/api/v1/projects/{p}/chat")),
        ("GET", format!("/api/v1/projects/{p}/chat")),
        // `/api/v1/projects/{project_id}` itself (the native project route, GET only).
        ("POST", "/api/v1/projects/chat".to_owned()),
    ] {
        let r = t.send(request(method, &path, &[], vec![])).await;
        assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
        if method != "HEAD" {
            assert_eq!(r.body, json!({"detail": "Method Not Allowed"}));
        }
    }
    assert!(up.forwarded().is_empty(), "{:#?}", up.forwarded());
});

/// A path under `/internal` as this service routes it (raw, case-sensitive): it must get the
/// ordinary 404. The other tricks land on another route template.
fn strict(path: &str) -> bool {
    !path.starts_with("/api/") && !path.starts_with("/webhooks/")
        || path.contains("/owner/channel")
        || path.starts_with("/webhooks/photon/")
        || path.contains("/chat")
}

/// The paths that must never reach the upstream: the internal link, and tricks around it.
fn internal_tricks() -> Vec<String> {
    let p = pid();
    vec![
        "/internal/v1/owner/notify".into(),
        "/internal/v1/owner/notify/".into(),
        "/internal/v1/anything".into(),
        "/internal/".into(),
        "/internal".into(),
        "/INTERNAL/V1/OWNER/NOTIFY".into(),
        "/Internal/v1/owner/notify".into(),
        "//internal/v1/owner/notify".into(),
        "/internal//v1/owner/notify".into(),
        "/%69nternal/v1/owner/notify".into(),
        "/internal%2Fv1%2Fowner%2Fnotify".into(),
        "/api/v1/owner/channel/../../../internal/v1/owner/notify".into(),
        "/api/v1/owner/channel/./../../../internal/v1/owner/notify".into(),
        "/api/v1/owner/channel/%2e%2e/%2e%2e/%2e%2e/internal/v1/owner/notify".into(),
        "/api/v1/owner/channel/%2E%2E/%2E%2E/%2E%2E/internal/v1/owner/notify".into(),
        "/api/v1/owner/channel%2F..%2F..%2F..%2Finternal%2Fv1%2Fowner%2Fnotify".into(),
        "/api/v1/owner/channel/..%2f..%2f..%2finternal/v1/owner/notify".into(),
        "/api/v1//owner/channel/../../internal/v1/owner/notify".into(),
        "/webhooks/photon/../../internal/v1/owner/notify".into(),
        "/webhooks/photon/..%2F..%2Finternal%2Fv1%2Fowner%2Fnotify".into(),
        // These match a forwarded template; the segment is refused.
        "/api/v1/projects/..%2F..%2F..%2Finternal%2Fv1%2Fowner%2Fnotify/chat".into(),
        "/api/v1/projects/%2e%2e%2f%2e%2e%2f%2e%2e%2finternal%2fv1%2fowner%2fnotify/chat".into(),
        "/api/v1/projects/../chat".into(),
        "/api/v1/projects/%2e%2e/chat".into(),
        "/api/v1/projects/..%5C..%5Cinternal/chat".into(),
        format!("/api/v1/projects/{p}/chat/../../../../internal/v1/owner/notify"),
        format!("/api/v1/projects/{p}/chat%2F..%2F..%2F..%2F..%2Finternal%2Fv1%2Fowner%2Fnotify"),
        format!("/api/v1/projects/{p}/calls/..%2F..%2F..%2F..%2Finternal%2Fv1%2Fowner%2Fnotify/simulate"),
    ]
}

both_backends!(the_internal_link_is_never_forwarded, |db| {
    let up = Upstream::echo();
    let t = app_for(db, &up.base);
    let (tok, _) = t.dev_token().await;
    let service = ss_server::security::issue_token_with(SECRET, Uuid::new_v4(), 120, "service");
    let notify = br#"{"owner_id": "0b5d3c4e-8f1a-4c2b-9d3e-1f2a3b4c5d6e", "project_id": "0b5d3c4e-8f1a-4c2b-9d3e-1f2a3b4c5d6e", "text": "hi", "purpose": "x", "idempotency_key": "k"}"#;
    for path in internal_tricks() {
        for method in ["POST", "GET", "PUT"] {
            for auth in [None, Some(tok.as_str()), Some(service.as_str())] {
                let mut headers: Vec<(&str, Vec<u8>)> =
                    vec![("content-type", b"application/json".to_vec())];
                if let Some(a) = auth {
                    headers.push(("authorization", format!("Bearer {a}").into_bytes()));
                }
                let h: Vec<(&str, &[u8])> =
                    headers.iter().map(|(k, v)| (*k, v.as_slice())).collect();
                let r = t.send(request(method, &path, &h, notify.to_vec())).await;
                if strict(&path) {
                    assert_eq!(
                        r.status,
                        StatusCode::NOT_FOUND,
                        "{method} {path}: {}",
                        r.text
                    );
                    assert_eq!(r.body, json!({"detail": "Not Found"}), "{method} {path}");
                } else {
                    // A trick on another template: answered here (the ordinary 404 unless a
                    // native route of that template answers it), never forwarded.
                    assert!(
                        [401, 404, 405, 422].contains(&r.status.as_u16()),
                        "{method} {path}: {}",
                        r.text
                    );
                }
            }
        }
    }
    assert!(up
        .captured()
        .iter()
        .all(|c| !c.target.to_ascii_lowercase().contains("internal")));
    assert!(up.forwarded().is_empty(), "{:#?}", up.forwarded());
});

/// The same over a real TCP connection to this service, with request targets no HTTP client
/// library would normalize (dot segments, absolute form, doubled slashes).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_internal_link_is_never_forwarded_over_tcp() {
    let up = Upstream::echo();
    let db = TestDb::new(Kind::Memory).await.unwrap();
    let t = app_for(&db, &up.base);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = t.router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let token = t.token_for(Uuid::new_v4());
    let mut targets = internal_tricks();
    targets.extend([
        format!("http://{addr}/internal/v1/owner/notify"),
        format!("http://{}/internal/v1/owner/notify", up.addr),
        "/internal/v1/owner/notify?next=/api/v1/owner/channel".into(),
        "/internal/v1/owner/notify#/api/v1/owner/channel".into(),
    ]);
    for target in &targets {
        let raw = format!(
            "POST {target} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let answer = tokio::task::spawn_blocking(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            s.write_all(raw.as_bytes()).unwrap();
            let mut out = Vec::new();
            let _ = s.read_to_end(&mut out);
            String::from_utf8_lossy(&out).into_owned()
        })
        .await
        .unwrap();
        let status_line = answer.lines().next().unwrap_or("");
        assert!(
            status_line.starts_with("HTTP/1.1 404") || status_line.starts_with("HTTP/1.1 400"),
            "{target}: {status_line}"
        );
    }
    // A control: a forwarded path with the same query reaches the upstream exactly as sent.
    let raw = format!(
        "POST /api/v1/owner/channel/poll?next=/internal/v1/owner/notify HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let answer = tokio::task::spawn_blocking(move || {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    })
    .await
    .unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let got: Vec<String> = up.forwarded().into_iter().map(|c| c.target).collect();
    assert_eq!(
        got,
        ["/api/v1/owner/channel/poll?next=/internal/v1/owner/notify"]
    );
    server.abort();
}

// ---- a native route wins ---------------------------------------------------------------------

async fn native() -> &'static str {
    "native"
}

#[tokio::test]
async fn a_native_route_wins_over_a_forwarded_one() {
    let up = Upstream::echo();
    let db = TestDb::new(Kind::Memory).await.unwrap();
    let t = app_for(&db, &up.base);
    let natives = || {
        Router::<AppState>::new()
            .route("/api/v1/projects/{project_id}/outreach", post(native))
            .route("/api/v1/projects/{project_id}/calls", post(native))
            .route("/webhooks/telnyx", post(native))
    };
    let fwd = ss_server::http::routes::forward::router;
    // In either merge order.
    for routes in [natives().merge(fwd()), fwd().merge(natives())] {
        let router = ss_server::http::with_middleware(routes, t.app.clone());
        let send = |m: &str, p: String| {
            let router = router.clone();
            let req = request(m, &p, &[], b"{}".to_vec());
            async move {
                let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
                let status = resp.status();
                let bytes = http_body_util::BodyExt::collect(resp.into_body())
                    .await
                    .unwrap()
                    .to_bytes();
                (status, String::from_utf8_lossy(&bytes).into_owned())
            }
        };
        let p = pid();
        assert_eq!(
            send("POST", format!("/api/v1/projects/{p}/outreach")).await,
            (StatusCode::OK, "native".into())
        );
        assert_eq!(
            send("POST", format!("/api/v1/projects/{p}/calls")).await,
            (StatusCode::OK, "native".into())
        );
        assert_eq!(
            send("POST", "/webhooks/telnyx".into()).await,
            (StatusCode::OK, "native".into())
        );
        // The forwarded method of a partly native path still goes to Python.
        let (s, body) = send("GET", format!("/api/v1/projects/{p}/calls")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["method"],
            "GET"
        );
        let (s, _) = send("DELETE", format!("/api/v1/projects/{p}/calls")).await;
        assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
    }
    let got: Vec<String> = up
        .forwarded()
        .into_iter()
        .map(|c| format!("{} {}", c.method, c.target))
        .collect();
    assert_eq!(
        got,
        [
            format!("GET /api/v1/projects/{}/calls", pid()),
            format!("GET /api/v1/projects/{}/calls", pid())
        ]
    );
}

// ---- failures --------------------------------------------------------------------------------

both_backends!(an_unset_url_is_an_honest_503, |db| {
    for url in [
        None,
        Some(""),
        Some("   "),
        Some("ftp://python:21"),
        Some("http://user:pw@python"),
    ] {
        let t = match url {
            None => TestApp::new(db, &[]),
            Some(u) => TestApp::new(db, &[("PYTHON_SERVICE_URL", u)]),
        };
        for (m, p) in [
            ("POST", "/webhooks/photon".to_owned()),
            ("GET", "/api/v1/owner/channel".to_owned()),
            ("POST", format!("/api/v1/projects/{}/chat", pid())),
        ] {
            let r = t
                .send(request(
                    m,
                    &p,
                    &[("content-type", b"application/json")],
                    b"{}".to_vec(),
                ))
                .await;
            assert_problem(&r, 503, "PYTHON_SERVICE_NOT_CONFIGURED");
            assert_eq!(r.body["retryable"], false);
            assert!(
                r.body["message"]
                    .as_str()
                    .unwrap()
                    .contains("runs in the Python service"),
                "{}",
                r.text
            );
            assert_eq!(
                r.body["request_id"],
                r.headers["x-request-id"].to_str().unwrap()
            );
            assert!(!r.text.contains("pw@"), "the URL leaked: {}", r.text);
        }
    }
});

both_backends!(an_unreachable_service_is_an_honest_502, |db| {
    let t = fast_app_for(db, &closed_port_url());
    for (m, p) in [
        ("POST", "/webhooks/photon".to_owned()),
        ("GET", "/api/v1/owner/channel".to_owned()),
        ("POST", format!("/api/v1/projects/{}/chat", pid())),
    ] {
        let r = t.send(request(m, &p, &[], b"{}".to_vec())).await;
        assert_problem(&r, 502, "PYTHON_SERVICE_UNAVAILABLE");
        assert_eq!(r.body["retryable"], true);
        assert_eq!(r.body["details"]["reason"], "connection_refused");
        assert!(
            r.body["message"]
                .as_str()
                .unwrap()
                .contains("Nothing was forwarded"),
            "{}",
            r.text
        );
    }
});

both_backends!(timeouts_are_honest_504s, |db| {
    let up = Upstream::start(|r| {
        let delay = if r.target.contains("slow") {
            4500
        } else {
            1800
        };
        Reply::json(200, json!({"ok": true})).delayed(Duration::from_millis(delay))
    });
    let t = fast_app_for(db, &up.base);
    // 1.8 s is over the 1 s default budget ...
    let r = t
        .send(request(
            "POST",
            "/api/v1/owner/channel/simulate-text?x",
            &[],
            b"{}".to_vec(),
        ))
        .await;
    assert_problem(&r, 504, "PYTHON_SERVICE_UNAVAILABLE");
    assert_eq!(r.body["retryable"], true);
    assert_eq!(r.body["details"]["reason"], "timeout");
    assert!(
        r.body["message"]
            .as_str()
            .unwrap()
            .contains("whether the request took effect there is unknown")
            || r.body["message"]
                .as_str()
                .unwrap()
                .contains("Whether the request took effect there is unknown"),
        "{}",
        r.text
    );
    // ... a webhook is never answered 2xx when it was not passed on ...
    let r = t
        .send(request("POST", "/webhooks/photon", &[], b"{}".to_vec()))
        .await;
    assert_eq!(r.status, StatusCode::GATEWAY_TIMEOUT);
    // ... and within the chat's 3 s budget (4.5 s is not).
    let chat = format!("/api/v1/projects/{}/chat", pid());
    let r = t.send(request("POST", &chat, &[], b"{}".to_vec())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = t
        .send(request(
            "POST",
            &format!("{chat}?slow"),
            &[],
            b"{}".to_vec(),
        ))
        .await;
    assert_problem(&r, 504, "PYTHON_SERVICE_UNAVAILABLE");
    // A GET that timed out says to try again.
    let r = t
        .send(request("GET", "/api/v1/owner/channel", &[], vec![]))
        .await;
    assert_problem(&r, 504, "PYTHON_SERVICE_UNAVAILABLE");
    assert!(
        r.body["message"].as_str().unwrap().contains("Try again"),
        "{}",
        r.text
    );
});

/// A listener whose accept queue is full: further connection attempts hang, so the connect
/// timeout (or, if the kernel completes the handshake anyway, the overall timeout) applies.
#[tokio::test]
async fn a_connection_that_never_opens_is_a_504() {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = sock.listen(1).unwrap();
    let addr = listener.local_addr().unwrap();
    let mut fill = Vec::new();
    for _ in 0..8 {
        if let Ok(Ok(s)) = tokio::time::timeout(
            Duration::from_millis(200),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        {
            fill.push(s);
        }
    }
    let db = TestDb::new(Kind::Memory).await.unwrap();
    let t = fast_app_for(&db, &format!("http://{addr}"));
    let started = std::time::Instant::now();
    let r = t
        .send(request("POST", "/webhooks/photon", &[], b"{}".to_vec()))
        .await;
    assert_problem(&r, 504, "PYTHON_SERVICE_UNAVAILABLE");
    let reason = r.body["details"]["reason"].as_str().unwrap();
    assert!(["connect_timeout", "timeout"].contains(&reason), "{reason}");
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(listener);
}

both_backends!(cut_off_and_oversized_answers_are_502s, |db| {
    let up = Upstream::start(|r| {
        if r.target.ends_with("cut") {
            Reply {
                declared_length: Some(100),
                ..Reply::new(200, b"0123456789".to_vec())
            }
        } else {
            Reply::new(200, vec![b'x'; 8 * 1024 * 1024 + 1])
        }
    });
    let t = fast_app_for(db, &up.base);
    let r = t
        .send(request("GET", "/api/v1/owner/channel?cut", &[], vec![]))
        .await;
    assert_problem(&r, 502, "PYTHON_SERVICE_UNAVAILABLE");
    assert_eq!(r.body["details"]["reason"], "transport_error");
    let r = t
        .send(request("GET", "/api/v1/owner/channel?big", &[], vec![]))
        .await;
    assert_problem(&r, 502, "PYTHON_SERVICE_UNAVAILABLE");
    assert_eq!(r.body["details"]["reason"], "response_too_large");
});

both_backends!(an_oversized_body_is_refused, |db| {
    let up = Upstream::start(|r| Reply::json(200, json!({"received": r.body.len()})));
    let t = app_for(db, &up.base);
    let limit = 2 * 1024 * 1024;
    let r = t
        .send(request("POST", "/webhooks/photon", &[], vec![b'a'; limit]))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["received"], limit);
    let r = t
        .send(request(
            "POST",
            "/webhooks/photon",
            &[],
            vec![b'a'; limit + 1],
        ))
        .await;
    assert_problem(&r, 413, "PAYLOAD_TOO_LARGE");
    // Refused from the declared length alone, before the body is read.
    let r = t
        .send(request(
            "POST",
            "/webhooks/photon",
            &[("content-length", b"3000000")],
            b"{}".to_vec(),
        ))
        .await;
    assert_problem(&r, 413, "PAYLOAD_TOO_LARGE");
    assert_eq!(up.forwarded().len(), 1);
});

// ---- /ready ----------------------------------------------------------------------------------

fn python_entry(ready: &Value) -> Value {
    ready["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "python_service")
        .cloned()
        .unwrap_or_else(|| panic!("no python_service entry: {ready}"))
}

async fn wait_for_status(t: &TestApp, status: &str) -> Value {
    for _ in 0..100 {
        let r = t.get("/ready", None).await;
        assert_conforms("ReadyResponse", &r.body);
        let e = python_entry(&r.body);
        if e["status"] == status {
            return e;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("python_service never reached {status}");
}

both_backends!(ready_reports_the_python_service_honestly, |db| {
    // Unset.
    let t = TestApp::new(db, &[]);
    let e = python_entry(&t.get("/ready", None).await.body);
    assert_eq!(e["mode"], "not_configured");
    assert_eq!(e["status"], "NOT_STARTED");
    assert!(
        e["note"].as_str().unwrap().starts_with("NOT_CONFIGURED"),
        "{e}"
    );
    // Not usable.
    let t = TestApp::new(db, &[("PYTHON_SERVICE_URL", "ftp://python")]);
    let e = python_entry(&t.get("/ready", None).await.body);
    assert_eq!(e["status"], "BLOCKED");
    // Configured but nothing listens: never LIVE on the strength of configuration alone.
    let t = fast_app_for(db, &closed_port_url());
    let e = wait_for_status(&t, "BLOCKED").await;
    assert!(
        e["note"].as_str().unwrap().contains("connection_refused"),
        "{e}"
    );
    // A service whose /health answers slowly: not checked yet, then LIVE ...
    let healthy = Arc::new(AtomicBool::new(true));
    let h2 = healthy.clone();
    let up = Upstream::start(move |r| {
        assert_eq!(r.target, "/health");
        if h2.load(Ordering::SeqCst) {
            Reply::json(200, json!({"status": "ok", "version": "0.1.0"}))
                .delayed(Duration::from_millis(400))
        } else {
            Reply::json(503, json!({"status": "down"}))
        }
    });
    let t = fast_app_for(db, &up.base);
    let e = python_entry(&t.get("/ready", None).await.body);
    assert_eq!(e["status"], "NOT_STARTED", "{e}");
    let e = wait_for_status(&t, "LIVE").await;
    assert_eq!(e["mode"], "forward");
    assert!(
        e["note"].as_str().unwrap().contains("answered HTTP 200"),
        "{e}"
    );
    // ... and BLOCKED within a few checks of it going down.
    healthy.store(false, Ordering::SeqCst);
    let e = wait_for_status(&t, "BLOCKED").await;
    assert!(e["note"].as_str().unwrap().contains("HTTP 503"), "{e}");
    // The overall status is still the database's.
    let r = t.get("/ready", None).await;
    assert_eq!(r.status, StatusCode::OK);
});

/// The registration `build_app` makes (default timeouts) checks the health right away.
#[tokio::test]
async fn the_default_link_checks_health_at_start() {
    let up = Upstream::start(|_| Reply::json(200, json!({"status": "ok"})));
    let db = TestDb::new(Kind::Memory).await.unwrap();
    let t = app_for(&db, &up.base);
    let e = wait_for_status(&t, "LIVE").await;
    assert!(e["note"].as_str().unwrap().contains("HTTP 200"));
    assert!(up
        .captured()
        .iter()
        .all(|c| c.method == "GET" && c.target == "/health"));
}

// ---- the log ---------------------------------------------------------------------------------

/// The binary's whole stdout and stderr while it forwards requests full of secrets, and fails
/// to: no secret appears; the failure line names the method, the path and the class only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_sensitive_is_logged() {
    let up = Upstream::start(|_| Reply::json(200, json!({"ok": true})));
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("forward-log-{}", Uuid::new_v4().simple()))
        .join("a")
        .join("b");
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_second-site-backend"))
        .current_dir(&dir)
        .env_clear()
        .env("STORE_BACKEND", "memory")
        .env("HOST", "127.0.0.1")
        .env("PORT", "0")
        .env("APP_ENV", "test")
        .env("AUTH_SECRET", "AUTH-SECRET-VALUE-1")
        .env("LOG_LEVEL", "DEBUG")
        .env("PYTHON_SERVICE_URL", &up.base)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let (l2, (tx, rx)) = (lines.clone(), std::sync::mpsc::channel::<String>());
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if line.contains("\"listening\"") {
                let _ = tx.send(line.clone());
            }
            l2.lock().unwrap().push(line);
        }
    });
    let listening = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the binary did not start");
    let v: Value = serde_json::from_str(&listening).unwrap();
    let addr: std::net::SocketAddr = v["addr"].as_str().unwrap().parse().unwrap();
    let secrets = [
        "TOKEN-SECRET-abc",
        "SIGNATURE-SECRET-def",
        "COOKIE-SECRET-ghi",
        "BODY-SECRET-jkl",
        "QUERY-SECRET-mno",
        "AUTH-SECRET-VALUE-1",
    ];
    let send = move |path: String| {
        let body = r#"{"text": "BODY-SECRET-jkl"}"#;
        let raw = format!(
            "POST {path}?token=QUERY-SECRET-mno HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer TOKEN-SECRET-abc\r\nWebhook-Signature: v1,SIGNATURE-SECRET-def\r\nWebhook-Id: msg_1\r\nWebhook-Timestamp: 1\r\nCookie: s=COOKIE-SECRET-ghi\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    };
    // `send` only captures the address, so it is `Copy`.
    let ok = tokio::task::spawn_blocking(move || send("/webhooks/photon".into()))
        .await
        .unwrap();
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    let base = up.base.clone();
    drop(up);
    // Now the upstream is gone: the failure path logs too.
    let failed =
        tokio::task::spawn_blocking(move || send(format!("/api/v1/projects/{}/chat", pid())))
            .await
            .unwrap();
    assert!(failed.starts_with("HTTP/1.1 502"), "{failed}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = child.kill();
    let mut stderr = String::new();
    let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
    let _ = child.wait();
    let out = lines.lock().unwrap().join("\n") + "\n" + &stderr;
    for s in secrets {
        assert!(!out.contains(s), "{s} appears in the log:\n{out}");
    }
    assert!(
        !out.contains(&base),
        "the upstream URL appears in the log:\n{out}"
    );
    let failure: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("python service unavailable"))
        .collect();
    assert_eq!(failure.len(), 1, "{out}");
    let f: Value = serde_json::from_str(failure[0]).unwrap();
    assert_eq!(f["level"], "WARNING");
    assert_eq!(f["method"], "POST");
    assert_eq!(f["path"], format!("/api/v1/projects/{}/chat", pid()));
    assert_eq!(f["reason"], "connection_refused");
    assert!(
        out.lines()
            .filter(|l| l.contains("\"app.request\""))
            .count()
            >= 2,
        "{out}"
    );
    let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
}
