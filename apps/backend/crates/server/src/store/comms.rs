//! Repositories of the call flow: ports of `registry.py`'s `communications`, `calls` and
//! `handoffs`, the read-only half of `owner_channels` (the Python Photon code owns that table and
//! writes it; Rust only reads the owner's phone from it, as `_owner_phone` does), and
//! `domains/communications/inbound.py::record_once` for `processed_webhook_events`.
//!
//! Same `data` documents (`model_dump(mode="json")`) and indexed columns as `registry.py`, so rows
//! written by either backend load in the other.

use serde_json::{json, Value};
use ss_domain::contracts::{
    CallSession, CommunicationSession, OwnerChannel, OwnerHandoffRequest,
};
use ss_domain::integrations::webhook_signatures::VerifiedWebhook;
use uuid::Uuid;

use super::repo::{Document, ProjectScoped, Repo};
use super::schema::{self, TableDef};
use super::{Query, Row, Tx};
use crate::error::AppResult;

impl Document for CommunicationSession {
    const TABLE: &'static TableDef = &schema::COMMUNICATIONS;
    fn id(&self) -> Uuid {
        self.communication_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("channel", self.channel.as_str())
            .with("state", self.state.as_str())
    }
}

impl ProjectScoped for CommunicationSession {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

impl Document for CallSession {
    const TABLE: &'static TableDef = &schema::CALL_SESSIONS;
    fn id(&self) -> Uuid {
        self.call_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("provider_call_id", self.provider_call_id.clone())
            .with("state", self.state.as_str())
    }
}

impl ProjectScoped for CallSession {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

impl Document for OwnerHandoffRequest {
    const TABLE: &'static TableDef = &schema::OWNER_HANDOFF_REQUESTS;
    fn id(&self) -> Uuid {
        self.handoff_request_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("call_id", self.call_id)
            .with("status", self.status.as_str())
    }
}

impl ProjectScoped for OwnerHandoffRequest {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

/// Read-only here: the Python Photon code writes `owner_channels`.
impl Document for OwnerChannel {
    const TABLE: &'static TableDef = &schema::OWNER_CHANNELS;
    fn id(&self) -> Uuid {
        self.owner_channel_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("owner_id", self.owner_id)
            .with("platform", self.platform.as_str())
            .with("handle", self.handle.as_str())
            .with("status", self.status.as_str())
    }
}

pub const COMMUNICATIONS: Repo<CommunicationSession> = Repo::new();
pub const CALLS: Repo<CallSession> = Repo::new();
pub const HANDOFFS: Repo<OwnerHandoffRequest> = Repo::new();
/// Read only (`channel_for_owner`); never `add` or `upsert` through it.
pub const OWNER_CHANNELS: Repo<OwnerChannel> = Repo::new();

/// The call whose carrier leg is `provider_call_id` (`select(CallSessionRow).where(
/// provider_call_id == leg)`; the column is unique).
pub async fn call_by_provider_id(
    tx: &mut dyn Tx,
    provider_call_id: &str,
) -> AppResult<Option<CallSession>> {
    Ok(CALLS
        .list(
            tx,
            Query::new()
                .eq("provider_call_id", provider_call_id)
                .limit(1),
        )
        .await?
        .into_iter()
        .next())
}

/// `channel_for_owner`: the owner's most recently created channel (`rows[-1]` in `created_at`
/// order), or `None`.
pub async fn channel_for_owner(tx: &mut dyn Tx, owner_id: Uuid) -> AppResult<Option<OwnerChannel>> {
    Ok(OWNER_CHANNELS
        .list(tx, Query::new().eq("owner_id", owner_id))
        .await?
        .pop())
}

pub mod webhooks {
    //! `processed_webhook_events` (`inbound.record_once`): the dedupe table that Photon's webhook
    //! and poller share; this crate writes only provider `telnyx`.

    use super::*;

    pub const UNIQUE_CONSTRAINT: &str = "uq_processed_webhook_events_provider";

    /// `record_once(db, verified, payload)`: `true` if this provider event is new. The row stores
    /// `verified = (mode == "live")` and `payload = {"mode": mode, "event": payload}`; the unique
    /// `(provider, provider_event_id)` constraint decides concurrent deliveries (the loser sees a
    /// unique violation inside its savepoint and answers `false`).
    pub async fn record_once(
        tx: &mut dyn Tx,
        verified: &VerifiedWebhook,
        payload: &Value,
    ) -> AppResult<bool> {
        let existing = tx
            .select(
                &schema::PROCESSED_WEBHOOK_EVENTS,
                &Query::new()
                    .eq("provider", verified.provider.as_str())
                    .eq("provider_event_id", verified.event_id.as_str())
                    .limit(1),
            )
            .await?;
        if !existing.is_empty() {
            return Ok(false);
        }
        let row = Row::new()
            .with("id", Uuid::new_v4())
            .with("provider", verified.provider.as_str())
            .with("provider_event_id", verified.event_id.as_str())
            .with("verified", verified.is_live_verified())
            .with(
                "payload",
                json!({"mode": verified.mode.as_str(), "event": payload}),
            );
        let sp = tx.savepoint().await?;
        match tx.insert(&schema::PROCESSED_WEBHOOK_EVENTS, row).await {
            Ok(_) => {
                tx.release_savepoint(sp).await?;
                Ok(true)
            }
            Err(e) if e.is_unique_violation(UNIQUE_CONSTRAINT) => {
                tx.rollback_to_savepoint(sp).await?;
                Ok(false)
            }
            Err(e) => {
                tx.rollback_to_savepoint(sp).await?;
                Err(e.into())
            }
        }
    }

    /// The stored rows for one provider (tests and audits).
    pub async fn rows(tx: &mut dyn Tx, provider: &str) -> AppResult<Vec<Row>> {
        Ok(tx
            .select(
                &schema::PROCESSED_WEBHOOK_EVENTS,
                &Query::new().eq("provider", provider).order_asc("created_at"),
            )
            .await?)
    }
}
