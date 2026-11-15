//! Router assembly and middleware (port of `apps/api/app/main.py`): request id in and out, one
//! structured request log line, CORS from `CORS_ORIGINS`, ProblemDetail for panics, FastAPI's
//! `{"detail": ...}` bodies for unknown paths and methods.
//!
//! Layer order matches Starlette's: the request-context middleware is outermost, CORS inside it.
//! Unlike Python, a 500 also carries `request_id` and the `x-request-id` header (Python's
//! catch-all handler runs outside its request middleware).
//!
//! Outside all of it, around the whole router, sits the public path guard (`PUBLIC_PATH_GUARD`,
//! off by default): see [`public_path_guard`].
//!
//! Inside CORS, every request body is read under an idle bound ([`BODY_IDLE_TIMEOUT`]): a body
//! that stops arriving is answered 408 `REQUEST_TIMEOUT` instead of holding its task and socket
//! for ever (see [`body_idle_bound`]). Responses (the event stream included) are not bounded.

pub mod extract;
pub mod routes;

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use uuid::Uuid;

use crate::app::AppState;
use crate::config::Settings;
use crate::error::{AppError, ProblemDetail};
use crate::logging::{self, REQUEST_ID};
use crate::services::forward;

/// The longest a request body may go without delivering data while its handler is reading it:
/// 30 s, measured afresh after every chunk that arrives, so a slow but steady upload (a phone
/// sending a photo over a poor link) is never cut off, only one that has stopped. It is the
/// usual idle bound of HTTP servers (nginx's `client_body_timeout` default is 60 s, uvicorn has
/// none), short enough that a stalled client holds a task and a socket for half a minute rather
/// than for ever. Python's service has no such bound; this is a Rust-only hardening.
pub const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// The `error_code` of the answer to a body that stopped arriving.
pub const REQUEST_TIMEOUT: &str = "REQUEST_TIMEOUT";

/// The error a bounded request body reports when no data arrived within the bound. The handler
/// reading the body sees it as a body read error; [`body_idle_bound`] then answers 408.
#[derive(Debug, Clone, Copy)]
pub struct BodyIdleTimeout {
    pub bound: Duration,
}

impl std::fmt::Display for BodyIdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no request body data arrived within {} s",
            self.bound.as_secs_f64()
        )
    }
}

impl std::error::Error for BodyIdleTimeout {}

/// The [`BodyIdleTimeout`] in this error's source chain, if any (a body read error, as an
/// extractor or `http_body_util::Limited` wraps it).
pub fn body_idle_timeout(e: &(dyn std::error::Error + 'static)) -> Option<BodyIdleTimeout> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(t) = err.downcast_ref::<BodyIdleTimeout>() {
            return Some(*t);
        }
        cur = err.source();
    }
    None
}

/// Marks a response that already is the answer to a stalled body, so [`body_idle_bound`] keeps
/// it (the forwarder's, which says that nothing was forwarded).
#[derive(Debug, Clone, Copy)]
struct BodyStalledAnswer;

/// The 408 for a request body that stopped arriving (`forwarded`: the request was for the Python
/// service). Honest about the effect: the body was never complete, so no handler acted on it and
/// nothing was forwarded; sending it again is safe. The connection is closed, since the rest of
/// the body may still arrive on it.
pub fn body_stalled_response(bound: Duration, forwarded: bool) -> Response {
    let effect = if forwarded {
        "nothing was forwarded to the Python service"
    } else {
        "the request was not processed"
    };
    let body = ProblemDetail {
        error_code: REQUEST_TIMEOUT.to_owned(),
        message: format!(
            "The request body stopped arriving (no data for {} s), so {effect}. Send it again.",
            bound.as_secs_f64()
        ),
        details: [("idle_timeout_s".to_owned(), json!(bound.as_secs_f64()))]
            .into_iter()
            .collect(),
        request_id: logging::current_request_id(),
        retryable: true,
    };
    let mut resp = (StatusCode::REQUEST_TIMEOUT, Json(body)).into_response();
    resp.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    resp.extensions_mut().insert(BodyStalledAnswer);
    resp
}

/// A request body under the idle bound: the timer starts when the reader waits for data and is
/// reset by every frame that arrives; when it runs out the body reports [`BodyIdleTimeout`] and
/// sets `stalled`. Data that is ready always wins over the timer.
struct IdleBoundBody {
    inner: Body,
    bound: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    stalled: Arc<AtomicBool>,
}

impl hyper::body::Body for IdleBoundBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, axum::Error>>> {
        let this = &mut *self;
        if let Poll::Ready(frame) = Pin::new(&mut this.inner).poll_frame(cx) {
            this.sleep = None;
            return Poll::Ready(frame);
        }
        let bound = this.bound;
        let sleep = this
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(bound)));
        match sleep.as_mut().poll(cx) {
            Poll::Ready(()) => {
                this.stalled.store(true, Ordering::SeqCst);
                Poll::Ready(Some(Err(axum::Error::new(BodyIdleTimeout { bound }))))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Can this request carry a body (HTTP/1.1 framing: a non-zero `content-length` or any
/// `transfer-encoding`)? Without one the body is empty and cannot stall.
fn may_have_body(headers: &HeaderMap) -> bool {
    headers.contains_key(header::TRANSFER_ENCODING)
        || headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() != "0")
}

/// The body idle bound middleware. A request whose body stalled and whose handler answered with
/// an error (an extractor's "failed to buffer the body", a multipart error, the forwarder's) is
/// answered with [`body_stalled_response`] instead, so every route gives the same honest 408.
/// A success answer is kept as it is (no handler here succeeds on a body it could not read, but
/// if one did, replacing its answer would hide what it did).
async fn body_idle_bound(bound: Duration, req: Request, next: Next) -> Response {
    if !may_have_body(req.headers()) {
        return next.run(req).await;
    }
    let stalled = Arc::new(AtomicBool::new(false));
    let flag = stalled.clone();
    let req = req.map(|inner| {
        Body::new(IdleBoundBody {
            inner,
            bound,
            sleep: None,
            stalled: flag,
        })
    });
    let resp = next.run(req).await;
    let failed = resp.status().is_client_error() || resp.status().is_server_error();
    if stalled.load(Ordering::SeqCst)
        && failed
        && resp.extensions().get::<BodyStalledAnswer>().is_none()
    {
        return body_stalled_response(bound, false);
    }
    resp
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response()
}

async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({"detail": "Method Not Allowed"})),
    )
        .into_response()
}

fn panic_response(err: Box<dyn Any + Send + 'static>) -> Response {
    let detail = if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        (*s).to_owned()
    } else {
        "panic".to_owned()
    };
    AppError::internal_kind("Panic", detail).into_response()
}

/// Path prefixes whose next segment is a credential (a signed link that works without a login).
/// The request log writes the prefix and `[redacted]` in place of everything after it. Add a
/// prefix here whenever a route takes a token, signature or other secret in its path; a secret in
/// the query string or the URL fragment is never logged (only `uri().path()` is written).
const SECRET_PATH_PREFIXES: &[&str] = &["/api/v1/media/public/"];

/// The request path as the request log writes it: a path under [`SECRET_PATH_PREFIXES`] becomes
/// the prefix plus `[redacted]`; every other path is returned unchanged.
pub fn loggable_path(path: &str) -> std::borrow::Cow<'_, str> {
    for prefix in SECRET_PATH_PREFIXES {
        if path.starts_with(prefix) {
            return format!("{prefix}[redacted]").into();
        }
    }
    path.into()
}

/// What a public request may reach when `PUBLIC_PATH_GUARD` is on: method and exact path shape
/// (`{x}` is one non-empty segment of `A-Z a-z 0-9 . _ ~ -`). `/ready` lists these lines.
pub const PUBLIC_SHAPES: &[&str] = &[
    "POST /webhooks/telnyx",
    "GET /api/v1/voice/stream/{bridge_id}",
    "GET|HEAD /api/v1/media/public/{token}",
    "GET /record",
    "POST /record/check",
    "POST /record/upload",
];

/// Headers a tunnel's edge or a reverse proxy adds; a request carrying any of them is public.
const FORWARDING_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "cf-connecting-ip",
    "cf-ray",
    "cdn-loop",
    "via",
];

/// The longest path a public request may have, in bytes.
const PUBLIC_PATH_MAX: usize = 512;

/// Is this `Host` value (or URI authority) a loopback name: `localhost`, `127.0.0.1` or `[::1]`,
/// compared case-insensitively, alone or followed by `:` and one to five digits. Nothing is
/// trimmed or normalised: a trailing dot, a space, user info or an empty port is not loopback.
fn is_loopback_authority(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    ["localhost", "127.0.0.1", "[::1]"].iter().any(|name| {
        v.strip_prefix(name).is_some_and(|rest| {
            rest.is_empty()
                || rest.strip_prefix(':').is_some_and(|port| {
                    (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit())
                })
        })
    })
}

/// Did this request come from outside this machine, as far as its headers can tell? Public unless
/// it has exactly one `Host` header naming loopback, a request target without an authority or
/// with a loopback one, and no forwarding header. Every doubt (no `Host`, two, not text) is public.
fn is_public_request(req: &Request) -> bool {
    let headers = req.headers();
    let mut hosts = headers.get_all(header::HOST).iter();
    let local_host = match (hosts.next(), hosts.next()) {
        (Some(one), None) => one.to_str().is_ok_and(is_loopback_authority),
        _ => false,
    };
    let local_target = req
        .uri()
        .authority()
        .is_none_or(|a| is_loopback_authority(a.as_str()));
    !local_host || !local_target || FORWARDING_HEADERS.iter().any(|h| headers.contains_key(*h))
}

/// One path segment a public request may put after an allowed prefix: non-empty, unreserved
/// characters only (so no `/`, `%`, `;` or `\`), and not a dot segment.
fn is_plain_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
}

/// Is this method and raw path (as received: not decoded, not normalised) one of
/// [`PUBLIC_SHAPES`]? Anything that would need normalising to match is refused.
fn is_public_shape(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    if path.len() > PUBLIC_PATH_MAX {
        return false;
    }
    let under = |prefix: &str| path.strip_prefix(prefix).is_some_and(is_plain_segment);
    if method == Method::POST {
        matches!(
            path,
            "/webhooks/telnyx" | "/record/check" | "/record/upload"
        )
    } else if method == Method::GET {
        path == "/record" || under("/api/v1/voice/stream/") || under("/api/v1/media/public/")
    } else if method == Method::HEAD {
        under("/api/v1/media/public/")
    } else {
        false
    }
}

/// The public path guard (`PUBLIC_PATH_GUARD`, off by default). On, a public request
/// ([`is_public_request`]) that is not one of [`PUBLIC_SHAPES`] is answered with the ordinary 404
/// here, outside the router and every other middleware: before routing, the request log's
/// middleware, CORS, authentication, any handler and any read of the body. It writes the request
/// line itself, with `public_refused: true` and a request id of its own: nothing of the caller's
/// headers, query or body is logged or echoed. A request from this machine is never touched.
async fn public_path_guard(on: bool, req: Request, next: Next) -> Response {
    if !on {
        return next.run(req).await;
    }
    if !is_public_request(&req) || is_public_shape(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    let start = Instant::now();
    let rid = Uuid::new_v4().to_string();
    let mut resp = not_found().await;
    // The headers of the router's own 404 (axum's routing sets the length; this is outside it).
    if let Some(n) = hyper::body::Body::size_hint(resp.body()).exact() {
        resp.headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    }
    if let Ok(v) = HeaderValue::from_str(&rid) {
        resp.headers_mut().insert("x-request-id", v);
    }
    log_request(
        req.method().to_string(),
        req.uri().path(),
        resp.status(),
        &rid,
        start,
        true,
    );
    resp
}

/// The `/ready` check line of the guard when it is on.
pub fn public_guard_check() -> String {
    format!(
        "on: a request with a non-loopback Host or a forwarding header reaches only: {}; every other path is 404",
        PUBLIC_SHAPES.join("; ")
    )
}

/// `request_context` middleware.
async fn request_context(req: Request, next: Next) -> Response {
    let rid = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let method = req.method().to_string();
    let path = req.uri().path().to_owned();
    let start = Instant::now();
    let mut resp = REQUEST_ID.scope(rid.clone(), next.run(req)).await;
    if let Ok(v) = HeaderValue::from_str(&rid) {
        resp.headers_mut().insert("x-request-id", v);
    }
    if path == "/health" {
        // Lets the link to the Python service recognise this service (services::forward).
        resp.headers_mut().insert(
            forward::BACKEND_MARKER,
            HeaderValue::from_static(forward::BACKEND_MARKER_VALUE),
        );
    }
    log_request(method, &path, resp.status(), &rid, start, false);
    resp
}

/// The one request log line. `public_refused` adds the field `public_refused: true` (the public
/// path guard's 404); an ordinary request's line is as it always was.
fn log_request(
    method: String,
    path: &str,
    status: StatusCode,
    rid: &str,
    start: Instant,
    public_refused: bool,
) {
    let ms = (start.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0;
    REQUEST_ID.sync_scope(rid.to_owned(), || {
        let mut fields = vec![
            ("method", Value::String(method)),
            ("path", Value::String(loggable_path(path).into_owned())),
            ("status", json!(status.as_u16())),
            ("request_id", Value::String(rid.to_owned())),
            ("ms", json!(ms)),
        ];
        if public_refused {
            fields.push(("public_refused", Value::Bool(true)));
        }
        logging::info("app.request", "request", &fields);
    });
}

/// `CORSMiddleware(allow_origins=CORS_ORIGINS, allow_credentials=True, allow_methods=["*"],
/// allow_headers=["*"])`.
pub fn cors_layer(settings: &Settings) -> CorsLayer {
    let origins = settings.cors_origin_list();
    let allow_origin = if origins.iter().any(|o| o == "*") {
        AllowOrigin::mirror_request()
    } else {
        let mut values = Vec::new();
        for o in &origins {
            match HeaderValue::from_str(o) {
                Ok(v) => values.push(v),
                Err(_) => logging::warning(
                    "app.cors",
                    "ignoring invalid CORS origin",
                    &[("origin", Value::String(o.clone()))],
                ),
            }
        }
        AllowOrigin::list(values)
    };
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_credentials(true)
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request())
        .max_age(Duration::from_secs(600))
}

/// The full HTTP application.
pub fn router(state: AppState) -> Router {
    with_middleware(routes::api_routes(), state)
}

/// Wrap route groups in the application's fallbacks and middleware.
pub fn with_middleware(routes: Router<AppState>, state: AppState) -> Router {
    with_body_idle_timeout(routes, state, BODY_IDLE_TIMEOUT)
}

/// [`with_middleware`] with another request body idle bound (tests use a short one).
pub fn with_body_idle_timeout(
    routes: Router<AppState>,
    state: AppState,
    bound: Duration,
) -> Router {
    let cors = cors_layer(&state.settings);
    let guard_on = state.settings.public_path_guard;
    let app = routes
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            body_idle_bound(bound, req, next)
        }))
        .layer(cors)
        .layer(middleware::from_fn(request_context))
        .with_state(state);
    // The public path guard wraps the whole application as one service. A `Router::layer` is
    // applied per route, inside axum's own routing, which adds an `allow` header to the answer
    // for a known path with another method; out here a refused request never reaches routing.
    Router::new()
        .fallback_service(app)
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            public_path_guard(guard_on, req, next)
        }))
}

/// Response for a stream body with an explicit content type.
pub fn stream_response(content_type: &'static str, body: Body) -> Response {
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    resp
}
