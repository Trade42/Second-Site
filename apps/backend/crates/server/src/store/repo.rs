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

// ---- registry.py: property analysis tables -----------------------------------------------------
//
// Same indexed columns as `registry.py` and `db/models.py`, same `data` document
// (`model_dump(mode="json")`), so rows written by either backend load in the other.
// `feasibility_reports` has a table but no contract and no repository in Python, so none here.

use ss_domain::contracts::{
    CostScenario, InfrastructureAssessment, PermitRecord, PropertyDigitalProfile,
    PropertyRankingResult, RenovationWorkItem, SourceEvidence,
};

impl Document for PropertyDigitalProfile {
    const TABLE: &'static TableDef = &schema::PROPERTIES;
    fn id(&self) -> Uuid {
        self.property_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with(
                "normalized_address",
                self.property_identity.normalized_address.as_str(),
            )
            .with("parcel_id", self.property_identity.parcel_id.clone())
            .with(
                "data_origin",
                self.commercial_listing
                    .as_ref()
                    .map_or("DERIVED", |l| l.data_origin.as_str()),
            )
    }
}

impl ProjectScoped for PropertyDigitalProfile {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

impl Document for PermitRecord {
    const TABLE: &'static TableDef = &schema::PERMIT_RECORDS;
    fn id(&self) -> Uuid {
        self.permit_record_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("property_id", self.property_id)
            .with("permit_number", self.permit_number.as_str())
            .with("permit_category", self.permit_category.as_str())
            .with("match_status", self.match_status.as_str())
    }
}

impl Document for SourceEvidence {
    const TABLE: &'static TableDef = &schema::PROPERTY_OBSERVATIONS;
    fn id(&self) -> Uuid {
        self.evidence_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("property_id", self.property_id)
            .with("provider", self.provider.as_str())
            .with("observation_type", self.observation_type.as_str())
            .with(
                "evidence_classification",
                self.evidence_classification.as_str(),
            )
            .with("data_origin", self.data_origin.as_str())
    }
}

impl Document for InfrastructureAssessment {
    const TABLE: &'static TableDef = &schema::INFRASTRUCTURE_ASSESSMENTS;
    fn id(&self) -> Uuid {
        self.assessment_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("property_id", self.property_id)
            .with("requirement_id", self.requirement_id.as_str())
            .with("status", self.status.as_str())
    }
}

impl ProjectScoped for InfrastructureAssessment {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

impl Document for RenovationWorkItem {
    const TABLE: &'static TableDef = &schema::RENOVATION_WORK_ITEMS;
    fn id(&self) -> Uuid {
        self.work_item_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("property_id", self.property_id)
            .with("category", self.category.as_str())
    }
}

impl Document for CostScenario {
    const TABLE: &'static TableDef = &schema::COST_SCENARIOS;
    fn id(&self) -> Uuid {
        self.scenario_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("property_id", self.property_id)
            .with("long_term_cost", self.long_term_cost)
    }
}

impl ProjectScoped for CostScenario {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

impl Document for PropertyRankingResult {
    const TABLE: &'static TableDef = &schema::PROPERTY_RANKINGS;
    fn id(&self) -> Uuid {
        self.ranking_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("ranking_version", self.ranking_version)
    }
}

impl ProjectScoped for PropertyRankingResult {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

pub const PROPERTIES: Repo<PropertyDigitalProfile> = Repo::new();
pub const PERMITS: Repo<PermitRecord> = Repo::new();
pub const EVIDENCE: Repo<SourceEvidence> = Repo::new();
pub const ASSESSMENTS: Repo<InfrastructureAssessment> = Repo::new();
pub const WORK_ITEMS: Repo<RenovationWorkItem> = Repo::new();
pub const COST_SCENARIOS: Repo<CostScenario> = Repo::new();
pub const RANKINGS: Repo<PropertyRankingResult> = Repo::new();

/// Tables whose rows carry `superseded` (outputs are superseded, never deleted).
pub mod superseded {
    use super::*;

    /// `UPDATE <table> SET superseded = true WHERE property_id = :id` for the three analysis
    /// output tables, as `PropertyAnalysisWorkflow.evaluate` does before writing new ones.
    pub async fn supersede_property_outputs(tx: &mut dyn Tx, property_id: Uuid) -> AppResult<()> {
        for table in [
            &schema::INFRASTRUCTURE_ASSESSMENTS,
            &schema::RENOVATION_WORK_ITEMS,
            &schema::COST_SCENARIOS,
        ] {
            tx.update_where(
                table,
                &[Filter::Eq("property_id", SqlValue::Uuid(property_id))],
                Row::new().with("superseded", true),
            )
            .await?;
        }
        Ok(())
    }

    /// Live (not superseded) rows of one property, in `created_at` order.
    pub async fn live_for_property<C: Document>(
        tx: &mut dyn Tx,
        property_id: Uuid,
    ) -> AppResult<Vec<C>> {
        Repo::<C>::new()
            .list(
                tx,
                Query::new()
                    .eq("property_id", property_id)
                    .eq("superseded", false),
            )
            .await
    }
}

/// `property_sources` (raw provider references, kept apart from consumer-facing records) and
/// `property_identity_matches`: plain rows written by permit ingest.
pub mod sources {
    use super::*;

    /// One `property_sources` row (`db.models.PropertySource`).
    #[derive(Debug, Clone, PartialEq)]
    pub struct PropertySource {
        pub id: Uuid,
        pub property_id: Uuid,
        pub provider: String,
        pub dataset: Option<String>,
        pub source_record_id: String,
        pub data_origin: String,
        pub raw_payload: Value,
    }

    /// One `property_identity_matches` row (`db.models.PropertyIdentityMatch`).
    #[derive(Debug, Clone, PartialEq)]
    pub struct IdentityMatch {
        pub id: Uuid,
        pub property_id: Uuid,
        pub provider: String,
        pub provider_record_id: String,
        pub match_status: String,
        pub match_confidence: Option<f64>,
        pub explanation: String,
    }

    pub async fn add_source(tx: &mut dyn Tx, s: &PropertySource) -> AppResult<()> {
        let row = Row::new()
            .with("id", s.id)
            .with("property_id", s.property_id)
            .with("provider", s.provider.as_str())
            .with("dataset", s.dataset.clone())
            .with("source_record_id", s.source_record_id.as_str())
            .with("data_origin", s.data_origin.as_str())
            .with("raw_payload", s.raw_payload.clone());
        tx.insert(&schema::PROPERTY_SOURCES, row).await?;
        Ok(())
    }

    pub async fn add_match(tx: &mut dyn Tx, m: &IdentityMatch) -> AppResult<()> {
        let row = Row::new()
            .with("id", m.id)
            .with("property_id", m.property_id)
            .with("provider", m.provider.as_str())
            .with("provider_record_id", m.provider_record_id.as_str())
            .with("match_status", m.match_status.as_str())
            .with("match_confidence", m.match_confidence)
            .with("explanation", m.explanation.as_str());
        tx.insert(&schema::PROPERTY_IDENTITY_MATCHES, row).await?;
        Ok(())
    }

    fn source_from(row: &Row) -> Result<PropertySource, StoreError> {
        Ok(PropertySource {
            id: row.uuid("id")?,
            property_id: row.uuid("property_id")?,
            provider: row.text("provider")?,
            dataset: row.opt_text("dataset")?,
            source_record_id: row.text("source_record_id")?,
            data_origin: row.text("data_origin")?,
            raw_payload: row.json("raw_payload")?,
        })
    }

    fn match_from(row: &Row) -> Result<IdentityMatch, StoreError> {
        Ok(IdentityMatch {
            id: row.uuid("id")?,
            property_id: row.uuid("property_id")?,
            provider: row.text("provider")?,
            provider_record_id: row.text("provider_record_id")?,
            match_status: row.text("match_status")?,
            match_confidence: match row.get("match_confidence") {
                Some(SqlValue::Float(f)) => Some(*f),
                _ => None,
            },
            explanation: row.text("explanation")?,
        })
    }

    pub async fn sources_for(tx: &mut dyn Tx, property_id: Uuid) -> AppResult<Vec<PropertySource>> {
        let rows = tx
            .select(
                &schema::PROPERTY_SOURCES,
                &Query::new()
                    .eq("property_id", property_id)
                    .order_asc("created_at"),
            )
            .await?;
        Ok(rows.iter().map(source_from).collect::<Result<_, _>>()?)
    }

    pub async fn matches_for(tx: &mut dyn Tx, property_id: Uuid) -> AppResult<Vec<IdentityMatch>> {
        let rows = tx
            .select(
                &schema::PROPERTY_IDENTITY_MATCHES,
                &Query::new()
                    .eq("property_id", property_id)
                    .order_asc("created_at"),
            )
            .await?;
        Ok(rows.iter().map(match_from).collect::<Result<_, _>>()?)
    }
}

// ---- registry.py: design and procurement tables -------------------------------------------------
//
// `geometries`, `designs`, `boms` and `offers` of `registry.py`, with the same indexed columns and
// `data` documents, plus the plain `rooms` rows `DesignService._persist` writes. `bom_items` and
// `design_objects` have tables but no repository and no writer in Python, so none here.

use ss_domain::contracts::{BillOfMaterials, DesignVersion, SiteGeometry, SupplierOffer};

impl Document for SiteGeometry {
    const TABLE: &'static TableDef = &schema::SITE_GEOMETRIES;
    fn id(&self) -> Uuid {
        self.site_geometry_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("property_id", self.property_id)
            .with("scale_verified", self.scale_verified)
    }
}

impl Document for DesignVersion {
    const TABLE: &'static TableDef = &schema::DESIGN_VERSIONS;
    fn id(&self) -> Uuid {
        self.design_version_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("property_id", self.property_id)
            .with("parent_version_id", self.parent_version_id)
            .with("version_number", self.version_number)
            .with("approval_status", self.approval_status.as_str())
            .with("valid", self.validation_report.valid)
    }
}

impl ProjectScoped for DesignVersion {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

impl Document for BillOfMaterials {
    const TABLE: &'static TableDef = &schema::BILLS_OF_MATERIALS;
    fn id(&self) -> Uuid {
        self.bom_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("design_version_id", self.design_version_id)
            .with("status", self.status.as_str())
    }
}

impl ProjectScoped for BillOfMaterials {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

impl Document for SupplierOffer {
    const TABLE: &'static TableDef = &schema::SUPPLIER_OFFERS;
    fn id(&self) -> Uuid {
        self.offer_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("bom_item_id", self.bom_item_id)
            .with("supplier", self.supplier.as_str())
    }
}

pub const GEOMETRIES: Repo<SiteGeometry> = Repo::new();
pub const DESIGNS: Repo<DesignVersion> = Repo::new();
pub const BOMS: Repo<BillOfMaterials> = Repo::new();
pub const OFFERS: Repo<SupplierOffer> = Repo::new();

/// `design_versions` queries that are not plain repository calls.
pub mod designs {
    use super::*;

    /// `select(func.max(DesignVersionRow.version_number)).where(property_id == :id)`: `None`
    /// when the property has no design version.
    pub async fn max_version_number(tx: &mut dyn Tx, property_id: Uuid) -> AppResult<Option<i64>> {
        let rows = tx
            .select(
                &schema::DESIGN_VERSIONS,
                &Query::new()
                    .eq("property_id", property_id)
                    .order_desc("version_number")
                    .limit(1),
            )
            .await?;
        Ok(rows
            .first()
            .map(|r| r.int("version_number"))
            .transpose()?)
    }

    /// The unique constraint on `(property_id, version_number)`.
    pub const VERSION_CONSTRAINT: &str = "uq_design_versions_property_id";

    /// `designs.add`, except that a clash on [`VERSION_CONSTRAINT`] is `Ok(false)` instead of an
    /// error (the caller renumbers). Run it inside a savepoint: a failed insert aborts the
    /// transaction until the savepoint is rolled back.
    pub async fn add_numbered(tx: &mut dyn Tx, dv: &DesignVersion) -> AppResult<bool> {
        let row = DESIGNS.row_for(dv)?;
        match tx.insert(DesignVersion::TABLE, row).await {
            Ok(_) => Ok(true),
            Err(e) if e.is_unique_violation(VERSION_CONSTRAINT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// `rooms` rows (`db.models.Room`): one per room polygon of a design version.
pub mod rooms {
    use super::*;

    /// One `rooms` row.
    #[derive(Debug, Clone, PartialEq)]
    pub struct RoomRow {
        pub id: Uuid,
        pub design_version_id: Uuid,
        /// `RoomPolygon.room_id`.
        pub room_key: String,
        pub room_type: String,
        pub area_sqm: f64,
        /// `[[x, y], ...]` (`[list(p) for p in r.polygon]`).
        pub polygon: Value,
    }

    pub async fn insert(tx: &mut dyn Tx, r: &RoomRow) -> AppResult<()> {
        let row = Row::new()
            .with("id", r.id)
            .with("design_version_id", r.design_version_id)
            .with("room_key", r.room_key.as_str())
            .with("room_type", r.room_type.as_str())
            .with("area_sqm", r.area_sqm)
            .with("polygon", r.polygon.clone());
        tx.insert(&schema::ROOMS, row).await?;
        Ok(())
    }

    fn from_row(row: &Row) -> Result<RoomRow, StoreError> {
        Ok(RoomRow {
            id: row.uuid("id")?,
            design_version_id: row.uuid("design_version_id")?,
            room_key: row.text("room_key")?,
            room_type: row.text("room_type")?,
            area_sqm: match row.get("area_sqm") {
                Some(SqlValue::Float(f)) => *f,
                Some(SqlValue::Int(i)) => *i as f64,
                _ => return Err(StoreError::Decode("rooms.area_sqm is not a number".into())),
            },
            polygon: row.json("polygon")?,
        })
    }

    /// The rooms of one design version, in `created_at` order.
    pub async fn for_design_version(
        tx: &mut dyn Tx,
        design_version_id: Uuid,
    ) -> AppResult<Vec<RoomRow>> {
        let rows = tx
            .select(
                &schema::ROOMS,
                &Query::new()
                    .eq("design_version_id", design_version_id)
                    .order_asc("created_at"),
            )
            .await?;
        Ok(rows.iter().map(from_row).collect::<Result<_, _>>()?)
    }
}
