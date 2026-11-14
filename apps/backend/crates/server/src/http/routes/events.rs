//! Port of `api/routes/events.py`: the persisted event log, as a page and as Server-Sent Events.
//!
//! The stream polls the event log (the source of truth) once a second for up to 3600 polls, each
//! poll in its own short read-only transaction (no transaction is held between polls). Framing is
//! Python's: `id: <seq>\nevent: <event_type>\ndata: <json.dumps(event)>\n\n` per event and
//! `: keepalive\n\n` after every poll.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;
use uuid::Uuid;

use crate::app::AppState;
use crate::contracts::{ActivityEvent, EventPage};
use crate::error::AppResult;
use crate::http::extract::{collect_query_errors, AuthUser, IntBounds, PathParams, QueryParams};
use crate::http::stream_response;
use crate::{logging, pyjson};

pub const POLLS: u32 = 3600;
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const STREAM_BATCH: i64 = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/projects/{project_id}/events", get(list_events))
        .route(
            "/api/v1/projects/{project_id}/events/stream",
            get(stream_events),
        )
}

const AFTER: IntBounds = IntBounds {
    default: 0,
    ge: Some(0),
    le: None,
};
const LIMIT: IntBounds = IntBounds {
    default: 200,
    ge: Some(1),
    le: Some(500),
};

async fn list_events(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
    query: QueryParams,
) -> AppResult<Json<EventPage>> {
    let project_id = params.uuid("project_id")?;
    let page = crate::in_tx!(app, |tx| {
        let project = app.authz().project_for_user(tx, project_id, user).await?;
        let [after, limit] =
            collect_query_errors([query.int("after", AFTER), query.int("limit", LIMIT)])?;
        let rows = app
            .events()
            .list(tx, project.project_id, after, limit)
            .await?;
        let next_cursor = rows.last().map_or(after, |(seq, _)| *seq);
        EventPage {
            events: rows.into_iter().map(|(_, e)| e).collect(),
            next_cursor,
        }
    });
    Ok(Json(page))
}

/// One SSE frame for an event.
pub fn sse_frame(seq: i64, event: &ActivityEvent) -> String {
    let data = pyjson::dumps(event).unwrap_or_else(|_| "{}".into());
    format!(
        "id: {seq}\nevent: {}\ndata: {data}\n\n",
        event.event_type.as_str()
    )
}

pub const KEEPALIVE: &str = ": keepalive\n\n";

struct StreamState {
    app: AppState,
    project_id: Uuid,
    cursor: i64,
    polls: u32,
    pending: VecDeque<Bytes>,
    done: bool,
}

async fn poll_once(s: &mut StreamState) -> AppResult<()> {
    let mut tx = s.app.store.begin().await?;
    let rows = s
        .app
        .events()
        .list(&mut *tx, s.project_id, s.cursor, STREAM_BATCH)
        .await;
    tx.rollback().await?;
    for (seq, ev) in rows? {
        s.cursor = seq;
        s.pending.push_back(Bytes::from(sse_frame(seq, &ev)));
    }
    s.pending
        .push_back(Bytes::from_static(KEEPALIVE.as_bytes()));
    Ok(())
}

async fn next_chunk(mut s: StreamState) -> Option<(Result<Bytes, Infallible>, StreamState)> {
    loop {
        if let Some(chunk) = s.pending.pop_front() {
            return Some((Ok(chunk), s));
        }
        if s.done || s.polls >= POLLS {
            return None;
        }
        if s.polls > 0 {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        s.polls += 1;
        if let Err(e) = poll_once(&mut s).await {
            logging::error(
                "app.api.events",
                "event stream poll failed; closing the stream",
                &[
                    ("project_id", Value::String(s.project_id.to_string())),
                    ("error", Value::String(e.to_string())),
                ],
            );
            s.done = true;
        }
    }
}

async fn stream_events(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
    query: QueryParams,
) -> AppResult<Response> {
    let project_id = params.uuid("project_id")?;
    crate::in_tx!(app, |tx| {
        app.authz().project_for_user(tx, project_id, user).await?;
    });
    let [after] = collect_query_errors([query.int("after", AFTER)])?;
    let state = StreamState {
        app: app.clone(),
        project_id,
        cursor: after,
        polls: 0,
        pending: VecDeque::new(),
        done: false,
    };
    let stream = futures_util::stream::unfold(state, next_chunk);
    Ok(stream_response(
        "text/event-stream; charset=utf-8",
        Body::from_stream(stream),
    ))
}
