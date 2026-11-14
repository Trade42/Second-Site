//! Port of `apps/api/app/services/events.py`: persist `ActivityEvent`s idempotently and dispatch
//! them to subscribers.
//!
//! Loop safety (as Python):
//! - `idempotency_key` is unique: re-emitting the same logical event is a no-op (`Ok(None)`).
//! - Handlers emit their result with key `"<handler>:<cause_event_id>"`, so redelivery never
//!   re-runs a handler.
//! - Causation chains deeper than [`MAX_CAUSATION_DEPTH`] are refused.
//!
//! Dispatch is inline, in the producer's transaction. A subscriber failure is logged and
//! propagated, which rolls back the producer's event too: Python's observable behaviour, kept on
//! purpose (its comment claims the opposite; a durable retry design is a later commission).
//! The insert runs inside a savepoint so that a concurrent emit of the same key (PostgreSQL unique
//! violation) is also reported as "already emitted" instead of failing the request.

use serde_json::Value;
use uuid::Uuid;

use crate::app::App;
use crate::contracts::{ActivityEvent, ActorType, EventType, JsonMap, NewEvent};
use crate::error::{AppError, AppResult};
use crate::logging;
use crate::store::repo::events as rows;
use crate::store::{Query, Tx};

pub const MAX_CAUSATION_DEPTH: usize = 12;

/// Arguments of `EventService.emit`.
#[derive(Debug, Clone)]
pub struct Emit<'a> {
    pub event_type: EventType,
    pub project_id: Uuid,
    pub payload: Value,
    pub actor_type: ActorType,
    pub actor_id: String,
    pub idempotency_key: String,
    pub correlation_id: Option<Uuid>,
    pub causation: Option<&'a ActivityEvent>,
    pub dispatch: bool,
}

impl<'a> Emit<'a> {
    pub fn new(
        event_type: EventType,
        project_id: Uuid,
        actor_type: ActorType,
        actor_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        payload: Value,
    ) -> Self {
        Emit {
            event_type,
            project_id,
            payload,
            actor_type,
            actor_id: actor_id.into(),
            idempotency_key: idempotency_key.into(),
            correlation_id: None,
            causation: None,
            dispatch: true,
        }
    }

    pub fn correlation(mut self, id: Uuid) -> Self {
        self.correlation_id = Some(id);
        self
    }

    pub fn caused_by(mut self, cause: &'a ActivityEvent) -> Self {
        self.causation = Some(cause);
        self
    }

    pub fn without_dispatch(mut self) -> Self {
        self.dispatch = false;
        self
    }
}

pub struct EventService<'a> {
    app: &'a App,
}

impl<'a> EventService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    pub async fn exists(&self, tx: &mut dyn Tx, idempotency_key: &str) -> AppResult<bool> {
        rows::exists(tx, idempotency_key).await
    }

    /// Length of the causation chain ending at `causation_id` (capped just above the limit).
    pub async fn depth(&self, tx: &mut dyn Tx, causation_id: Option<Uuid>) -> AppResult<usize> {
        let mut d = 0;
        let mut cur = causation_id;
        while let Some(id) = cur {
            if d > MAX_CAUSATION_DEPTH {
                break;
            }
            match rows::get_row(tx, id).await? {
                None => break,
                Some(row) => {
                    cur = row.causation_id;
                    d += 1;
                }
            }
        }
        Ok(d)
    }

    /// `emit(...)`: the new event, or `None` when the idempotency key was already used.
    pub async fn emit(&self, tx: &mut dyn Tx, e: Emit<'_>) -> AppResult<Option<ActivityEvent>> {
        if self.exists(tx, &e.idempotency_key).await? {
            return Ok(None);
        }
        if let Some(cause) = e.causation {
            if self.depth(tx, Some(cause.event_id)).await? >= MAX_CAUSATION_DEPTH {
                // Python raises RuntimeError (a 500).
                return Err(AppError::internal_kind(
                    "RuntimeError",
                    format!("causation chain too deep for {}", e.event_type),
                ));
            }
        }
        let payload: JsonMap = match e.payload {
            Value::Object(m) => m,
            _ => {
                return Err(ss_core::ValidationError::value_error(
                    "ActivityEvent",
                    "payload must be an object",
                )
                .into())
            }
        };
        let correlation_id = e
            .correlation_id
            .or_else(|| e.causation.map(|c| c.correlation_id))
            .unwrap_or_else(Uuid::new_v4);
        let event = ActivityEvent::new(NewEvent {
            event_type: e.event_type,
            project_id: e.project_id,
            correlation_id,
            causation_id: e.causation.map(|c| c.event_id),
            idempotency_key: e.idempotency_key,
            actor_type: e.actor_type,
            actor_id: e.actor_id,
            payload,
        })?;
        let sp = tx.savepoint().await?;
        match rows::insert(tx, &event).await {
            Ok(_) => tx.release_savepoint(sp).await?,
            Err(err) => {
                tx.rollback_to_savepoint(sp).await?;
                if err.is_unique_violation(rows::IDEMPOTENCY_CONSTRAINT) {
                    return Ok(None);
                }
                return Err(err.into());
            }
        }
        if e.dispatch {
            self.dispatch(tx, &event).await?;
        }
        Ok(Some(event))
    }

    /// Run every subscriber of the event's type, inline. A failure is logged and propagated.
    pub async fn dispatch(&self, tx: &mut dyn Tx, event: &ActivityEvent) -> AppResult<()> {
        for (name, handler) in self.app.subscribers(event.event_type) {
            if let Err(err) = handler.handle(self.app, tx, event).await {
                logging::error(
                    "app.services.events",
                    "event handler failed",
                    &[
                        ("handler", Value::String(name.clone())),
                        ("event", Value::String(event.event_id.to_string())),
                        ("error", Value::String(err.to_string())),
                    ],
                );
                return Err(err);
            }
        }
        Ok(())
    }

    /// `list(project_id, after_seq, limit)`: `(seq, event)` in `seq` order.
    pub async fn list(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        after_seq: i64,
        limit: i64,
    ) -> AppResult<Vec<(i64, ActivityEvent)>> {
        let found = rows::select(
            tx,
            Query::new()
                .eq("project_id", project_id)
                .gt("seq", after_seq)
                .order_asc("seq")
                .limit(limit),
        )
        .await?;
        found.iter().map(|r| Ok((r.seq, rows::parse(r)?))).collect()
    }

    pub async fn get(&self, tx: &mut dyn Tx, event_id: Uuid) -> AppResult<Option<ActivityEvent>> {
        match rows::get_row(tx, event_id).await? {
            Some(r) => Ok(Some(rows::parse(&r)?)),
            None => Ok(None),
        }
    }

    /// Events with this `correlation_id`, in `seq` order (the cross-domain chain of one cause).
    pub async fn by_correlation(
        &self,
        tx: &mut dyn Tx,
        correlation_id: Uuid,
    ) -> AppResult<Vec<(i64, ActivityEvent)>> {
        let found = rows::select(
            tx,
            Query::new()
                .eq("correlation_id", correlation_id)
                .order_asc("seq"),
        )
        .await?;
        found.iter().map(|r| Ok((r.seq, rows::parse(r)?))).collect()
    }

    /// `already_handled(cause, result_type)` (the redelivery guard in
    /// `workflows/property_analysis.py`): a result event with `causation_id = cause.event_id`.
    pub async fn already_handled(
        &self,
        tx: &mut dyn Tx,
        cause: &ActivityEvent,
        result_type: EventType,
    ) -> AppResult<bool> {
        let found = rows::select(
            tx,
            Query::new()
                .eq("causation_id", cause.event_id)
                .eq("event_type", result_type.as_str())
                .limit(1),
        )
        .await?;
        Ok(!found.is_empty())
    }
}
