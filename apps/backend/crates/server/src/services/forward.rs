//! The link to the Python service, which keeps Photon (owner messaging) and the LangChain
//! supervisor (project chat). After the switch the browser and the providers talk to this
//! service only; it forwards the paths listed in [`crate::http::routes::forward`] to
//! `PYTHON_SERVICE_URL` and passes the answer back.
//!
//! What is passed through, unchanged:
//! - the method, the path and the raw query string;
//! - the body, as raw bytes (never parsed or re-encoded), so Photon's signature over the raw
//!   body still verifies in Python;
//! - every end-to-end header, including `authorization`, `content-type`, `webhook-id`,
//!   `webhook-timestamp`, `webhook-signature`, the `telnyx-*` signature headers, `cookie`,
//!   `origin`, `user-agent` and any `x-forwarded-*` the client sent. `x-request-id` is this
//!   request's id (the client's, or the one generated here when it sent none).
//!
//! Not passed: the hop-by-hop headers (`connection` and every header it names, `keep-alive`,
//! `transfer-encoding`, `te`, `trailer`, `upgrade`, `proxy-*`), `host` (set from
//! `PYTHON_SERVICE_URL`), `content-length` (set from the body) and `expect` (the body has
//! already been received here). Nothing is added: no `x-forwarded-for`, no `accept`.
//!
//! The answer comes back with its status, its body bytes and its end-to-end headers (several
//! `set-cookie`s stay several). Its hop-by-hop headers and `content-length` are dropped, and so
//! are its `access-control-*` headers: this service's CORS policy is the only one that applies,
//! exactly as for its own routes. Redirects are returned to the client, never followed.
//!
//! No authentication or project check is made here: the Python app makes its own with the same
//! tokens and the same `AUTH_SECRET`. Bodies and the values of `authorization`, `cookie` and the
//! signature headers are never logged; a failure is logged with the method, the path (without
//! the query) and the failure class only.
//!
//! Bounds: request bodies up to [`MAX_REQUEST_BYTES`] (larger is 413), answers up to
//! [`MAX_RESPONSE_BYTES`], a connect timeout of 3 s, and an overall timeout of 60 s for the chat
//! and 15 s for everything else (request sent and answer read).
//!
//! Failure is honest, and never a 2xx (a provider whose webhook could not be forwarded retries
//! it):
//! - `PYTHON_SERVICE_URL` unset (or not a valid http(s) URL): 503 `PYTHON_SERVICE_NOT_CONFIGURED`,
//!   not retryable;
//! - connection refused or failed, or an answer that was cut off or too large: 502
//!   `PYTHON_SERVICE_UNAVAILABLE`, retryable;
//! - connect or overall timeout: 504 `PYTHON_SERVICE_UNAVAILABLE`, retryable. After the request
//!   was sent, the message says that whether it took effect is unknown.
//!
//! `/ready` reports the link as `python_service` ([`PythonServiceStatus`]): `NOT_CONFIGURED`
//! when the setting is unset, otherwise the result of the latest `GET {PYTHON_SERVICE_URL}/health`
//! (checked every 5 s in the background, 2 s timeout), never `LIVE` on the strength of
//! configuration alone.
//!
//! The client is hyper's (`hyper-util`), asynchronous, so a slow chat occupies no thread; it
//! reads no proxy variables, adds no header but `host`, and speaks HTTP/1.1, over TLS (rustls,
//! webpki roots) for an `https` URL.

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use serde_json::{json, Map, Value};
use ss_core::IntegrationStatus;

use crate::app::{App, ProviderStatus};
use crate::config::Settings;
use crate::contracts::ProviderStatusEntry;
use crate::error::ProblemDetail;
use crate::logging;

/// The setting that names the Python service's base URL.
pub const SETTING: &str = "PYTHON_SERVICE_URL";
/// The name of the `/ready` entry.
pub const STATUS_NAME: &str = "python_service";
/// Largest request body forwarded (2 MiB).
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
/// Largest answer passed back (8 MiB).
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const NOT_CONFIGURED: &str = "PYTHON_SERVICE_NOT_CONFIGURED";
pub const UNAVAILABLE: &str = "PYTHON_SERVICE_UNAVAILABLE";
/// How long an idle pooled connection to the Python service is kept.
pub const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(4);

/// Which overall timeout a forwarded route gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// The supervisor chat (a model call): 60 s.
    Chat,
    /// Everything else: 15 s.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub chat: Duration,
    pub default: Duration,
    pub health: Duration,
    pub health_interval: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(3),
            chat: Duration::from_secs(60),
            default: Duration::from_secs(15),
            health: Duration::from_secs(2),
            health_interval: Duration::from_secs(5),
        }
    }
}

impl Timeouts {
    pub fn of(&self, budget: Budget) -> Duration {
        match budget {
            Budget::Chat => self.chat,
            Budget::Default => self.default,
        }
    }
}

/// `PYTHON_SERVICE_URL`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    NotConfigured,
    /// Set, but not usable; the reason never repeats the value (it could hold credentials).
    Invalid(&'static str),
    /// `scheme://authority[/prefix]`, without a trailing slash.
    Base(String),
}

/// Check the configured value: empty or blank is unset; otherwise an absolute `http` or
/// `https` URL with a host, no credentials, no query or fragment, and an optional path prefix.
pub fn parse_target(raw: Option<&str>) -> Target {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Target::NotConfigured;
    };
    if raw.contains(['#', '?']) {
        return Target::Invalid("it must not have a query or fragment");
    }
    let Ok(uri) = raw.parse::<Uri>() else {
        return Target::Invalid("it is not a URL");
    };
    let scheme = match uri.scheme_str() {
        Some("http") => "http",
        Some("https") => "https",
        _ => return Target::Invalid("it must start with http:// or https://"),
    };
    let Some(authority) = uri.authority() else {
        return Target::Invalid("it has no host");
    };
    if authority.as_str().contains('@') {
        return Target::Invalid("credentials in the URL are not supported");
    }
    if authority.host().is_empty() {
        return Target::Invalid("it has no host");
    }
    let prefix = uri.path().trim_end_matches('/');
    Target::Base(format!("{scheme}://{authority}{prefix}"))
}

type HttpsClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// The latest `GET /health` of the Python service.
#[derive(Debug, Clone)]
enum Health {
    NotChecked,
    Checked {
        at: Instant,
        outcome: Result<u16, &'static str>,
    },
}

/// The link: the checked target, one pooled client, the timeouts and the latest health check.
pub struct PythonLink {
    target: Target,
    client: Option<HttpsClient>,
    timeouts: Timeouts,
    health: Mutex<Health>,
}

/// Why a forward did not produce an answer from the Python service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// The connection could not be opened in time; nothing was sent.
    ConnectTimeout,
    /// The connection was refused; nothing was sent.
    Refused,
    /// The connection could not be opened (DNS, routing, TLS); nothing was sent.
    Connect,
    /// The request or its answer failed after the connection was open.
    Transport,
    /// The overall timeout passed.
    Timeout,
    /// The answer was larger than [`MAX_RESPONSE_BYTES`].
    TooLarge,
    /// The Python service asked to switch protocols, which is not forwarded.
    Upgrade,
}

impl Failure {
    fn class(self) -> &'static str {
        match self {
            Failure::ConnectTimeout => "connect_timeout",
            Failure::Refused => "connection_refused",
            Failure::Connect => "connect_failed",
            Failure::Transport => "transport_error",
            Failure::Timeout => "timeout",
            Failure::TooLarge => "response_too_large",
            Failure::Upgrade => "protocol_upgrade",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Failure::ConnectTimeout | Failure::Timeout => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        }
    }

    /// Was the request (possibly) sent?
    fn after_sending(self) -> bool {
        !matches!(
            self,
            Failure::ConnectTimeout | Failure::Refused | Failure::Connect
        )
    }
}

fn classify(e: &hyper_util::client::legacy::Error) -> Failure {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    let mut kind = None;
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            kind = Some(io.kind());
        }
        source = s.source();
    }
    match (e.is_connect(), kind) {
        (true, Some(std::io::ErrorKind::TimedOut)) => Failure::ConnectTimeout,
        (true, Some(std::io::ErrorKind::ConnectionRefused)) => Failure::Refused,
        (true, _) => Failure::Connect,
        (false, _) => Failure::Transport,
    }
}

/// A `ProblemDetail` answer with this request's id.
pub fn problem(
    status: StatusCode,
    code: &str,
    message: String,
    details: Value,
    retryable: bool,
) -> Response {
    let details = match details {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let body = ProblemDetail {
        error_code: code.to_owned(),
        message,
        details,
        request_id: logging::current_request_id(),
        retryable,
    };
    (status, Json(body)).into_response()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection" | "keep-alive" | "transfer-encoding" | "te" | "trailer" | "upgrade"
    ) || name.as_str().starts_with("proxy-")
}

/// The header names a `Connection` header lists (they are hop-by-hop too), lower case.
fn connection_listed(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// The end-to-end headers of a message: hop-by-hop ones, those `Connection` lists, and `drop`
/// are left out; repeated headers keep every value, in order.
fn end_to_end(headers: &HeaderMap, drop: &[HeaderName]) -> HeaderMap {
    let listed = connection_listed(headers);
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if is_hop_by_hop(name) || drop.contains(name) || listed.iter().any(|l| l == name.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

fn is_cors(name: &HeaderName) -> bool {
    name.as_str().starts_with("access-control-")
}

impl PythonLink {
    pub fn new(target: Target, timeouts: Timeouts) -> Self {
        let (target, client) = match target {
            Target::Base(base) => match build_client(timeouts.connect) {
                Ok(c) => (Target::Base(base), Some(c)),
                Err(_) => (Target::Invalid("no TLS configuration could be built"), None),
            },
            other => (other, None),
        };
        Self {
            target,
            client,
            timeouts,
            health: Mutex::new(Health::NotChecked),
        }
    }

    pub fn from_settings(settings: &Settings, timeouts: Timeouts) -> Self {
        Self::new(
            parse_target(settings.python_service_url.as_deref()),
            timeouts,
        )
    }

    /// The registered link, or one built from the settings (no health checks) for an app that
    /// was assembled without [`register`].
    pub fn of(app: &App) -> Arc<PythonLink> {
        app.extensions.get::<PythonLink>().unwrap_or_else(|| {
            Arc::new(PythonLink::from_settings(
                &app.settings,
                Timeouts::default(),
            ))
        })
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    fn base(&self) -> Option<(&str, &HttpsClient)> {
        match (&self.target, &self.client) {
            (Target::Base(b), Some(c)) => Some((b.as_str(), c)),
            _ => None,
        }
    }

    fn not_configured(&self) -> Response {
        let message = match &self.target {
            Target::Invalid(why) => format!(
                "This part of Second Site runs in the Python service, and {SETTING} on this \
                 server is not usable ({why})."
            ),
            _ => format!(
                "This part of Second Site (owner messaging and the project chat) runs in the \
                 Python service, and {SETTING} is not set on this server."
            ),
        };
        problem(
            StatusCode::SERVICE_UNAVAILABLE,
            NOT_CONFIGURED,
            message,
            json!({"service": "python"}),
            false,
        )
    }

    fn failed(
        &self,
        method: &Method,
        path: &str,
        failure: Failure,
        budget: Budget,
        ms: f64,
    ) -> Response {
        logging::warning(
            "app.forward",
            "python service unavailable",
            &[
                ("method", Value::String(method.to_string())),
                ("path", Value::String(path.to_owned())),
                ("reason", Value::String(failure.class().into())),
                ("ms", json!((ms * 10.0).round() / 10.0)),
            ],
        );
        let what = match failure {
            Failure::ConnectTimeout => format!(
                "could not be reached within {} s",
                self.timeouts.connect.as_secs_f64()
            ),
            Failure::Refused => "refused the connection".to_owned(),
            Failure::Connect => "could not be reached".to_owned(),
            Failure::Transport => "dropped the connection".to_owned(),
            Failure::Timeout => format!(
                "did not answer within {} s",
                self.timeouts.of(budget).as_secs_f64()
            ),
            Failure::TooLarge => format!(
                "answered with more than {} MiB",
                MAX_RESPONSE_BYTES / (1024 * 1024)
            ),
            Failure::Upgrade => "asked to switch protocols, which is not forwarded".to_owned(),
        };
        let effect = if !failure.after_sending() {
            "Nothing was forwarded."
        } else if method == Method::GET {
            "Try again."
        } else {
            "Whether the request took effect there is unknown."
        };
        problem(
            failure.status(),
            UNAVAILABLE,
            format!("The Python service {what}; this part of Second Site runs there. {effect}"),
            json!({"service": "python", "reason": failure.class()}),
            true,
        )
    }

    /// Forward one request (already matched against the forwarded routes) and pass the answer
    /// back.
    pub async fn forward(&self, req: Request<Body>, budget: Budget) -> Response {
        let Some((base, client)) = self.base() else {
            return self.not_configured();
        };
        let started = Instant::now();
        let (parts, body) = req.into_parts();
        let path = parts.uri.path().to_owned();
        let declared = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if declared.is_some_and(|n| n > MAX_REQUEST_BYTES as u64) {
            return too_large();
        }
        let bytes = match Limited::new(body, MAX_REQUEST_BYTES).collect().await {
            Ok(c) => c.to_bytes(),
            Err(e)
                if e.downcast_ref::<http_body_util::LengthLimitError>()
                    .is_some() =>
            {
                return too_large()
            }
            Err(_) => {
                return problem(
                    StatusCode::BAD_REQUEST,
                    "BAD_REQUEST",
                    "the request body could not be read".into(),
                    json!({}),
                    false,
                )
            }
        };
        let target = match parts.uri.query() {
            Some(q) => format!("{base}{path}?{q}"),
            None => format!("{base}{path}"),
        };
        let Ok(uri) = target.parse::<Uri>() else {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "unexpected server error".into(),
                json!({}),
                true,
            );
        };
        let mut headers = end_to_end(
            &parts.headers,
            &[
                header::HOST,
                header::CONTENT_LENGTH,
                header::EXPECT,
                HeaderName::from_static("x-request-id"),
            ],
        );
        if let Some(rid) =
            logging::current_request_id().and_then(|r| HeaderValue::from_str(&r).ok())
        {
            headers.insert(HeaderName::from_static("x-request-id"), rid);
        }
        let carries_body = !bytes.is_empty()
            || declared.is_some()
            || matches!(parts.method, Method::POST | Method::PUT | Method::PATCH);
        if carries_body {
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
        }
        let mut up = Request::new(Full::new(bytes));
        *up.method_mut() = parts.method.clone();
        *up.uri_mut() = uri;
        *up.headers_mut() = headers;
        let exchange = async {
            let resp = client.request(up).await.map_err(|e| classify(&e))?;
            let (rparts, rbody) = resp.into_parts();
            if rparts.status == StatusCode::SWITCHING_PROTOCOLS {
                return Err(Failure::Upgrade);
            }
            let bytes = match Limited::new(rbody, MAX_RESPONSE_BYTES).collect().await {
                Ok(c) => c.to_bytes(),
                Err(e)
                    if e.downcast_ref::<http_body_util::LengthLimitError>()
                        .is_some() =>
                {
                    return Err(Failure::TooLarge)
                }
                Err(_) => return Err(Failure::Transport),
            };
            Ok((rparts, bytes))
        };
        let ms = || started.elapsed().as_secs_f64() * 1000.0;
        match tokio::time::timeout(self.timeouts.of(budget), exchange).await {
            Err(_) => self.failed(&parts.method, &path, Failure::Timeout, budget, ms()),
            Ok(Err(f)) => self.failed(&parts.method, &path, f, budget, ms()),
            Ok(Ok((rparts, bytes))) => {
                let mut resp = Response::new(Body::from(bytes));
                *resp.status_mut() = rparts.status;
                let mut headers = end_to_end(&rparts.headers, &[header::CONTENT_LENGTH]);
                let cors: Vec<HeaderName> =
                    headers.keys().filter(|n| is_cors(n)).cloned().collect();
                for name in cors {
                    headers.remove(&name);
                }
                *resp.headers_mut() = headers;
                resp
            }
        }
    }

    /// One `GET {PYTHON_SERVICE_URL}/health` with the short health timeout; the result is kept
    /// for `/ready`.
    pub async fn check_health(&self) {
        let Some((base, client)) = self.base() else {
            return;
        };
        let outcome = match format!("{base}/health").parse::<Uri>() {
            Err(_) => Err("not a URL"),
            Ok(uri) => {
                let mut req = Request::new(Full::new(Bytes::new()));
                *req.uri_mut() = uri;
                let exchange = async {
                    let resp = client.request(req).await.map_err(|e| classify(&e))?;
                    let status = resp.status().as_u16();
                    // Read (at most 64 KiB of) the answer so the connection can be reused.
                    let _ = Limited::new(resp.into_body(), 64 * 1024).collect().await;
                    Ok::<u16, Failure>(status)
                };
                match tokio::time::timeout(self.timeouts.health, exchange).await {
                    Err(_) => Err(Failure::Timeout.class()),
                    Ok(Err(f)) => Err(f.class()),
                    Ok(Ok(status)) => Ok(status),
                }
            }
        };
        if let Ok(mut h) = self.health.lock() {
            *h = Health::Checked {
                at: Instant::now(),
                outcome,
            };
        }
    }

    /// The `/ready` entry.
    pub fn status_entry(&self) -> ProviderStatusEntry {
        let entry = |mode: &str, status, note: String| ProviderStatusEntry {
            provider: STATUS_NAME.to_owned(),
            mode: mode.to_owned(),
            status,
            credentials_present: false,
            note,
        };
        match &self.target {
            Target::NotConfigured => entry(
                "not_configured",
                IntegrationStatus::NotStarted,
                format!(
                    "NOT_CONFIGURED: {SETTING} is not set. Owner messaging, the Photon webhook \
                     and the project chat run in the Python service; their routes answer 503 \
                     {NOT_CONFIGURED} here."
                ),
            ),
            Target::Invalid(why) => entry(
                "forward",
                IntegrationStatus::Blocked,
                format!("{SETTING} is not usable ({why}); forwarded routes answer 503."),
            ),
            Target::Base(_) => {
                let health = self
                    .health
                    .lock()
                    .map(|h| h.clone())
                    .unwrap_or(Health::NotChecked);
                let stale_after = self.timeouts.health_interval * 3 + self.timeouts.health;
                match health {
                    Health::NotChecked => entry(
                        "forward",
                        IntegrationStatus::NotStarted,
                        "GET /health of the Python service has not been answered yet.".into(),
                    ),
                    Health::Checked { at, .. } if at.elapsed() > stale_after => entry(
                        "forward",
                        IntegrationStatus::Blocked,
                        format!(
                            "The last GET /health of the Python service is {} s old; the \
                             check is not running.",
                            at.elapsed().as_secs()
                        ),
                    ),
                    Health::Checked { at, outcome } => {
                        let age = at.elapsed().as_secs();
                        match outcome {
                            Ok(s) if (200..300).contains(&s) => entry(
                                "forward",
                                IntegrationStatus::Live,
                                format!(
                                    "GET /health of the Python service answered HTTP {s} {age} s \
                                     ago. Photon owner messaging and the project chat are \
                                     forwarded to it."
                                ),
                            ),
                            Ok(s) => entry(
                                "forward",
                                IntegrationStatus::Blocked,
                                format!(
                                    "GET /health of the Python service answered HTTP {s} {age} s \
                                     ago."
                                ),
                            ),
                            Err(class) => entry(
                                "forward",
                                IntegrationStatus::Blocked,
                                format!(
                                    "GET /health of the Python service failed {age} s ago \
                                     ({class})."
                                ),
                            ),
                        }
                    }
                }
            }
        }
    }
}

fn too_large() -> Response {
    problem(
        StatusCode::PAYLOAD_TOO_LARGE,
        "PAYLOAD_TOO_LARGE",
        format!(
            "the request body is larger than {} MiB, the limit for requests forwarded to the \
             Python service",
            MAX_REQUEST_BYTES / (1024 * 1024)
        ),
        json!({"limit_bytes": MAX_REQUEST_BYTES}),
        false,
    )
}

fn build_client(connect_timeout: Duration) -> Result<HttpsClient, rustls::Error> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(connect_timeout));
    let connector = HttpsConnectorBuilder::new()
        .with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())?
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    let mut builder = Client::builder(TokioExecutor::new());
    // Below uvicorn's 5 s keep-alive, so a pooled connection is not reused just as the Python
    // service closes it.
    builder
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .pool_timer(TokioTimer::new());
    Ok(builder.build(connector))
}

/// The `/ready` reporter.
pub struct PythonServiceStatus {
    link: Arc<PythonLink>,
}

impl ProviderStatus for PythonServiceStatus {
    fn name(&self) -> &str {
        STATUS_NAME
    }

    fn status(&self, _settings: &Settings) -> ProviderStatusEntry {
        self.link.status_entry()
    }
}

/// Register the link from the settings (one line in `lib.rs::register_providers`).
pub fn register(app: &App) {
    install(
        app,
        Arc::new(PythonLink::from_settings(
            &app.settings,
            Timeouts::default(),
        )),
    );
}

/// Install `link` as the app's link to the Python service: shared with the forwarded routes,
/// reported by `/ready`, and health-checked in the background while the app holds it (tests
/// install one with short timeouts).
pub fn install(app: &App, link: Arc<PythonLink>) {
    app.extensions.insert(link.clone());
    app.providers
        .register(Arc::new(PythonServiceStatus { link: link.clone() }));
    if link.base().is_some() {
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(poll_health(Arc::downgrade(&link)));
        } else {
            logging::warning(
                "app.forward",
                "no async runtime: the Python service's health is not checked",
                &[],
            );
        }
    }
}

/// Check the health every `health_interval` until the link is dropped.
async fn poll_health(link: Weak<PythonLink>) {
    loop {
        let Some(l) = link.upgrade() else {
            return;
        };
        l.check_health().await;
        let interval = l.timeouts.health_interval;
        drop(l);
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_checked() {
        assert_eq!(parse_target(None), Target::NotConfigured);
        assert_eq!(parse_target(Some("  ")), Target::NotConfigured);
        assert_eq!(
            parse_target(Some("http://python:8001")),
            Target::Base("http://python:8001".into())
        );
        assert_eq!(
            parse_target(Some(" https://py.example.com/prefix/ ")),
            Target::Base("https://py.example.com/prefix".into())
        );
        assert_eq!(
            parse_target(Some("http://127.0.0.1:9/")),
            Target::Base("http://127.0.0.1:9".into())
        );
        for bad in [
            "python:8001",
            "ftp://python",
            "http://user:pw@python",
            "http://python/?a=1",
            "http://python/#x",
            "/relative",
            "http://",
        ] {
            assert!(
                matches!(parse_target(Some(bad)), Target::Invalid(_)),
                "{bad}"
            );
        }
        // The reason never repeats the value.
        let Target::Invalid(why) = parse_target(Some("http://user:secret@python")) else {
            panic!()
        };
        assert!(!why.contains("secret"));
    }

    #[test]
    fn timeouts_are_the_briefs() {
        let t = Timeouts::default();
        assert_eq!(t.connect, Duration::from_secs(3));
        assert_eq!(t.of(Budget::Chat), Duration::from_secs(60));
        assert_eq!(t.of(Budget::Default), Duration::from_secs(15));
        assert_eq!(MAX_REQUEST_BYTES, 2_097_152);
    }

    #[test]
    fn hop_by_hop_headers_are_dropped() {
        let mut h = HeaderMap::new();
        for (k, v) in [
            ("connection", "keep-alive, x-hop"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("te", "trailers"),
            ("trailer", "x-t"),
            ("upgrade", "websocket"),
            ("proxy-authorization", "Basic x"),
            ("proxy-connection", "keep-alive"),
            ("x-hop", "1"),
            ("host", "rust.example"),
            ("authorization", "Bearer t"),
            ("webhook-signature", "v1,abc"),
            ("set-cookie", "a=1"),
        ] {
            h.append(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        h.append(header::SET_COOKIE, HeaderValue::from_static("b=2"));
        let out = end_to_end(&h, &[header::HOST]);
        let names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["authorization", "webhook-signature", "set-cookie"]);
        assert_eq!(out.get_all(header::SET_COOKIE).iter().count(), 2);
    }
}
