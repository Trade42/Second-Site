//! Typed repositories: ports of `repositories/records.py` (`RecordRepository`) and
//! `repositories/registry.py`, plus the non-document rows (`users`, `activity_events`).
//!
//! A contract stored in a `DocumentMixin` table implements [`Document`]: its table, its id and
//! the indexed columns copied out of it. [`Repo<C>`] then offers `add`, `upsert`,
//! `update_versioned`, `get`, `require`, `list` exactly like `RecordRepository`. Later commissions
//! add one `impl Document` per contract and one `pub const` repo; the store does not change.

use std::marker::PhantomData;

use serde_json::Value;
use ss_core::{BusinessExpansionProfile, Contract, ProjectContext};
use uuid::Uuid;

use super::schema::{self, TableDef};
use super::{Filter, Query, Row, SqlValue, StoreError, Tx, UpdateOutcome};
use crate::contracts::{ActivityEvent, ApprovalRequest, TaskRecord};
use crate::error::{AppError, AppResult};

/// A contract stored as a row document (`data`) plus indexed columns.
pub trait Document: Contract + Clone + Send + Sync + 'static {
    const TABLE: &'static TableDef;
    /// `id_field` of `RecordRepository`.
    fn id(&self) -> Uuid;
    /// The indexed columns (`columns` lambda in `registry.py`).
    fn columns(&self) -> Row;
}

/// Records that belong to a project (for the "belongs to this project or 404" checks).
pub trait ProjectScoped {
    fn project_id(&self) -> Uuid;
}

fn to_data<C: Document>(record: &C) -> AppResult<Value> {
    serde_json::to_value(record)
        .map_err(|e| AppError::internal(format!("serialize {}: {e}", C::NAME)))
}

fn from_row<C: Document>(row: &Row) -> AppResult<C> {
    let data = row.json("data")?;
    Ok(C::from_json_value(data)?)
}

/// `RecordRepository` for one contract.
pub struct Repo<C>(PhantomData<fn() -> C>);

impl<C> Repo<C> {
    pub const fn new() -> Self {
        Repo(PhantomData)
    }
}

impl<C> Default for Repo<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Document> Repo<C> {
    fn row_for(&self, record: &C) -> AppResult<Row> {
        let mut row = record.columns();
        row.set("id", record.id());
        row.set("data", to_data(record)?);
        Ok(row)
    }

    /// `add`: insert a new row (`record_version` 1).
    pub async fn add(&self, tx: &mut dyn Tx, record: &C) -> AppResult<()> {
        let row = self.row_for(record)?;
        tx.insert(C::TABLE, row).await?;
        Ok(())
    }

    pub async fn add_all(&self, tx: &mut dyn Tx, records: &[C]) -> AppResult<()> {
        for r in records {
            self.add(tx, r).await?;
        }
        Ok(())
    }

    /// `upsert`: insert, or replace `data` and the columns and increment `record_version`.
    pub async fn upsert(&self, tx: &mut dyn Tx, record: &C) -> AppResult<()> {
        let id = record.id();
        if tx.get(C::TABLE, id).await?.is_none() {
            return self.add(tx, record).await;
        }
        let mut set = self.row_for(record)?;
        set.set("id", id);
        match tx.update(C::TABLE, id, set, None).await? {
            UpdateOutcome::Updated(_) => Ok(()),
            UpdateOutcome::NotFound => self.add(tx, record).await,
            UpdateOutcome::VersionMismatch { .. } => {
                Err(AppError::internal("unexpected version check"))
            }
        }
    }

    /// `update_versioned`: optimistic concurrency. Returns the new `record_version`.
    pub async fn update_versioned(
        &self,
        tx: &mut dyn Tx,
        record: &C,
        expected_version: i64,
    ) -> AppResult<i64> {
        let set = self.row_for(record)?;
        match tx
            .update(C::TABLE, record.id(), set, Some(expected_version))
            .await?
        {
            UpdateOutcome::Updated(row) => Ok(row.int("record_version")?),
            UpdateOutcome::NotFound => Err(AppError::not_found(format!("{} not found", C::NAME))),
            UpdateOutcome::VersionMismatch { actual } => {
                Err(AppError::conflict("stale record version")
                    .code("STALE_VERSION")
                    .details(serde_json::json!({"expected": expected_version, "actual": actual})))
            }
        }
    }

    pub async fn get(&self, tx: &mut dyn Tx, id: Uuid) -> AppResult<Option<C>> {
        match tx.get(C::TABLE, id).await? {
            Some(row) => Ok(Some(from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// The record and its row `record_version`.
    pub async fn get_versioned(&self, tx: &mut dyn Tx, id: Uuid) -> AppResult<Option<(C, i64)>> {
        match tx.get(C::TABLE, id).await? {
            Some(row) => Ok(Some((from_row(&row)?, row.int("record_version")?))),
            None => Ok(None),
        }
    }

    /// Lock the row for the rest of the transaction (`SELECT ... FOR UPDATE`).
    pub async fn get_for_update(&self, tx: &mut dyn Tx, id: Uuid) -> AppResult<Option<(C, i64)>> {
        match tx.get_for_update(C::TABLE, id).await? {
            Some(row) => Ok(Some((from_row(&row)?, row.int("record_version")?))),
            None => Ok(None),
        }
    }

    /// `require`: 404 `"<Contract> <id> not found"`.
    pub async fn require(&self, tx: &mut dyn Tx, id: Uuid) -> AppResult<C> {
        self.get(tx, id)
            .await?
            .ok_or_else(|| AppError::not_found(format!("{} {id} not found", C::NAME)))
    }

    /// `list(**filters)`: ordered by `created_at` unless the query orders otherwise.
    pub async fn list(&self, tx: &mut dyn Tx, query: Query) -> AppResult<Vec<C>> {
        let mut q = query;
        if q.order_by.is_empty() {
            q = q.order_asc("created_at");
        }
        let rows = tx.select(C::TABLE, &q).await?;
        rows.iter().map(from_row).collect()
    }

    /// Equality filters only (the Python call shape `list(db, project_id=...)`).
    pub async fn list_by(
        &self,
        tx: &mut dyn Tx,
        filters: &[(&'static str, SqlValue)],
    ) -> AppResult<Vec<C>> {
        let mut q = Query::new();
        for (c, v) in filters {
            q.filters.push(Filter::Eq(c, v.clone()));
        }
        self.list(tx, q).await
    }
}

impl<C: Document + ProjectScoped> Repo<C> {
    /// The record if it exists and belongs to `project_id`; `None` otherwise (callers turn that
    /// into a 404 so other projects' ids are not discoverable).
    pub async fn get_in_project(
        &self,
        tx: &mut dyn Tx,
        id: Uuid,
        project_id: Uuid,
    ) -> AppResult<Option<C>> {
        Ok(self
            .get(tx, id)
            .await?
            .filter(|r| r.project_id() == project_id))
    }

    /// 404 `not_found_message` unless the record exists and belongs to `project_id`.
    pub async fn require_in_project(
        &self,
        tx: &mut dyn Tx,
        id: Uuid,
        project_id: Uuid,
        not_found_message: &str,
    ) -> AppResult<C> {
        self.get_in_project(tx, id, project_id)
            .await?
            .ok_or_else(|| AppError::not_found(not_found_message))
    }
}

// ---- registry.py ------------------------------------------------------------------------------

impl Document for ProjectContext {
    const TABLE: &'static TableDef = &schema::PROJECTS;
    fn id(&self) -> Uuid {
        self.project_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("owner_id", self.owner_id)
            .with("status", self.project_status.as_str())
    }
}

impl Document for BusinessExpansionProfile {
    const TABLE: &'static TableDef = &schema::BUSINESS_REQUIREMENTS;
    fn id(&self) -> Uuid {
        self.profile_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("confirmation_status", self.confirmation_status.as_str())
    }
}

impl ProjectScoped for BusinessExpansionProfile {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

impl Document for TaskRecord {
    const TABLE: &'static TableDef = &schema::TASKS;
    fn id(&self) -> Uuid {
        self.task_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("task_type", self.task_type.as_str())
            .with("status", self.status.as_str())
    }
}

impl ProjectScoped for TaskRecord {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

impl Document for ApprovalRequest {
    const TABLE: &'static TableDef = &schema::APPROVALS;
    fn id(&self) -> Uuid {
        self.approval_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("status", self.approval_status.as_str())
            .with("action_type", self.action_type.as_str())
            .with("idempotency_key", self.idempotency_key.as_str())
    }
}

impl ProjectScoped for ApprovalRequest {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

pub const PROJECTS: Repo<ProjectContext> = Repo::new();
pub const PROFILES: Repo<BusinessExpansionProfile> = Repo::new();
pub const TASKS: Repo<TaskRecord> = Repo::new();
pub const APPROVALS: Repo<ApprovalRequest> = Repo::new();

// ---- users ------------------------------------------------------------------------------------

/// A `users` row (`db.models.User`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: Uuid,
    pub display_name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
}

pub mod users {
    use super::*;

    fn from_row(row: &Row) -> Result<User, StoreError> {
        Ok(User {
            id: row.uuid("id")?,
            display_name: row.text("display_name")?,
            email: row.opt_text("email")?,
            phone: row.opt_text("phone")?,
        })
    }

    pub async fn get(tx: &mut dyn Tx, id: Uuid) -> AppResult<Option<User>> {
        Ok(tx
            .get(&schema::USERS, id)
            .await?
            .as_ref()
            .map(from_row)
            .transpose()?)
    }

    pub async fn find_by_email(tx: &mut dyn Tx, email: &str) -> AppResult<Option<User>> {
        let rows = tx
            .select(&schema::USERS, &Query::new().eq("email", email).limit(1))
            .await?;
        Ok(rows.first().map(from_row).transpose()?)
    }

    pub async fn insert(tx: &mut dyn Tx, user: &User) -> Result<(), StoreError> {
        let row = Row::new()
            .with("id", user.id)
            .with("display_name", user.display_name.as_str())
            .with("email", user.email.clone())
            .with("phone", user.phone.clone());
        tx.insert(&schema::USERS, row).await.map(|_| ())
    }
}

// ---- activity_events --------------------------------------------------------------------------

/// The indexed columns of an `activity_events` row.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub id: Uuid,
    pub seq: i64,
    pub causation_id: Option<Uuid>,
    pub envelope: Value,
}

pub mod events {
    use super::*;

    pub const IDEMPOTENCY_CONSTRAINT: &str = "uq_activity_events_idempotency_key";

    fn from_row(row: &Row) -> Result<EventRow, StoreError> {
        Ok(EventRow {
            id: row.uuid("id")?,
            seq: row.int("seq")?,
            causation_id: row.opt_uuid("causation_id")?,
            envelope: row.json("envelope")?,
        })
    }

    pub fn parse(row: &EventRow) -> AppResult<ActivityEvent> {
        Ok(ActivityEvent::from_json_value(row.envelope.clone())?)
    }

    /// Insert; returns the assigned `seq`.
    pub async fn insert(tx: &mut dyn Tx, event: &ActivityEvent) -> Result<i64, StoreError> {
        let envelope =
            serde_json::to_value(event).map_err(|e| StoreError::Decode(e.to_string()))?;
        let row = Row::new()
            .with("id", event.event_id)
            .with("project_id", event.project_id)
            .with("event_type", event.event_type.as_str())
            .with("correlation_id", event.correlation_id)
            .with("causation_id", event.causation_id)
            .with("idempotency_key", event.idempotency_key.as_str())
            .with("envelope", envelope);
        let stored = tx.insert(&schema::ACTIVITY_EVENTS, row).await?;
        stored.int("seq")
    }

    pub async fn exists(tx: &mut dyn Tx, idempotency_key: &str) -> AppResult<bool> {
        let rows = tx
            .select(
                &schema::ACTIVITY_EVENTS,
                &Query::new().eq("idempotency_key", idempotency_key).limit(1),
            )
            .await?;
        Ok(!rows.is_empty())
    }

    pub async fn get_row(tx: &mut dyn Tx, id: Uuid) -> AppResult<Option<EventRow>> {
        Ok(tx
            .get(&schema::ACTIVITY_EVENTS, id)
            .await?
            .as_ref()
            .map(from_row)
            .transpose()?)
    }

    /// Rows matching `query` (ordered by `seq` unless the query orders otherwise).
    pub async fn select(tx: &mut dyn Tx, query: Query) -> AppResult<Vec<EventRow>> {
        let mut q = query;
        if q.order_by.is_empty() {
            q = q.order_asc("seq");
        }
        let rows = tx.select(&schema::ACTIVITY_EVENTS, &q).await?;
        Ok(rows.iter().map(from_row).collect::<Result<Vec<_>, _>>()?)
    }
}
