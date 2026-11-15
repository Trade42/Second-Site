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
    // Live voice only: the settings without which a live dial is refused (names, never values).
    let voice_missing = app.settings.voice_live_missing();
    if !voice_missing.is_empty() {
        checks.insert(
            "telnyx_live_dial".into(),
            format!("refused until set: {}", voice_missing.join(", ")),
        );
    }
    // Live voice only: whom a live dial would ring. Says whether the test number is set, never
    // the number.
    if app.settings.voice_provider_mode == crate::config::ProviderMode::Live {
        use crate::config::LiveCallTarget;
        let target = match app.settings.live_call_target() {
            LiveCallTarget::TestNumber(_) => {
                "the owner's test number only (LIVE_CALLS_ONLY_TO is set); no listing contact is \
                 dialed"
            }
            LiveCallTarget::Contacts => {
                "each listing contact's own number (LIVE_CALLS_TO_CONTACTS=true, \
                 LIVE_CALLS_ONLY_TO is not set)"
            }
            LiveCallTarget::Nobody => {
                "nobody: every live dial is refused until LIVE_CALLS_ONLY_TO is set to the \
                 owner's test number (+ and 8 to 15 digits)"
            }
        };
        checks.insert("live_call_target".into(), target.to_owned());
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
