//! Port of `api/routes/health.py`.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use indexmap::IndexMap;

use crate::app::AppState;
use crate::contracts::{HealthResponse, ReadyResponse};
use crate::store::Backend;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
}

async fn health(State(app): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
        version: app.settings.app_version.clone(),
    })
}

/// In-memory mode is reported honestly: the store answers, but nothing is durable, so the
/// service is `degraded` rather than `ready`.
pub const MEMORY_DATABASE_CHECK: &str = "memory: not durable, data is lost on restart";

async fn ready(State(app): State<AppState>) -> Json<ReadyResponse> {
    let mut checks: IndexMap<String, String> = IndexMap::new();
    let database = match app.store.ping().await {
        Ok(()) if app.store.backend() == Backend::Memory => MEMORY_DATABASE_CHECK.to_owned(),
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("error: {}", e.kind()),
    };
    checks.insert("database".into(), database);
    let missing = app.settings.missing_for_live();
    if !missing.is_empty() {
        let parts: Vec<String> = missing
            .iter()
            .map(|(k, v)| format!("{k}={}", v.join(",")))
            .collect();
        checks.insert(
            "live_config".into(),
            format!("missing: {}", parts.join("; ")),
        );
    }
    if app.settings.public_path_guard {
        checks.insert(
            "public_path_guard".into(),
            crate::http::public_guard_check(),
        );
    }
    let ok = checks.get("database").map(String::as_str) == Some("ok");
    Json(ReadyResponse {
        status: if ok { "ready" } else { "degraded" }.into(),
        checks,
        providers: app.providers.statuses(&app.settings),
    })
}
