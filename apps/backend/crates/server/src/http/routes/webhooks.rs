//! Port of the Telnyx route of `api/routes/webhooks.py`: verify the signature on the RAW body,
//! dedupe atomically on the provider event id, route to the call orchestrator. The Photon webhook
//! stays in the Python service.
//!
//! - The signature (`ss_domain::integrations::webhook_signatures::verify_telnyx`, Ed25519 over
//!   `"{telnyx-timestamp}|{raw body}"`, 5-minute tolerance, `data.id` required) is checked the
//!   same way in live and fixture mode; without `TELNYX_PUBLIC_KEY` every delivery is refused.
//!   A refused delivery is 400 `INVALID_SIGNATURE` and nothing is stored or processed.
//! - `processed_webhook_events` keeps one row per `(telnyx, data.id)` (`verified` is true only in
//!   live mode). A replayed id answers 204 and does nothing; the row and the call's changes
//!   commit together, so an event whose handling fails is not marked processed.
//! - Events of unknown calls are recorded and ignored (204), as Python ignores them.
//! - Owner texts the event caused are sent after the commit.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use ss_domain::integrations::webhook_signatures::{
    header_value_latin1, verify_telnyx, WebhookError, WebhookMode,
};

use crate::app::AppState;
use crate::config::ProviderMode;
use crate::error::{AppError, AppResult};
use crate::services::call_orchestration::CallOrchestrator;
use crate::store::comms::webhooks::record_once;

pub fn router() -> Router<AppState> {
    Router::new().route("/webhooks/telnyx", post(telnyx))
}

/// The request headers as Starlette gives them to Python: names lower-case, values latin-1.
pub fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), header_value_latin1(v.as_bytes())))
        .collect()
}

fn webhook_error(e: WebhookError) -> AppError {
    match e {
        WebhookError::InvalidSignature(m) => AppError::invalid_signature(m),
        WebhookError::Unhandled(m) => AppError::internal_kind("ValueError", m),
    }
}

async fn telnyx(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<StatusCode> {
    let mode = if app.settings.voice_provider_mode == ProviderMode::Live {
        WebhookMode::Live
    } else {
        WebhookMode::Fixture
    };
    let verified = verify_telnyx(
        app.settings.telnyx_public_key.as_deref(),
        &header_pairs(&headers),
        &body,
        mode,
        None,
    )
    .map_err(webhook_error)?;
    let event = ss_domain::py::json_loads_bytes(&body)
        .map_err(|e| AppError::internal_kind("JSONDecodeError", e))?;
    // Bridge events of the calls this event starts open their own transactions on this app.
    app.voice_calls().bind(&app);
    let notices = crate::in_tx!(app, |tx| {
        if !record_once(tx, &verified, &event).await? {
            Vec::new() // duplicate: already accepted, do nothing again
        } else {
            let mut orch = CallOrchestrator::new(&app);
            orch.handle_event(tx, &event).await?;
            orch.take_notices()
        }
    });
    app.owner_notify().dispatch(app.clone(), notices);
    Ok(StatusCode::NO_CONTENT)
}
