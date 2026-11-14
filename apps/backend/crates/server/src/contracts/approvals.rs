//! Port of `apps/api/app/contracts/approvals.py`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ss_core::{
    check_transition, new_id, string_enum, utcnow, Checker, Contract, InvalidStateTransition,
    TransitionTable, UtcDateTime, Validate,
};
use uuid::Uuid;

use super::{plain_contract, JsonMap};
use crate::pyjson;

string_enum! {
    #[derive(Default)]
    pub enum ApprovalStatus {
        #[default]
        Pending = "PENDING",
        Approved = "APPROVED",
        Rejected = "REJECTED",
        Expired = "EXPIRED",
        Cancelled = "CANCELLED",
        Executed = "EXECUTED",
    }
}

/// `APPROVAL_TRANSITIONS`.
pub const APPROVAL_TRANSITIONS: &TransitionTable<ApprovalStatus> = &[
    (
        ApprovalStatus::Pending,
        &[
            ApprovalStatus::Approved,
            ApprovalStatus::Rejected,
            ApprovalStatus::Expired,
            ApprovalStatus::Cancelled,
        ],
    ),
    (
        ApprovalStatus::Approved,
        &[
            ApprovalStatus::Executed,
            ApprovalStatus::Expired,
            ApprovalStatus::Cancelled,
        ],
    ),
    (ApprovalStatus::Rejected, &[]),
    (ApprovalStatus::Expired, &[]),
    (ApprovalStatus::Cancelled, &[]),
    (ApprovalStatus::Executed, &[]),
];

string_enum! {
    pub enum ActionType {
        SendMessage = "SEND_MESSAGE",
        PlaceCall = "PLACE_CALL",
        ContactSupplier = "CONTACT_SUPPLIER",
        Purchase = "PURCHASE",
        FinancialCommitment = "FINANCIAL_COMMITMENT",
        ApproveLayout = "APPROVE_LAYOUT",
        ApproveBom = "APPROVE_BOM",
    }
}

string_enum! {
    pub enum RiskCategory {
        ExternalCommunication = "EXTERNAL_COMMUNICATION",
        /// Purchases, money, binding terms.
        Consequential = "CONSEQUENTIAL",
        Internal = "INTERNAL",
    }
}

/// `payload_hash(payload)`: SHA-256 hex of
/// `json.dumps(payload, sort_keys=True, separators=(",", ":"), default=str)`.
///
/// Parity notes: integers are exact within i64/u64 (serde_json cannot represent larger integers
/// without the `arbitrary_precision` feature; Python could). Python's `default=str` only applies to
/// non-JSON objects (UUID, datetime) a Python caller puts in the dict; a Rust payload is already
/// JSON, so a value that must hash identically in both backends must be stored as its JSON string.
pub fn payload_hash(payload: &JsonMap) -> String {
    let canonical = pyjson::dumps_canonical(&serde_json::Value::Object(payload.clone()));
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

fn default_status() -> ApprovalStatus {
    ApprovalStatus::Pending
}

/// `ApprovalRequest`. Deserialization applies `model_post_init`: an empty `action_payload_hash` is
/// filled with `payload_hash(action_payload)`; a stored non-empty hash is kept as is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "ApprovalRequestWire")]
pub struct ApprovalRequest {
    pub approval_id: Uuid,
    pub project_id: Uuid,
    pub requested_by_agent: String,
    pub action_type: ActionType,
    pub action_payload: JsonMap,
    pub action_payload_hash: String,
    pub risk_category: RiskCategory,
    pub approval_status: ApprovalStatus,
    /// What the owner is approving, in plain language.
    pub summary: String,
    pub expires_at: UtcDateTime,
    pub authorized_owner_id: Uuid,
    pub decision_timestamp: Option<UtcDateTime>,
    pub decided_by: Option<Uuid>,
    pub idempotency_key: String,
    pub created_at: UtcDateTime,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalRequestWire {
    #[serde(default = "new_id")]
    approval_id: Uuid,
    project_id: Uuid,
    requested_by_agent: String,
    action_type: ActionType,
    action_payload: JsonMap,
    #[serde(default)]
    action_payload_hash: String,
    risk_category: RiskCategory,
    #[serde(default = "default_status")]
    approval_status: ApprovalStatus,
    summary: String,
    expires_at: UtcDateTime,
    authorized_owner_id: Uuid,
    #[serde(default)]
    decision_timestamp: Option<UtcDateTime>,
    #[serde(default)]
    decided_by: Option<Uuid>,
    idempotency_key: String,
    #[serde(default = "utcnow")]
    created_at: UtcDateTime,
}

impl From<ApprovalRequestWire> for ApprovalRequest {
    fn from(w: ApprovalRequestWire) -> Self {
        let action_payload_hash = if w.action_payload_hash.is_empty() {
            payload_hash(&w.action_payload)
        } else {
            w.action_payload_hash
        };
        ApprovalRequest {
            approval_id: w.approval_id,
            project_id: w.project_id,
            requested_by_agent: w.requested_by_agent,
            action_type: w.action_type,
            action_payload: w.action_payload,
            action_payload_hash,
            risk_category: w.risk_category,
            approval_status: w.approval_status,
            summary: w.summary,
            expires_at: w.expires_at,
            authorized_owner_id: w.authorized_owner_id,
            decision_timestamp: w.decision_timestamp,
            decided_by: w.decided_by,
            idempotency_key: w.idempotency_key,
            created_at: w.created_at,
        }
    }
}

/// Required fields of a new approval; everything else takes its Python default.
#[derive(Debug, Clone)]
pub struct NewApproval {
    pub project_id: Uuid,
    pub requested_by_agent: String,
    pub action_type: ActionType,
    pub action_payload: JsonMap,
    pub risk_category: RiskCategory,
    pub summary: String,
    pub expires_at: UtcDateTime,
    pub authorized_owner_id: Uuid,
    pub idempotency_key: String,
}

impl ApprovalRequest {
    pub fn new(n: NewApproval) -> Self {
        let action_payload_hash = payload_hash(&n.action_payload);
        ApprovalRequest {
            approval_id: new_id(),
            project_id: n.project_id,
            requested_by_agent: n.requested_by_agent,
            action_type: n.action_type,
            action_payload: n.action_payload,
            action_payload_hash,
            risk_category: n.risk_category,
            approval_status: ApprovalStatus::Pending,
            summary: n.summary,
            expires_at: n.expires_at,
            authorized_owner_id: n.authorized_owner_id,
            decision_timestamp: None,
            decided_by: None,
            idempotency_key: n.idempotency_key,
            created_at: utcnow(),
        }
    }

    /// `ApprovalRequest.transition(target)`.
    pub fn transition(&self, target: ApprovalStatus) -> Result<Self, InvalidStateTransition> {
        check_transition(
            "approval",
            APPROVAL_TRANSITIONS,
            self.approval_status,
            target,
        )?;
        let mut next = self.clone();
        next.approval_status = target;
        Ok(next)
    }
}

impl Validate for ApprovalRequest {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for ApprovalRequest {
    const NAME: &'static str = "ApprovalRequest";
}

/// `ApprovalDecision` (request body of the decision route).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecision {
    /// APPROVED or REJECTED.
    pub decision: ApprovalStatus,
    /// Hash of the payload the owner saw; must match.
    pub action_payload_hash: String,
    #[serde(default)]
    pub note: Option<String>,
}

plain_contract!(ApprovalDecision);
