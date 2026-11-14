//! The approval gate at the point where a simulated payment is sent.
//!
//! A simulated payment is a consequential action in the product's model even though Nessie money
//! is mock money. The flow the server integration follows:
//!
//! 1. Build a [`PaymentIntent`] from a planned payment, the sandbox ids and an attempt id
//!    ([`crate::MoveInPaymentPlan::intent_for`]). The plan item id and the attempt id are part
//!    of the approved payload and start the Nessie description ([`description_prefix`]), so a
//!    transaction found in the sandbox can be matched to its approval.
//! 2. Request an approval with `action_type = intent.action.as_str()` (`PURCHASE` or
//!    `FINANCIAL_COMMITMENT`, both existing `ActionType` values), `action_payload =
//!    intent.approval_payload()`, `risk_category = CONSEQUENTIAL` and `summary =
//!    intent.summary()`.
//! 3. When the owner has approved it, call the server's `require_approved(tx, project_id,
//!    approval_id, action, &intent.approval_payload())`, turn **the approval it returned in that
//!    same transaction** into an [`ApprovalEvidence`] (never anything taken from the request),
//!    and build a [`PaymentAuthorization`] with [`PaymentAuthorization::verify`].
//! 4. Pass the authorization to [`crate::NessieBank::execute`]. It is consumed (not `Clone`), the
//!    bank refuses an approval id it has already used, and the Nessie request body is built from
//!    the approved intent, so no other payload can be sent. Spend the approval with
//!    `mark_executed` in the same transaction, which is the durable single-use guard.
//!
//! What [`PaymentAuthorization::verify`] proves, and what it does not: it checks the evidence it
//! is handed against the intent (status APPROVED, the action type, the project, unexpired, the
//! stored payload equal to the intent's payload). [`ApprovalEvidence`] is a plain struct anyone
//! can fill in, so `verify` alone only proves that its caller *claims* an APPROVED approval for
//! this payload. The authoritative checks are the server's: `require_approved` (the approval's
//! project, expiry, action type, status and payload hash, with the row locked) and
//! `mark_executed` (single use) in the same transaction as the bank call. This crate cannot
//! depend on the server, so that split cannot be closed here; the server must build the evidence
//! only from what `require_approved` returned.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::models::validate_id;
use crate::money::format_usd;
use crate::Simulation;

/// The `ActionType` values (`apps/api/app/contracts/approvals.py`) that can authorize a
/// simulated payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApprovalAction {
    /// Build-out and furnishing purchases at a supplier.
    #[serde(rename = "PURCHASE")]
    Purchase,
    /// Deposits, rent and recurring obligations to the landlord.
    #[serde(rename = "FINANCIAL_COMMITMENT")]
    FinancialCommitment,
}

impl ApprovalAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            ApprovalAction::Purchase => "PURCHASE",
            ApprovalAction::FinancialCommitment => "FINANCIAL_COMMITMENT",
        }
    }
}

/// The facts of a stored approval, as the server's `require_approved` returned it
/// (`ApprovalRequest`'s `approval_id`, `project_id`, `action_type`, `approval_status`,
/// `action_payload` and `expires_at`).
///
/// A plain struct: it carries no proof of where its values came from. Build it only from the
/// approval `require_approved` returned in the transaction that also makes the bank call and
/// `mark_executed`, never from request data (see the module documentation).
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalEvidence {
    pub approval_id: Uuid,
    pub project_id: Uuid,
    /// The `ActionType` wire value.
    pub action_type: String,
    /// The `ApprovalStatus` wire value.
    pub approval_status: String,
    pub action_payload: Map<String, Value>,
    pub expires_at: DateTime<Utc>,
}

/// The Nessie call a simulated payment makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentOperation {
    /// `POST /accounts/{account_id}/purchases` at `merchant_id` (the path account pays).
    Purchase {
        account_id: String,
        merchant_id: String,
    },
    /// `POST /accounts/{account_id}/bills` with `status: recurring`. Records the monthly
    /// obligation in the sandbox; no money moves when the bill is created.
    CreateRecurringBill {
        account_id: String,
        payee: String,
        nickname: String,
        recurring_day: u8,
    },
    /// `POST /bills/{bill_id}/payments` with the approved `amount_cents`. `account_id` is the
    /// account the bill belongs to, which pays it: it is not sent (the path names the bill), but
    /// it is approved and compared with the payer of the bank's answer.
    PayBill { account_id: String, bill_id: String },
}

impl PaymentOperation {
    pub const fn name(&self) -> &'static str {
        match self {
            PaymentOperation::Purchase { .. } => "purchase",
            PaymentOperation::CreateRecurringBill { .. } => "create_recurring_bill",
            PaymentOperation::PayBill { .. } => "pay_bill",
        }
    }

    /// True when the call moves (mock) money.
    pub const fn moves_money(&self) -> bool {
        !matches!(self, PaymentOperation::CreateRecurringBill { .. })
    }
}

/// One simulated payment, exactly as it will be approved and sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentIntent {
    /// The bank the approval is for: an approval for the fixture bank cannot authorize a
    /// sandbox call, and the reverse.
    pub simulation: Simulation,
    pub project_id: Uuid,
    pub property_id: Uuid,
    pub scenario_id: Uuid,
    /// At most [`PLAN_ITEM_ID_MAX_LEN`] printable ASCII characters without spaces.
    pub plan_item_id: String,
    /// Identifies this approval's attempt in the sandbox: part of the approved payload and of
    /// the Nessie description ([`validate_attempt_id`]).
    pub attempt_id: String,
    pub action: ApprovalAction,
    pub operation: PaymentOperation,
    /// US cents, `>= 1`.
    pub amount_minor: i64,
    /// Always `USD`: Nessie amounts are US cents.
    pub currency: String,
    /// Sent to Nessie as the transaction description; starts with
    /// [`description_prefix`]`(plan_item_id, attempt_id)`.
    pub description: String,
}

pub const PAYLOAD_INTEGRATION: &str = "capital_one_nessie_v2";

/// Longest plan item id an intent accepts.
pub const PLAN_ITEM_ID_MAX_LEN: usize = 64;
/// An attempt id is 8 to 16 lower-case hexadecimal characters.
pub const ATTEMPT_ID_MIN_LEN: usize = 8;
pub const ATTEMPT_ID_MAX_LEN: usize = 16;

/// Checks an attempt id: 8 to 16 lower-case hexadecimal characters.
pub fn validate_attempt_id(attempt_id: &str) -> Result<(), String> {
    let n = attempt_id.len();
    if !(ATTEMPT_ID_MIN_LEN..=ATTEMPT_ID_MAX_LEN).contains(&n)
        || !attempt_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!(
            "attempt_id must be {ATTEMPT_ID_MIN_LEN} to {ATTEMPT_ID_MAX_LEN} lower-case hexadecimal characters"
        ));
    }
    Ok(())
}

/// A fresh random attempt id: 12 lower-case hexadecimal characters (48 random bits of a v4
/// UUID). A caller may instead derive its own, as long as it passes [`validate_attempt_id`].
pub fn new_attempt_id() -> String {
    Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect()
}

/// The start of every simulated payment's Nessie description: it names the plan item and the
/// attempt, so a transaction in the sandbox can be matched to its approval.
pub fn description_prefix(plan_item_id: &str, attempt_id: &str) -> String {
    format!("SIMULATION (Second Site plan item {plan_item_id}, attempt {attempt_id}): ")
}

impl PaymentIntent {
    /// Checks the intent against the documented Nessie limits and this crate's rules.
    pub fn validate(&self) -> Result<(), String> {
        if self.amount_minor < 1 {
            return Err(format!(
                "amount_minor must be at least 1 cent, got {}",
                self.amount_minor
            ));
        }
        if self.currency != "USD" {
            return Err(format!(
                "Nessie amounts are US cents; currency {} cannot be simulated",
                self.currency
            ));
        }
        if self.plan_item_id.is_empty() {
            return Err("plan_item_id must not be empty".into());
        }
        if self.plan_item_id.len() > PLAN_ITEM_ID_MAX_LEN
            || !self.plan_item_id.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(format!(
                "plan_item_id must be at most {PLAN_ITEM_ID_MAX_LEN} printable ASCII characters without spaces"
            ));
        }
        validate_attempt_id(&self.attempt_id)?;
        if !self.description.starts_with("SIMULATION") {
            return Err("description must start with SIMULATION".into());
        }
        if !self
            .description
            .starts_with(&description_prefix(&self.plan_item_id, &self.attempt_id))
        {
            return Err("description must name the plan item and the attempt id".into());
        }
        if self.description.chars().count() > 200 {
            return Err("description must be at most 200 characters".into());
        }
        match &self.operation {
            PaymentOperation::Purchase {
                account_id,
                merchant_id,
            } => {
                validate_id("account_id", account_id)?;
                validate_id("merchant_id", merchant_id)?;
            }
            PaymentOperation::CreateRecurringBill {
                account_id,
                payee,
                nickname,
                recurring_day,
            } => {
                validate_id("account_id", account_id)?;
                let n = payee.chars().count();
                if !(1..=80).contains(&n) {
                    return Err("payee must be 1 to 80 characters".into());
                }
                if nickname.chars().count() > 80 {
                    return Err("nickname must be at most 80 characters".into());
                }
                if !(1..=31).contains(recurring_day) {
                    return Err("recurring_day must be 1 to 31".into());
                }
                if self.action != ApprovalAction::FinancialCommitment {
                    return Err("a recurring bill is a FINANCIAL_COMMITMENT".into());
                }
            }
            PaymentOperation::PayBill {
                account_id,
                bill_id,
            } => {
                validate_id("account_id", account_id)?;
                validate_id("bill_id", bill_id)?;
                if self.action != ApprovalAction::FinancialCommitment {
                    return Err("a bill payment is a FINANCIAL_COMMITMENT".into());
                }
            }
        }
        Ok(())
    }

    /// The approval's `action_payload`: everything the Nessie call depends on, plus the
    /// simulation label. The request body is derived from these same fields.
    pub fn approval_payload(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("integration".into(), json!(PAYLOAD_INTEGRATION));
        m.insert("simulation".into(), json!(self.simulation.as_str()));
        m.insert("disclaimer".into(), json!(self.simulation.disclaimer()));
        m.insert("operation".into(), json!(self.operation.name()));
        m.insert("moves_money".into(), json!(self.operation.moves_money()));
        m.insert("project_id".into(), json!(self.project_id.to_string()));
        m.insert("property_id".into(), json!(self.property_id.to_string()));
        m.insert("scenario_id".into(), json!(self.scenario_id.to_string()));
        m.insert("plan_item_id".into(), json!(self.plan_item_id));
        m.insert("attempt_id".into(), json!(self.attempt_id));
        m.insert("amount_minor".into(), json!(self.amount_minor));
        m.insert("currency".into(), json!(self.currency));
        m.insert("description".into(), json!(self.description));
        match &self.operation {
            PaymentOperation::Purchase {
                account_id,
                merchant_id,
            } => {
                m.insert("account_id".into(), json!(account_id));
                m.insert("merchant_id".into(), json!(merchant_id));
            }
            PaymentOperation::CreateRecurringBill {
                account_id,
                payee,
                nickname,
                recurring_day,
            } => {
                m.insert("account_id".into(), json!(account_id));
                m.insert("payee".into(), json!(payee));
                m.insert("nickname".into(), json!(nickname));
                m.insert("recurring_day".into(), json!(recurring_day));
            }
            PaymentOperation::PayBill {
                account_id,
                bill_id,
            } => {
                m.insert("account_id".into(), json!(account_id));
                m.insert("bill_id".into(), json!(bill_id));
            }
        }
        m
    }

    /// Plain-language text for the approval's `summary`.
    pub fn summary(&self) -> String {
        let bank = match self.simulation {
            Simulation::NessieSandbox => "in the Capital One Nessie sandbox (mock money)",
            Simulation::FixtureBank => "in Second Site's fixture test bank (fixture data)",
        };
        let what = match &self.operation {
            PaymentOperation::Purchase { .. } => {
                format!("a simulated purchase of {}", format_usd(self.amount_minor))
            }
            PaymentOperation::CreateRecurringBill { recurring_day, .. } => format!(
                "a simulated recurring bill of {} on day {recurring_day} of each month (no money moves when it is created)",
                format_usd(self.amount_minor)
            ),
            PaymentOperation::PayBill { .. } => {
                format!("a simulated bill payment of {}", format_usd(self.amount_minor))
            }
        };
        format!(
            "Simulation only: {what} {bank} for plan item {} (attempt {}). Nothing real is paid or committed.",
            self.plan_item_id, self.attempt_id
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthorizationError {
    #[error("approval is {0}, not APPROVED")]
    NotApproved(String),
    #[error("approval is for {found}, this payment needs {expected}")]
    WrongAction { expected: String, found: String },
    #[error("approval belongs to another project")]
    WrongProject,
    #[error("approval has expired")]
    Expired,
    #[error("approved payload differs from the payment being sent")]
    PayloadMismatch,
    #[error("payment intent is invalid: {0}")]
    InvalidIntent(String),
    #[error("approval {0} was already used for a simulated payment")]
    AlreadyUsed(Uuid),
    #[error("approval is for the {approved} bank, this is the {bank} bank")]
    WrongBank {
        approved: Simulation,
        bank: Simulation,
    },
}

/// An approval checked against exactly one simulated payment. Not `Clone`:
/// [`crate::NessieBank::execute`] consumes it. It is only as good as the [`ApprovalEvidence`] it
/// was built from (see the module documentation for what [`PaymentAuthorization::verify`] proves).
#[derive(Debug, PartialEq, Eq)]
pub struct PaymentAuthorization {
    approval_id: Uuid,
    intent: PaymentIntent,
}

impl PaymentAuthorization {
    /// Checks the evidence against the intent, at `now`: status APPROVED, the action type, the
    /// project, unexpired, and the stored payload equal to the intent's payload. It does not (and
    /// cannot) prove that the evidence came from the stored approval; the server's
    /// `require_approved` in the same transaction does.
    pub fn verify(
        evidence: &ApprovalEvidence,
        intent: PaymentIntent,
        now: DateTime<Utc>,
    ) -> Result<Self, AuthorizationError> {
        intent
            .validate()
            .map_err(AuthorizationError::InvalidIntent)?;
        if evidence.approval_status != "APPROVED" {
            return Err(AuthorizationError::NotApproved(
                evidence.approval_status.clone(),
            ));
        }
        if evidence.action_type != intent.action.as_str() {
            return Err(AuthorizationError::WrongAction {
                expected: intent.action.as_str().to_owned(),
                found: evidence.action_type.clone(),
            });
        }
        if evidence.project_id != intent.project_id {
            return Err(AuthorizationError::WrongProject);
        }
        if now >= evidence.expires_at {
            return Err(AuthorizationError::Expired);
        }
        if evidence.action_payload != intent.approval_payload() {
            return Err(AuthorizationError::PayloadMismatch);
        }
        Ok(Self {
            approval_id: evidence.approval_id,
            intent,
        })
    }

    pub fn approval_id(&self) -> Uuid {
        self.approval_id
    }

    pub fn intent(&self) -> &PaymentIntent {
        &self.intent
    }

    pub(crate) fn into_parts(self) -> (Uuid, PaymentIntent) {
        (self.approval_id, self.intent)
    }
}
