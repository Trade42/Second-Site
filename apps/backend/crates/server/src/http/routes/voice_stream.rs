//! The carrier's media-stream WebSocket: `GET /api/v1/voice/stream/{bridge_id}?token=<secret>`.
//!
//! Outside user authentication (the carrier cannot sign in); the per-call secret the orchestrator
//! minted for the call's one bridge is the credential. Without it a caller can do nothing but be
//! refused: an unknown bridge id or a wrong or missing token is 401 before any upgrade (one answer
//! for both, constant-time comparison), a second connection with the right token is 409, and a
//! finished call's bridge is gone (401). No audio, transcript or token is logged.

use std::collections::HashMap;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;

use crate::app::AppState;
use crate::services::voice_bridge::accept_carrier;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/voice/stream/{bridge_id}", get(stream))
}

async fn stream(
    State(app): State<AppState>,
    Path(bridge_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let voice = app.voice_calls();
    voice.bind(&app);
    accept_carrier(
        &voice.registry,
        &bridge_id,
        q.get("token").map(String::as_str),
        ws,
    )
}
