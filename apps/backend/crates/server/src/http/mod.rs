//! Router assembly and middleware (port of `apps/api/app/main.py`): request id in and out, one
//! structured request log line, CORS from `CORS_ORIGINS`, ProblemDetail for panics, FastAPI's
//! `{"detail": ...}` bodies for unknown paths and methods.
//!
//! Layer order matches Starlette's: the request-context middleware is outermost, CORS inside it.
//! Unlike Python, a 500 also carries `request_id` and the `x-request-id` header (Python's
//! catch-all handler runs outside its request middleware).

pub mod extract;
pub mod routes;

use std::any::Any;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use uuid::Uuid;

use crate::app::AppState;
use crate::config::Settings;
use crate::error::AppError;
use crate::logging::{self, REQUEST_ID};

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
    let ms = (start.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0;
    REQUEST_ID.sync_scope(rid.clone(), || {
        logging::info(
            "app.request",
            "request",
            &[
                ("method", Value::String(method)),
                ("path", Value::String(path)),
                ("status", json!(resp.status().as_u16())),
                ("request_id", Value::String(rid.clone())),
                ("ms", json!(ms)),
            ],
        );
    });
    resp
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
    let cors = cors_layer(&state.settings);
    routes
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(cors)
        .layer(middleware::from_fn(request_context))
        .with_state(state)
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
