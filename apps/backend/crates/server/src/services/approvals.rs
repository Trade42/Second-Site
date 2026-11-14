//! Port of `apps/api/app/services/approvals.py`: the enforcement point for external and
//! consequential actions.
//!
//! [`ApprovalService::require_approved`] is called at the action boundary, inside the
//! provider-call path, not in a prompt. An approval authorizes only the exact payload hash the
//! owner saw, once.
//!
//! Fixed Python defects:
//! - `require_approved` is bound to the project: an approval from another project cannot
//!   authorize an action (it is reported exactly like a missing approval).
//! - Idempotency lookups in `request` are scoped to the project: a key used by another project is
//!   a 409 instead of returning that project's approval.
//! - The approval row is locked (`SELECT ... FOR UPDATE`) in `decide` and `require_approved`, and
//!   `mark_executed` is a version-checked update of the row `require_approved` read, so two
//!   concurrent executions cannot both pass. Call `require_approved`, perform the action and call
//!   `mark_executed` in the same transaction.

use chrono::{Duration, Utc};
use serde_json::json;
use ss_core::UtcDateTime;
use uuid::Uuid;

use crate::app::App;
use crate::contracts::{
    payload_hash, ActionType, ActorType, ApprovalRequest, ApprovalStatus, EventType, JsonMap,
    NewApproval, RiskCategory,
};
use crate::error::{AppError, AppResult};
use crate::services::events::Emit;
use crate::store::repo::APPROVALS;
use crate::store::{Query, Tx};

pub const IDEMPOTENCY_CONSTRAINT: &str = "uq_approvals_idempotency_key";

/// `RISK`.
pub fn risk_for(action: ActionType) -> RiskCategory {
    match action {
        ActionType::SendMessage | ActionType::PlaceCall | ActionType::ContactSupplier => {
            RiskCategory::ExternalCommunication
        }
        ActionType::Purchase | ActionType::FinancialCommitment | ActionType::ApproveBom => {
            RiskCategory::Consequential
        }
        ActionType::ApproveLayout => RiskCategory::Internal,
    }
}

/// Arguments of `ApprovalService.request`.
#[derive(Debug, Clone)]
pub struct ApprovalSpec {
    pub project_id: Uuid,
    pub owner_id: Uuid,
    pub agent: String,
    pub action_type: ActionType,
    pub payload: JsonMap,
    pub summary: String,
    pub idempotency_key: String,
    /// Default 24 hours.
    pub ttl: Duration,
}

impl ApprovalSpec {
    pub fn new(
        project_id: Uuid,
        owner_id: Uuid,
        agent: impl Into<String>,
        action_type: ActionType,
        payload: JsonMap,
        summary: impl Into<String>,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            project_id,
            owner_id,
            agent: agent.into(),
            action_type,
            payload,
            summary: summary.into(),
            idempotency_key: idempotency_key.into(),
            ttl: Duration::hours(24),
        }
    }

    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }
}

/// Proof that an approval authorizes an action, returned by `require_approved`. Spend it with
/// `mark_executed` in the same transaction.
#[derive(Debug, Clone)]
pub struct ApprovedAction {
    pub approval: ApprovalRequest,
    record_version: i64,
}

pub struct ApprovalService<'a> {
    app: &'a App,
}

impl<'a> ApprovalService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    async fn by_key(&self, tx: &mut dyn Tx, key: &str) -> AppResult<Option<ApprovalRequest>> {
        Ok(APPROVALS
            .list(tx, Query::new().eq("idempotency_key", key).limit(1))
            .await?
            .into_iter()
            .next())
    }

    fn existing_for(
        &self,
        project_id: Uuid,
        existing: ApprovalRequest,
    ) -> AppResult<ApprovalRequest> {
        if existing.project_id == project_id {
            Ok(existing)
        } else {
            Err(
                AppError::conflict("idempotency key already used by another project")
                    .code("IDEMPOTENCY_KEY_CONFLICT"),
            )
        }
    }

    /// `request(...)`: idempotent per `idempotency_key` within the project.
    pub async fn request(&self, tx: &mut dyn Tx, spec: ApprovalSpec) -> AppResult<ApprovalRequest> {
        if let Some(existing) = self.by_key(tx, &spec.idempotency_key).await? {
            return self.existing_for(spec.project_id, existing);
        }
        let expires_at = UtcDateTime::from(Utc::now() + spec.ttl);
        let appr = ApprovalRequest::new(NewApproval {
            project_id: spec.project_id,
            requested_by_agent: spec.agent.clone(),
            action_type: spec.action_type,
            action_payload: spec.payload,
            risk_category: risk_for(spec.action_type),
            summary: spec.summary,
            expires_at,
            authorized_owner_id: spec.owner_id,
            idempotency_key: spec.idempotency_key.clone(),
        });
        let sp = tx.savepoint().await?;
        match APPROVALS.add(tx, &appr).await {
            Ok(()) => tx.release_savepoint(sp).await?,
            Err(err) => {
                tx.rollback_to_savepoint(sp).await?;
                // A concurrent request with the same key won the race.
                if let Some(existing) = self.by_key(tx, &spec.idempotency_key).await? {
                    return self.existing_for(spec.project_id, existing);
                }
                return Err(err);
            }
        }
        self.app
            .events()
            .emit(
                tx,
                Emit::new(
                    EventType::ApprovalRequested,
                    spec.project_id,
                    ActorType::Agent,
                    spec.agent,
                    format!("approval:{}:requested", appr.approval_id),
                    json!({
                        "approval_id": appr.approval_id.to_string(),
                        "action_type": spec.action_type.as_str(),
                        "approval_status": appr.approval_status.as_str(),
                    }),
                ),
            )
            .await?;
        Ok(appr)
    }

    /// `_expire_if_needed`: PENDING/APPROVED past `expires_at` become EXPIRED (persisted).
    async fn expire_if_needed(
        &self,
        tx: &mut dyn Tx,
        appr: ApprovalRequest,
    ) -> AppResult<ApprovalRequest> {
        if matches!(
            appr.approval_status,
            ApprovalStatus::Pending | ApprovalStatus::Approved
        ) && appr.expires_at < UtcDateTime::now()
        {
            let expired = appr.transition(ApprovalStatus::Expired)?;
            APPROVALS.upsert(tx, &expired).await?;
            return Ok(expired);
        }
        Ok(appr)
    }

    async fn require_locked(&self, tx: &mut dyn Tx, id: Uuid) -> AppResult<(ApprovalRequest, i64)> {
        APPROVALS
            .get_for_update(tx, id)
            .await?
            .ok_or_else(|| AppError::not_found(format!("ApprovalRequest {id} not found")))
    }

    /// `decide(approval_id, user_id, decision, presented_hash)`.
    pub async fn decide(
        &self,
        tx: &mut dyn Tx,
        approval_id: Uuid,
        user_id: Uuid,
        decision: ApprovalStatus,
        presented_hash: &str,
    ) -> AppResult<ApprovalRequest> {
        let (appr, _) = self.require_locked(tx, approval_id).await?;
        if appr.authorized_owner_id != user_id {
            return Err(AppError::forbidden(
                "only the authorized owner can decide this approval",
            ));
        }
        let appr = self.expire_if_needed(tx, appr).await?;
        if !matches!(
            decision,
            ApprovalStatus::Approved | ApprovalStatus::Rejected
        ) {
            return Err(AppError::conflict("decision must be APPROVED or REJECTED")
                .code("INVALID_DECISION"));
        }
        if presented_hash != appr.action_payload_hash {
            return Err(AppError::conflict(
                "payload changed since it was presented; re-review required",
            )
            .code("APPROVAL_PAYLOAD_MISMATCH"));
        }
        let mut decided = match appr.transition(decision) {
            Ok(a) => a,
            Err(exc) => {
                return Err(AppError::conflict(exc.to_string())
                    .code("INVALID_STATE_TRANSITION")
                    .details(json!({"current": appr.approval_status.as_str()})))
            }
        };
        decided.decision_timestamp = Some(UtcDateTime::now());
        decided.decided_by = Some(user_id);
        APPROVALS.upsert(tx, &decided).await?;
        self.app
            .events()
            .emit(
                tx,
                Emit::new(
                    EventType::ApprovalDecided,
                    decided.project_id,
                    ActorType::User,
                    user_id.to_string(),
                    format!("approval:{}:{}", decided.approval_id, decision.as_str()),
                    json!({
                        "approval_id": decided.approval_id.to_string(),
                        "action_type": decided.action_type.as_str(),
                        "approval_status": decided.approval_status.as_str(),
                    }),
                ),
            )
            .await?;
        Ok(decided)
    }

    /// `require_approved(...)`: an unexpired APPROVED approval of this project for exactly this
    /// action type and payload, or `ApprovalRequiredError` (409). The row stays locked until the
    /// transaction ends.
    pub async fn require_approved(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        approval_id: Option<Uuid>,
        action_type: ActionType,
        payload: &JsonMap,
    ) -> AppResult<ApprovedAction> {
        let Some(id) = approval_id else {
            return Err(AppError::approval_required(format!(
                "{} requires owner approval",
                action_type.as_str()
            )));
        };
        let (appr, record_version) = self.require_locked(tx, id).await?;
        if appr.project_id != project_id {
            // Indistinguishable from a missing approval: other projects' ids stay undiscoverable.
            return Err(AppError::not_found(format!(
                "ApprovalRequest {id} not found"
            )));
        }
        let appr = self.expire_if_needed(tx, appr).await?;
        if appr.action_type != action_type {
            return Err(AppError::approval_required(
                "approval is for a different action type",
            ));
        }
        if appr.approval_status != ApprovalStatus::Approved {
            return Err(AppError::approval_required(format!(
                "approval is {}",
                appr.approval_status.as_str()
            ))
            .details(json!({"approval_status": appr.approval_status.as_str()})));
        }
        if payload_hash(payload) != appr.action_payload_hash {
            return Err(
                AppError::approval_required("payload differs from the approved payload")
                    .code("APPROVAL_PAYLOAD_MISMATCH"),
            );
        }
        // Every write path above returned an error, so the row is as read (and locked).
        Ok(ApprovedAction {
            approval: appr,
            record_version,
        })
    }

    /// `mark_executed(appr)`: APPROVED -> EXECUTED, single use. Fails with 409 `STALE_VERSION` if
    /// the row changed since `require_approved` read it.
    pub async fn mark_executed(
        &self,
        tx: &mut dyn Tx,
        approved: ApprovedAction,
    ) -> AppResult<ApprovalRequest> {
        let executed = approved.approval.transition(ApprovalStatus::Executed)?;
        APPROVALS
            .update_versioned(tx, &executed, approved.record_version)
            .await?;
        Ok(executed)
    }
}
