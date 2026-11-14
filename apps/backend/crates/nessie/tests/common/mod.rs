//! Shared test helpers: golden data, Nessie schema conformance, a scripted in-process HTTP
//! server, and a capture of every `log` record.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Map, Value};
use ss_core::Contract;
use ss_domain::contracts::CostScenario;
use ss_nessie::{ApprovalEvidence, PaymentIntent};
use uuid::Uuid;

pub fn repo_root() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.ancestors().nth(4).unwrap_or(&here).to_path_buf()
}

pub fn read_json(rel: &str) -> Value {
    let path = repo_root().join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

pub fn nessie_golden(name: &str) -> Value {
    read_json(&format!("apps/backend/tests-golden/nessie/{name}"))
}

/// Validates `instance` against a component schema of the published Nessie spec subset.
pub fn nessie_conforms(schema_name: &str, instance: &Value) -> Vec<String> {
    let spec = nessie_golden("schemas.json");
    let mut root = Map::new();
    root.insert(
        "$schema".into(),
        Value::String("https://json-schema.org/draft/2020-12/schema".into()),
    );
    root.insert(
        "$ref".into(),
        Value::String(format!("#/components/schemas/{schema_name}")),
    );
    root.insert("components".into(), spec["components"].clone());
    let schema = Value::Object(root);
    let validator = jsonschema::options()
        .build(&schema)
        .unwrap_or_else(|e| panic!("schema {schema_name}: {e}"));
    validator
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect()
}

pub fn assert_nessie_conforms(schema_name: &str, instance: &Value) {
    let errors = nessie_conforms(schema_name, instance);
    assert!(
        errors.is_empty(),
        "{schema_name}: {errors:#?}\n{instance:#}"
    );
}

/// Replaces the golden vectors' `<id:N>` and `<timestamp>` placeholders.
fn fill_placeholders(v: &mut Value) {
    match v {
        Value::String(s) => {
            if let Some(n) = s.strip_prefix("<id:").and_then(|r| r.strip_suffix('>')) {
                let n: u64 = n.parse().expect("placeholder id");
                *s = format!("00000000-0000-4000-8000-{n:012}");
            } else if s == "<timestamp>" {
                *s = "2026-11-14T00:00:00Z".into();
            }
        }
        Value::Array(a) => a.iter_mut().for_each(fill_placeholders),
        Value::Object(o) => o.values_mut().for_each(fill_placeholders),
        _ => {}
    }
}

/// The finance engine's golden `CostScenario` for a case, property and horizon
/// (`apps/backend/tests-golden/domain/part2_pipeline.json`, generated from the Python oracle on
/// the facial-studio fixture project).
pub fn golden_scenario(case: &str, address: &str, horizon: &str) -> CostScenario {
    let d = read_json("apps/backend/tests-golden/domain/part2_pipeline.json");
    let c = d["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["name"] == case)
        .unwrap_or_else(|| panic!("no case {case}"));
    let mut s = c["output"]["properties"][address]["scenarios"][horizon].clone();
    assert!(!s.is_null(), "no scenario {case}/{address}/{horizon}");
    fill_placeholders(&mut s);
    CostScenario::from_json_value(s).expect("golden scenario validates")
}

pub const HOLLYWOOD: &str = "6922 W HOLLYWOOD BLVD";
pub const ALHAMBRA: &str = "5380 E ALHAMBRA AVE";

pub fn t(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).single().expect("time")
}

/// The approval the server would hand over for `intent`, APPROVED and unexpired.
pub fn approved(intent: &PaymentIntent) -> ApprovalEvidence {
    ApprovalEvidence {
        approval_id: Uuid::new_v4(),
        project_id: intent.project_id,
        action_type: intent.action.as_str().to_owned(),
        approval_status: "APPROVED".into(),
        action_payload: intent.approval_payload(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
    }
}

// ---- Log capture --------------------------------------------------------------------------------

struct Capture;

static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        let line = format!("{} {}: {}", record.level(), record.target(), record.args());
        LINES
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(line);
    }
    fn flush(&self) {}
}

static LOGGER: Capture = Capture;

/// Installs the capture (once per test binary) at trace level.
pub fn capture_logs() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Trace);
}

pub fn captured_logs() -> Vec<String> {
    LINES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

// ---- Scripted HTTP server -----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    /// Path without the query.
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("body {:?}: {e}", self.body))
    }
}

#[derive(Debug, Clone)]
pub enum Reply {
    /// Status, content type, body.
    Body(u16, &'static str, String),
    /// Accept, read the request, then wait this long without answering.
    Stall(Duration),
    /// A redirect to `location`.
    Redirect(u16, String),
}

pub fn json(status: u16, v: Value) -> Reply {
    Reply::Body(status, "application/json", v.to_string())
}

pub fn problem(status: u16, slug: &str, title: &str, detail: &str) -> Reply {
    Reply::Body(
        status,
        "application/problem+json",
        serde_json::json!({
            "type": format!("https://nessieisreal.com/problems/{slug}"),
            "title": title,
            "status": status,
            "detail": detail,
        })
        .to_string(),
    )
}

pub struct MockServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    /// Serves `replies` in order, one per connection, then stops accepting.
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recs = Arc::clone(&requests);
        thread::spawn(move || {
            for reply in replies {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                if let Some(r) = read_request(&stream) {
                    recs.lock().unwrap_or_else(|p| p.into_inner()).push(r);
                }
                answer(stream, reply);
            }
        });
        Self { url, requests }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

fn read_request(stream: &TcpStream) -> Option<Recorded> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (target, String::new()),
    };
    let mut headers = Vec::new();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_owned(), v.trim().to_owned());
            if k.eq_ignore_ascii_case("content-length") {
                len = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some(Recorded {
        method,
        path,
        query,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn answer(mut stream: TcpStream, reply: Reply) {
    let text = match reply {
        Reply::Body(status, ctype, body) => {
            if status == 204 {
                format!("HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            } else {
                format!(
                    "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    reason(status),
                    body.len()
                )
            }
        }
        Reply::Stall(d) => {
            thread::sleep(d);
            return;
        }
        Reply::Redirect(status, location) => format!(
            "HTTP/1.1 {status} {}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            reason(status)
        ),
    };
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}
