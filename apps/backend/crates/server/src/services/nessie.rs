//! The Capital One Nessie sandbox, server side (`ss-nessie` wired into the service): which bank
//! answers, the Rust-owned records of each project's sandbox and of every simulated payment
//! attempt, and the flows behind the routes in [`crate::http::routes::nessie`].
//!
//! Mock money only. Every answer carries the crate's own `simulation` label and disclaimer (taken
//! from the crate's labelled output, never chosen by a caller), and nothing here is a real
//! balance, payment, paid deposit or signed commitment.
//!
//! **Storage** (`nessie_simulation_records`, a Rust-owned table that alembic does not know; see
//! `migrations/rust-owned/`): one `sandbox` row per project with the sandbox ids, and one
//! `payment_attempt` row per property and plan item (`record_key` `{property_id}:{plan_item_id}`)
//! with the approval id, attempt id, outcome, provider status and Nessie transaction or bill id.
//! Sandbox objects are never looked up in Nessie by name: every project shares one key and the
//! stand-in names are constants.
//!
//! **Single payment per plan item (D2).** The execute transaction takes the advisory lock
//! [`plan_item_lock_key`] and refuses (409, without reaching the bank) when the plan item's
//! attempt row exists and was not refused by the bank. Otherwise it claims the row
//! (`IN_FLIGHT`) before the bank call. A refusal by the bank marks the row `REFUSED` (kept, with
//! the refusal history) and the next attempt takes it over; any other outcome keeps it.
//!
//! **Durable outcome (D1).** The outcome is decided inside the transaction and committed before
//! the answer: a refusal leaves the approval APPROVED; an attempt that may have happened spends it
//! (`mark_executed`) and is recorded `MAY_EXIST`; then the route answers 422 or 502.
//!
//! **Lock order.** Execute: plan-item advisory lock, then the approval row (`require_approved`,
//! `FOR UPDATE`), then the attempt row (its insert also takes a key-share lock on the project
//! row), then the bank call, then the approval update (`mark_executed`). It emits no event, so it
//! never takes the project's event lock. Approval request: plan-item advisory lock, then the
//! approval insert and the `approval.requested` emit (event lock), as every other emitter takes it
//! last. Set-up: the project's sandbox advisory lock ([`sandbox_lock_key`]), the bank calls, then
//! the sandbox row. The approval decision route takes the approval row and then the event lock,
//! so no cycle exists with these orders.
//!
//! **What is held during each bank call.** Set-up: the request transaction (one pool connection)
//! and the project's sandbox advisory lock, which only another set-up of the same project waits
//! for. Execute: the transaction, the plan-item advisory lock, the approval row lock, the claimed
//! attempt row and a key-share lock on the project row (so a business-profile update or a search
//! of the same project waits for the call). That is what makes the payment single-use, and it is
//! bounded by the bank: the fixture bank answers in process (microseconds); the live client sends
//! the POST once with a whole-request deadline of `NessieConfig::timeout_s` (20 s). Plan,
//! affordability, sandbox reads, the approval request's account check and refresh make their
//! bank calls with no transaction open. With the in-memory store a transaction holds the store's
//! global lock, so every other request waits for a set-up or an execute (same bounds).

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use ss_core::{utcnow, Checker, Contract, IntegrationStatus, UtcDateTime, Validate};
use ss_domain::contracts::{BillOfMaterials, CostScenario, ProviderStatusEntry};
use ss_domain::integrations::base::{status_entry, ProviderErrorKind};
use ss_nessie::approval::validate_attempt_id;
use ss_nessie::models::{
    Account, AccountCreate, AccountType, Address, CustomerCreate, Transaction,
};
use ss_nessie::money::format_usd;
use ss_nessie::plan::{stand_in_merchant, ITEM_FIRST_MONTH, ITEM_OCCUPANCY_BILL};
use ss_nessie::{
    plan_move_in_payments, select_bank, AffordabilityReport, ApprovalAction, ApprovalEvidence,
    AttemptOutcome, AuthorizationError, BankRecord, BomTotals, FixtureBank, MoveInPaymentPlan,
    NessieBank, NessieConfig, NessieError, PayeeRole, PaymentAuthorization, PlanError,
    SandboxTargets, SimulatedOutcome, SimulatedPaymentRecord, Simulation,
};
use uuid::Uuid;

use crate::app::{App, ProviderStatus};
use crate::config::Settings;
use crate::contracts::{payload_hash, ActionType, ApprovalRequest, ApprovalStatus, JsonMap};
use crate::error::{AppError, AppResult};
use crate::logging;
use crate::services::approvals::{ApprovalSpec, ApprovedAction};
use crate::services::design::lock_key;
use crate::store::repo::{superseded, Document, ProjectScoped, Repo, APPROVALS, BOMS, COST_SCENARIOS};
use crate::store::schema::{self, TableDef};
use crate::store::{Query, Row, Tx};

/// `kind` of the project's sandbox row.
pub const KIND_SANDBOX: &str = "sandbox";
/// `kind` of a payment attempt row.
pub const KIND_PAYMENT: &str = "payment_attempt";
/// `record_key` of the sandbox row.
pub const SANDBOX_KEY: &str = "sandbox";

/// The sandbox has every id and can be used.
pub const SANDBOX_READY: &str = "READY";
/// Set-up stopped part way; the ids created so far are recorded.
pub const SANDBOX_INCOMPLETE: &str = "INCOMPLETE";

/// Claimed for a bank call in progress (never visible after a commit).
pub const ATTEMPT_IN_FLIGHT: &str = "IN_FLIGHT";
/// The bank answered with the approved object; `outcome` says what it reported (pending,
/// completed, ...). Not "completed" unless the bank said so.
pub const ATTEMPT_SENT: &str = "SENT";
/// The bank refused before anything existed; the approval was not spent.
pub const ATTEMPT_REFUSED: &str = "REFUSED";
/// The call may have been carried out (timeout, 5xx, an answer about another object); the
/// approval is spent and the owner must check the sandbox account before trying again.
pub const ATTEMPT_MAY_EXIST: &str = "MAY_EXIST";

/// Highest opening balance a sandbox is set up with ($1,000,000,000.00 of mock money).
pub const MAX_OPENING_BALANCE_MINOR: i64 = 100_000_000_000;

/// `requested_by_agent` of the approvals this module requests.
pub const APPROVAL_AGENT: &str = "nessie-sandbox";

const SCENARIO_RULE: &str = "The property's live (not superseded) cost scenario; when several are live, the latest by created_at. Superseded scenarios are ignored.";
const BOM_RULE: &str = "The property's latest bill of materials by created_at (any design version of this property in this project), or none.";

// ---- The bank this service uses ------------------------------------------------------------------

/// The bank and what identifies it, so stored sandbox ids are only used with the bank that made
/// them.
pub struct NessieState {
    bank: Arc<dyn NessieBank>,
    /// `fixture:<random per process>` for the fixture bank (it forgets everything on restart and
    /// reuses the same ids), `sandbox:<base URL>` for the Capital One sandbox.
    bank_ref: String,
    config_error: Option<String>,
}

impl NessieState {
    /// `select_bank`: the live client only with `NESSIE_PROVIDER_MODE=live` and a key.
    pub fn from_config(config: &NessieConfig) -> Self {
        let bank = select_bank(config);
        let bank_ref = match bank.simulation() {
            Simulation::FixtureBank => format!("fixture:{}", Uuid::new_v4().simple()),
            Simulation::NessieSandbox => format!("sandbox:{}", config.base_url),
        };
        Self {
            bank,
            bank_ref,
            config_error: None,
        }
    }

    /// A fresh in-memory fixture bank.
    pub fn fixture() -> Self {
        Self::from_config(&NessieConfig::default())
    }

    /// A given bank (tests: a live client pointed at a local stub). `bank_ref` names it.
    pub fn with_bank(bank: Arc<dyn NessieBank>, bank_ref: impl Into<String>) -> Self {
        Self {
            bank,
            bank_ref: bank_ref.into(),
            config_error: None,
        }
    }

    /// Invalid `NESSIE_*` settings: `/ready` says BLOCKED and every route answers 503.
    pub fn misconfigured(error: impl Into<String>) -> Self {
        Self {
            bank: Arc::new(FixtureBank::new()),
            bank_ref: "misconfigured".into(),
            config_error: Some(error.into()),
        }
    }

    pub fn bank(&self) -> AppResult<Arc<dyn NessieBank>> {
        match &self.config_error {
            None => Ok(self.bank.clone()),
            Some(e) => Err(AppError::provider_unavailable(format!(
                "the Nessie sandbox integration is misconfigured: {e}"
            ))
            .code("NESSIE_MISCONFIGURED")
            .with_retryable(false)),
        }
    }

    pub fn simulation(&self) -> Simulation {
        self.bank.simulation()
    }

    pub fn bank_ref(&self) -> &str {
        &self.bank_ref
    }

    /// The `/ready` entry (`provider: "nessie"`).
    pub fn status(&self) -> ProviderStatusEntry {
        match &self.config_error {
            None => self.bank.status(),
            Some(e) => status_entry(
                ss_nessie::PROVIDER,
                "invalid",
                IntegrationStatus::Blocked,
                false,
                &format!(
                    "NOT_CONFIGURED: invalid Nessie settings ({e}); every Nessie route answers 503. Mock money only."
                ),
            ),
        }
    }
}

/// Where the current [`NessieState`] lives (`app.extensions`); `/ready` reads it at call time.
pub struct NessieSlot {
    state: RwLock<Arc<NessieState>>,
}

impl NessieSlot {
    pub fn current(&self) -> Arc<NessieState> {
        self.state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

struct NessieReady(Arc<NessieSlot>);

impl ProviderStatus for NessieReady {
    fn name(&self) -> &str {
        ss_nessie::PROVIDER
    }

    fn status(&self, _settings: &Settings) -> ProviderStatusEntry {
        self.0.current().status()
    }
}

/// Selects the bank from the environment (`NessieConfig::from_env`: `NESSIE_PROVIDER_MODE`,
/// `NESSIE_API_KEY`, `NESSIE_BASE_URL`) and registers the `/ready` entry. The key is never
/// logged: only the settings error text is, and it names the variable, not its value.
pub fn register(app: &App) {
    let state = match NessieConfig::from_env() {
        Ok(c) => NessieState::from_config(&c),
        Err(e) => {
            logging::warning(
                "app.integrations.nessie",
                "invalid Nessie settings; the Nessie routes answer 503",
                &[("error", Value::String(e.clone()))],
            );
            NessieState::misconfigured(e)
        }
    };
    install(app, state);
}

/// Installs `state` (replacing the current one: tests use it to choose the bank).
pub fn install(app: &App, state: NessieState) {
    match app.extensions.get::<NessieSlot>() {
        Some(slot) => {
            *slot.state.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(state);
        }
        None => {
            let slot = Arc::new(NessieSlot {
                state: RwLock::new(Arc::new(state)),
            });
            app.extensions.insert(slot.clone());
            app.providers.register(Arc::new(NessieReady(slot)));
        }
    }
}

impl App {
    /// The Nessie bank this service uses.
    pub fn nessie(&self) -> Arc<NessieState> {
        match self.extensions.get::<NessieSlot>() {
            Some(slot) => slot.current(),
            None => Arc::new(NessieState::misconfigured("the Nessie integration is not registered")),
        }
    }
}

/// Runs one blocking bank call on tokio's blocking pool.
pub async fn bank_call<T, F>(bank: Arc<dyn NessieBank>, f: F) -> AppResult<Result<T, NessieError>>
where
    T: Send + 'static,
    F: FnOnce(&dyn NessieBank) -> Result<T, NessieError> + Send + 'static,
{
    crate::integrations::blocking(move || f(bank.as_ref())).await
}

/// A bank failure outside `execute`. Messages never contain the key (the crate scrubs them).
pub fn bank_error(e: &NessieError) -> AppError {
    match e {
        NessieError::Provider {
            error,
            http_status,
            problem,
        } => AppError::provider_unavailable(error.to_string())
            .details(json!({
                "provider": error.provider,
                "kind": error.kind.as_str(),
                "http_status": http_status,
                "problem_slug": problem.as_ref().map(|p| p.slug.clone()),
            }))
            .with_retryable(error.retryable())
            .with_class_name("ProviderError"),
        NessieError::NotAuthorized(_) => AppError::approval_required(e.to_string()),
        NessieError::InvalidRequest(_) | NessieError::Money(_) => {
            AppError::bad_request(e.to_string()).code("NESSIE_INVALID_REQUEST")
        }
    }
}

fn is_not_found(e: &NessieError) -> bool {
    e.kind() == Some(ProviderErrorKind::NotFound)
}

// ---- Lock keys ---------------------------------------------------------------------------------------

/// The plan item's lock (D2): the first 8 bytes of
/// `SHA-256("second-site:nessie:{project_id}:{property_id}:{plan_item_id}")`, big-endian signed,
/// the formula of `services::design::lock_key` with a composite id. Python:
/// `int.from_bytes(hashlib.sha256(f"second-site:nessie:{project}:{property}:{item}".encode()).digest()[:8], "big", signed=True)`.
pub fn plan_item_lock_key(project_id: Uuid, property_id: Uuid, plan_item_id: &str) -> i64 {
    let digest =
        Sha256::digest(format!("second-site:nessie:{project_id}:{property_id}:{plan_item_id}"));
    let mut b = [0u8; 8];
    b.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(b)
}

/// The project's sandbox set-up lock: `lock_key("nessie_sandbox", project_id)`.
pub fn sandbox_lock_key(project_id: Uuid) -> i64 {
    lock_key("nessie_sandbox", project_id)
}

// ---- The Rust-owned record ---------------------------------------------------------------------------

/// The ids of the project's sandbox (kind `sandbox`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxData {
    /// [`SANDBOX_READY`] or [`SANDBOX_INCOMPLETE`].
    pub status: String,
    /// `Simulation` wire value of the bank that made the ids.
    pub simulation: String,
    pub bank_ref: String,
    pub customer_id: Option<String>,
    pub account_id: Option<String>,
    pub landlord_merchant_id: Option<String>,
    pub contractor_merchant_id: Option<String>,
    pub supplier_merchant_id: Option<String>,
    pub opening_balance_minor: i64,
    /// `BUSINESS_PROFILE_OPENING_CAPITAL` or `SET_UP_REQUEST`.
    pub opening_balance_source: String,
    pub failure: Option<String>,
    /// Earlier sandboxes of this project that were replaced, and why.
    pub replaced: Vec<ReplacedSandbox>,
    pub set_up_at: UtcDateTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacedSandbox {
    pub simulation: String,
    pub customer_id: Option<String>,
    pub account_id: Option<String>,
    pub reason: String,
    pub replaced_at: UtcDateTime,
}

/// What the bank reported about an attempt that did not produce a record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptFailure {
    pub outcome: String,
    pub http_status: Option<u16>,
    pub problem_slug: Option<String>,
    pub kind: Option<String>,
    pub message: String,
}

/// A refusal by the bank that an earlier attempt of this plan item got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EarlierRefusal {
    pub approval_id: Uuid,
    pub attempt_id: String,
    pub failure: AttemptFailure,
    pub attempted_at: UtcDateTime,
}

/// `SimulatedPaymentRecord` as stored (the crate's record is not deserializable; its disclaimer
/// is derived from `simulation` again when it is read back).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredPaymentRecord {
    pub simulation: String,
    pub approval_id: Uuid,
    pub project_id: Uuid,
    pub property_id: Uuid,
    pub scenario_id: Uuid,
    pub plan_item_id: String,
    pub attempt_id: String,
    pub operation: String,
    pub action_type: String,
    pub moves_money: bool,
    pub amount_minor: i64,
    pub currency: String,
    pub outcome: String,
    pub provider_status: String,
    pub nessie_transaction_id: Option<String>,
    pub nessie_bill_id: Option<String>,
    pub note: String,
}

impl StoredPaymentRecord {
    pub fn of(r: &SimulatedPaymentRecord) -> Self {
        Self {
            simulation: r.simulation.as_str().to_owned(),
            approval_id: r.approval_id,
            project_id: r.project_id,
            property_id: r.property_id,
            scenario_id: r.scenario_id,
            plan_item_id: r.plan_item_id.clone(),
            attempt_id: r.attempt_id.clone(),
            operation: r.operation.to_owned(),
            action_type: r.action_type.as_str().to_owned(),
            moves_money: r.moves_money,
            amount_minor: r.amount_minor,
            currency: r.currency.clone(),
            outcome: r.outcome.as_str().to_owned(),
            provider_status: r.provider_status.clone(),
            nessie_transaction_id: r.nessie_transaction_id.clone(),
            nessie_bill_id: r.nessie_bill_id.clone(),
            note: r.note.clone(),
        }
    }

    /// The crate's labelled record again.
    pub fn restore(&self) -> AppResult<SimulatedPaymentRecord> {
        let bad = |what: &str| AppError::internal(format!("stored Nessie record: unknown {what}"));
        let simulation = Simulation::from_wire(&self.simulation).ok_or_else(|| bad("simulation"))?;
        let operation = match self.operation.as_str() {
            "purchase" => "purchase",
            "create_recurring_bill" => "create_recurring_bill",
            "pay_bill" => "pay_bill",
            _ => return Err(bad("operation")),
        };
        let action_type = match self.action_type.as_str() {
            "PURCHASE" => ApprovalAction::Purchase,
            "FINANCIAL_COMMITMENT" => ApprovalAction::FinancialCommitment,
            _ => return Err(bad("action type")),
        };
        let outcome = outcome_from_wire(&self.outcome).ok_or_else(|| bad("outcome"))?;
        Ok(SimulatedPaymentRecord {
            simulation,
            disclaimer: simulation.disclaimer(),
            approval_id: self.approval_id,
            project_id: self.project_id,
            property_id: self.property_id,
            scenario_id: self.scenario_id,
            plan_item_id: self.plan_item_id.clone(),
            attempt_id: self.attempt_id.clone(),
            operation,
            action_type,
            moves_money: self.moves_money,
            amount_minor: self.amount_minor,
            currency: self.currency.clone(),
            outcome,
            provider_status: self.provider_status.clone(),
            nessie_transaction_id: self.nessie_transaction_id.clone(),
            nessie_bill_id: self.nessie_bill_id.clone(),
            note: self.note.clone(),
        })
    }
}

fn outcome_from_wire(s: &str) -> Option<SimulatedOutcome> {
    [
        SimulatedOutcome::SimulatedPending,
        SimulatedOutcome::SimulatedCompleted,
        SimulatedOutcome::SimulatedDeclined,
        SimulatedOutcome::SimulatedCancelled,
        SimulatedOutcome::SimulatedBillRecorded,
        SimulatedOutcome::Unknown,
    ]
    .into_iter()
    .find(|o| o.as_str() == s)
}

/// One simulated payment attempt of a plan item (kind `payment_attempt`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentAttemptData {
    /// [`ATTEMPT_SENT`], [`ATTEMPT_REFUSED`] or [`ATTEMPT_MAY_EXIST`] ([`ATTEMPT_IN_FLIGHT`]
    /// only inside the execute transaction).
    pub attempt_status: String,
    pub simulation: String,
    pub bank_ref: String,
    pub sandbox_record_id: Uuid,
    pub property_id: Uuid,
    pub scenario_id: Uuid,
    pub plan_item_id: String,
    pub attempt_id: String,
    pub approval_id: Uuid,
    pub operation: String,
    pub amount_minor: i64,
    /// `SimulatedOutcome` wire value, when the bank answered with the approved object.
    pub outcome: Option<String>,
    pub provider_status: Option<String>,
    pub nessie_transaction_id: Option<String>,
    pub nessie_bill_id: Option<String>,
    pub record: Option<StoredPaymentRecord>,
    pub failure: Option<AttemptFailure>,
    pub earlier_refusals: Vec<EarlierRefusal>,
    pub attempted_at: UtcDateTime,
    pub refreshed_at: Option<UtcDateTime>,
}

/// A row of `nessie_simulation_records` (`id` is `record_id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NessieSimulationRecord {
    pub record_id: Uuid,
    pub project_id: Uuid,
    /// [`KIND_SANDBOX`] or [`KIND_PAYMENT`].
    pub kind: String,
    /// [`SANDBOX_KEY`], or `{property_id}:{plan_item_id}`.
    pub record_key: String,
    pub approval_id: Option<Uuid>,
    #[serde(default)]
    pub sandbox: Option<SandboxData>,
    #[serde(default)]
    pub payment: Option<PaymentAttemptData>,
    pub created_at: UtcDateTime,
    pub updated_at: UtcDateTime,
}

impl Validate for NessieSimulationRecord {
    fn check(&self, c: &mut Checker) {
        c.max_len("kind", &self.kind, 32);
        c.max_len("record_key", &self.record_key, 255);
        match self.kind.as_str() {
            KIND_SANDBOX => {
                if self.sandbox.is_none() || self.payment.is_some() || self.record_key != SANDBOX_KEY
                {
                    c.value_error("a sandbox record carries sandbox data only, under record_key 'sandbox'");
                }
            }
            KIND_PAYMENT => {
                if self.payment.is_none() || self.sandbox.is_some() {
                    c.value_error("a payment attempt record carries payment data only");
                }
            }
            _ => c.field_value_error("kind", "kind must be 'sandbox' or 'payment_attempt'"),
        }
    }
}

impl Contract for NessieSimulationRecord {
    const NAME: &'static str = "NessieSimulationRecord";
}

impl Document for NessieSimulationRecord {
    const TABLE: &'static TableDef = &schema::NESSIE_SIMULATION_RECORDS;
    fn id(&self) -> Uuid {
        self.record_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("project_id", self.project_id)
            .with("kind", self.kind.as_str())
            .with("record_key", self.record_key.as_str())
            .with("approval_id", self.approval_id)
    }
}

impl ProjectScoped for NessieSimulationRecord {
    fn project_id(&self) -> Uuid {
        self.project_id
    }
}

pub const NESSIE_RECORDS: Repo<NessieSimulationRecord> = Repo::new();

/// `{property_id}:{plan_item_id}`.
pub fn attempt_key(property_id: Uuid, plan_item_id: &str) -> String {
    format!("{property_id}:{plan_item_id}")
}

async fn record_by_key(
    tx: &mut dyn Tx,
    project_id: Uuid,
    kind: &str,
    key: &str,
) -> AppResult<Option<NessieSimulationRecord>> {
    Ok(NESSIE_RECORDS
        .list(
            tx,
            Query::new()
                .eq("project_id", project_id)
                .eq("kind", kind)
                .eq("record_key", key)
                .limit(1),
        )
        .await?
        .into_iter()
        .next())
}

pub async fn sandbox_record(
    tx: &mut dyn Tx,
    project_id: Uuid,
) -> AppResult<Option<NessieSimulationRecord>> {
    record_by_key(tx, project_id, KIND_SANDBOX, SANDBOX_KEY).await
}

pub async fn attempt_record(
    tx: &mut dyn Tx,
    project_id: Uuid,
    property_id: Uuid,
    plan_item_id: &str,
) -> AppResult<Option<NessieSimulationRecord>> {
    record_by_key(
        tx,
        project_id,
        KIND_PAYMENT,
        &attempt_key(property_id, plan_item_id),
    )
    .await
}

/// Why a stored sandbox cannot be used with the current bank.
fn sandbox_unusable(state: &NessieState, sb: &SandboxData) -> Option<String> {
    if sb.bank_ref != state.bank_ref() {
        return Some(match Simulation::from_wire(&sb.simulation) {
            Some(Simulation::FixtureBank) if state.simulation() == Simulation::FixtureBank => {
                "the fixture bank was reset (the server restarted), so the sandbox it held is gone".into()
            }
            _ => format!(
                "the sandbox was set up in another bank ({}) than the one this server uses ({})",
                sb.simulation,
                state.simulation()
            ),
        });
    }
    if sb.status != SANDBOX_READY {
        return Some(format!(
            "the sandbox set-up did not complete{}",
            sb.failure
                .as_deref()
                .map(|f| format!(": {f}"))
                .unwrap_or_default()
        ));
    }
    if sb.account_id.is_none()
        || sb.landlord_merchant_id.is_none()
        || sb.contractor_merchant_id.is_none()
        || sb.supplier_merchant_id.is_none()
    {
        return Some("the stored sandbox is missing ids".into());
    }
    None
}

/// The project's sandbox if it was set up in this bank and completed, else 409 (for the routes
/// that need one) with the reason.
fn usable_sandbox(
    state: &NessieState,
    rec: Option<NessieSimulationRecord>,
) -> AppResult<(NessieSimulationRecord, SandboxData)> {
    let Some(rec) = rec else {
        return Err(AppError::conflict(
            "no Nessie sandbox is set up for this project; POST /nessie/sandbox first",
        )
        .code("NESSIE_SANDBOX_REQUIRED"));
    };
    let Some(sb) = rec.sandbox.clone() else {
        return Err(AppError::internal("sandbox record without sandbox data"));
    };
    if let Some(reason) = sandbox_unusable(state, &sb) {
        return Err(AppError::conflict(format!(
            "the project's Nessie sandbox cannot be used: {reason}; POST /nessie/sandbox to set it up again"
        ))
        .code("NESSIE_SANDBOX_GONE")
        .details(json!({"reason": reason})));
    }
    Ok((rec, sb))
}

fn targets_of(sb: &SandboxData, occupancy_bill_id: Option<String>) -> SandboxTargets {
    SandboxTargets {
        account_id: sb.account_id.clone().unwrap_or_default(),
        landlord_merchant_id: sb.landlord_merchant_id.clone(),
        contractor_merchant_id: sb.contractor_merchant_id.clone(),
        supplier_merchant_id: sb.supplier_merchant_id.clone(),
        occupancy_bill_id,
    }
}

/// G1: the crate's evidence, built only from the approval `require_approved` returned in the
/// same transaction (an [`ApprovedAction`] cannot be built anywhere else).
pub fn approval_evidence(approved: &ApprovedAction) -> ApprovalEvidence {
    let a = &approved.approval;
    ApprovalEvidence {
        approval_id: a.approval_id,
        project_id: a.project_id,
        action_type: a.action_type.as_str().to_owned(),
        approval_status: a.approval_status.as_str().to_owned(),
        action_payload: a.action_payload.clone(),
        expires_at: a.expires_at.as_datetime(),
    }
}

/// `PURCHASE` / `FINANCIAL_COMMITMENT` as the server's `ActionType`.
pub fn action_type(a: ApprovalAction) -> ActionType {
    match a {
        ApprovalAction::Purchase => ActionType::Purchase,
        ApprovalAction::FinancialCommitment => ActionType::FinancialCommitment,
    }
}

// ---- Plan inputs -------------------------------------------------------------------------------------

/// The stored inputs of a property's plan, and how they were chosen.
#[derive(Debug, Clone)]
pub struct PlanInputs {
    pub scenario: CostScenario,
    pub bom: Option<BillOfMaterials>,
    pub live_scenarios: usize,
    pub superseded_scenarios: usize,
    pub boms_for_property: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BomInputView {
    pub bom_id: Uuid,
    pub design_version_id: Uuid,
    pub status: String,
    pub estimated_total_minor: i64,
    pub approved_total_minor: Option<i64>,
}

/// Which stored rows the plan was built from.
#[derive(Debug, Clone, Serialize)]
pub struct PlanInputsView {
    pub scenario_id: Uuid,
    pub comparison_horizon_months: i64,
    pub scenario_calculated_at: UtcDateTime,
    pub scenario_rule: &'static str,
    pub live_scenarios: usize,
    pub superseded_scenarios_ignored: usize,
    pub bill_of_materials: Option<BomInputView>,
    pub bills_of_materials_for_property: usize,
    pub bom_rule: &'static str,
}

impl PlanInputs {
    pub fn view(&self) -> PlanInputsView {
        PlanInputsView {
            scenario_id: self.scenario.scenario_id,
            comparison_horizon_months: self.scenario.comparison_horizon_months,
            scenario_calculated_at: self.scenario.calculated_at,
            scenario_rule: SCENARIO_RULE,
            live_scenarios: self.live_scenarios,
            superseded_scenarios_ignored: self.superseded_scenarios,
            bill_of_materials: self.bom.as_ref().map(|b| BomInputView {
                bom_id: b.bom_id,
                design_version_id: b.design_version_id,
                status: b.status.as_str().to_owned(),
                estimated_total_minor: b.estimated_total,
                approved_total_minor: b.approved_total,
            }),
            bills_of_materials_for_property: self.boms_for_property,
            bom_rule: BOM_RULE,
        }
    }
}

/// The property's live cost scenario (the latest when several are live) and its latest bill of
/// materials. The property must already have been checked to belong to the project.
pub async fn plan_inputs(
    tx: &mut dyn Tx,
    project_id: Uuid,
    property_id: Uuid,
) -> AppResult<PlanInputs> {
    let all: Vec<CostScenario> = COST_SCENARIOS
        .list(tx, Query::new().eq("property_id", property_id))
        .await?
        .into_iter()
        .filter(|s| s.project_id == project_id)
        .collect();
    let mut live: Vec<CostScenario> = superseded::live_for_property::<CostScenario>(tx, property_id)
        .await?
        .into_iter()
        .filter(|s| s.project_id == project_id)
        .collect();
    let live_scenarios = live.len();
    let Some(scenario) = live.pop() else {
        return Err(AppError::not_found(
            "this property has no live cost scenario yet; analyze it first",
        )
        .code("COST_SCENARIO_REQUIRED"));
    };
    let mut boms: Vec<BillOfMaterials> = BOMS
        .list(tx, Query::new().eq("project_id", project_id))
        .await?
        .into_iter()
        .filter(|b| b.property_id == property_id)
        .collect();
    let boms_for_property = boms.len();
    Ok(PlanInputs {
        superseded_scenarios: all.len().saturating_sub(live_scenarios),
        scenario,
        bom: boms.pop(),
        live_scenarios,
        boms_for_property,
    })
}

pub fn bom_totals(b: &BillOfMaterials) -> BomTotals {
    BomTotals {
        bom_id: b.bom_id,
        project_id: b.project_id,
        property_id: b.property_id,
        design_version_id: b.design_version_id,
        currency: b.currency.as_str().to_owned(),
        estimated_total_minor: b.estimated_total,
        approved_total_minor: b.approved_total,
        status: b.status.as_str().to_owned(),
    }
}

pub fn plan_error(e: PlanError) -> AppError {
    match e {
        PlanError::UnknownItem(item) => AppError::not_found(format!(
            "plan item {item} is not a simulated payment of this property's move-in plan"
        )),
        PlanError::MissingTarget(_) => {
            AppError::conflict(e.to_string()).code("NESSIE_TARGET_MISSING")
        }
        other => AppError::conflict(format!("the move-in plan cannot be built: {other}"))
            .code("NESSIE_PLAN_UNAVAILABLE"),
    }
}

/// T1: the plan always targets the bank this server uses (`bank.simulation()`).
pub fn build_plan(state: &NessieState, inputs: &PlanInputs) -> AppResult<MoveInPaymentPlan> {
    let bom = inputs.bom.as_ref().map(bom_totals);
    plan_move_in_payments(&inputs.scenario, bom.as_ref(), state.simulation(), None).map_err(plan_error)
}

// ---- Views (every one carries the crate's label at the top level) -----------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PlanView {
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    pub inputs: PlanInputsView,
    pub plan: MoveInPaymentPlan,
}

impl PlanView {
    pub fn new(inputs: &PlanInputs, plan: MoveInPaymentPlan) -> Self {
        Self {
            simulation: plan.simulation,
            disclaimer: plan.disclaimer,
            inputs: inputs.view(),
            plan,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MerchantIds {
    pub landlord: Option<String>,
    pub contractor: Option<String>,
    pub furnishing_supplier: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SandboxView {
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    pub project_id: Uuid,
    pub sandbox_status: String,
    /// True when this request created it.
    pub created: bool,
    pub customer_id: Option<String>,
    pub account_id: Option<String>,
    pub merchants: MerchantIds,
    pub opening_balance_minor: i64,
    pub opening_balance_source: String,
    pub opening_balance_note: String,
    pub set_up_at: UtcDateTime,
    /// The account as the bank reports it now.
    pub account: BankRecord<Account>,
    pub notes: Vec<String>,
}

fn opening_note(sim: Simulation, minor: i64, source: &str) -> String {
    let bank = match sim {
        Simulation::NessieSandbox => "the Capital One Nessie sandbox account",
        Simulation::FixtureBank => "the fixture test bank account",
    };
    let what = if source == SOURCE_PROFILE {
        "the opening capital the owner reported in the business profile"
    } else {
        "the figure the owner entered when setting up the sandbox"
    };
    format!(
        "The opening balance of {} is {what}, entered into {bank} as mock money. It is not a real balance and says nothing about the owner's real funds.",
        format_usd(minor)
    )
}

const SOURCE_PROFILE: &str = "BUSINESS_PROFILE_OPENING_CAPITAL";
const SOURCE_REQUEST: &str = "SET_UP_REQUEST";

fn sandbox_view(
    project_id: Uuid,
    sb: &SandboxData,
    created: bool,
    account: BankRecord<Account>,
) -> SandboxView {
    SandboxView {
        simulation: account.simulation,
        disclaimer: account.disclaimer,
        project_id,
        sandbox_status: sb.status.clone(),
        created,
        customer_id: sb.customer_id.clone(),
        account_id: sb.account_id.clone(),
        merchants: MerchantIds {
            landlord: sb.landlord_merchant_id.clone(),
            contractor: sb.contractor_merchant_id.clone(),
            furnishing_supplier: sb.supplier_merchant_id.clone(),
        },
        opening_balance_minor: sb.opening_balance_minor,
        opening_balance_source: sb.opening_balance_source.clone(),
        opening_balance_note: opening_note(account.simulation, sb.opening_balance_minor, &sb.opening_balance_source),
        set_up_at: sb.set_up_at,
        notes: vec![
            "The landlord, contractor and furnishing supplier are stand-in sandbox merchants, not the real parties.".into(),
            "Second Site stores these ids for this project; it never looks sandbox objects up in Nessie by name.".into(),
            "Setting up the sandbox pays nobody, so it needs no approval; every simulated payment does.".into(),
        ],
        account,
    }
}

// ---- Sandbox set-up and read ------------------------------------------------------------------------

/// Outcome of a set-up, decided inside the transaction and committed before the answer.
pub enum SetUp {
    Created(Box<SandboxView>),
    Existing(Box<SandboxView>),
    /// Set-up stopped part way: what was created is recorded, the answer is 502.
    Incomplete(AppError),
}

struct Created {
    customer_id: Option<String>,
    account: Option<BankRecord<Account>>,
    merchants: [Option<String>; 3],
    error: Option<NessieError>,
}

fn create_sandbox_objects(bank: &dyn NessieBank, project_id: Uuid, opening: i64) -> Created {
    let mut out = Created {
        customer_id: None,
        account: None,
        merchants: [None, None, None],
        error: None,
    };
    let customer = CustomerCreate {
        first_name: "SIMULATED".into(),
        last_name: "SECOND SITE OWNER (stand-in)".into(),
        address: Address {
            street_number: "1".into(),
            street_name: "Simulation Way (Second Site stand-in)".into(),
            city: "Los Angeles".into(),
            state: "CA".into(),
            zip: "90028".into(),
        },
    };
    let c = match bank.create_customer(&customer) {
        Ok(c) => c,
        Err(e) => {
            out.error = Some(e);
            return out;
        }
    };
    out.customer_id = Some(c.record.id.clone());
    let short: String = project_id.simple().to_string().chars().take(8).collect();
    let account = AccountCreate {
        account_type: AccountType::Checking,
        nickname: format!("SIMULATION Second Site {short}"),
        balance_cents: opening,
        credit_limit_cents: None,
        rewards: None,
    };
    match bank.create_account(&c.record.id, &account) {
        Ok(a) => out.account = Some(a),
        Err(e) => {
            out.error = Some(e);
            return out;
        }
    }
    for (i, role) in [
        PayeeRole::Landlord,
        PayeeRole::Contractor,
        PayeeRole::FurnishingSupplier,
    ]
    .into_iter()
    .enumerate()
    {
        match bank.create_merchant(&stand_in_merchant(role)) {
            Ok(m) => out.merchants[i] = Some(m.record.id),
            Err(e) => {
                out.error = Some(e);
                return out;
            }
        }
    }
    out
}

/// `POST /nessie/sandbox`: once per project (per bank). `opening` is the request's balance, or
/// `None` for the business profile's opening capital.
pub async fn set_up_sandbox(app: &App, project_id: Uuid, requested: Option<i64>) -> AppResult<SetUp> {
    let state = app.nessie();
    let bank = state.bank()?;
    let out = crate::in_tx!(app, |tx| {
        tx.advisory_xact_lock(sandbox_lock_key(project_id)).await?;
        let existing = sandbox_record(tx, project_id).await?;
        let mut replace_reason: Option<String> = None;
        if let Some(rec) = &existing {
            let sb = rec
                .sandbox
                .clone()
                .ok_or_else(|| AppError::internal("sandbox record without sandbox data"))?;
            match sandbox_unusable(&state, &sb) {
                Some(reason) => replace_reason = Some(reason),
                None => {
                    // Never present stored ids as a live sandbox without asking the bank.
                    let account_id = sb.account_id.clone().unwrap_or_default();
                    match bank_call(bank.clone(), move |b| b.get_account(&account_id)).await? {
                        Ok(account) => {
                            return Ok(SetUp::Existing(Box::new(sandbox_view(
                                project_id, &sb, false, account,
                            ))));
                        }
                        Err(e) if is_not_found(&e) => {
                            replace_reason = Some(
                                "the bank no longer knows the stored sandbox account".into(),
                            )
                        }
                        Err(e) => return Err(bank_error(&e)),
                    }
                }
            }
        }
        let (opening, source) = match requested {
            Some(v) => (v, SOURCE_REQUEST),
            None => {
                let profile =
                    crate::workflows::property_analysis::current_profile(tx, project_id).await?;
                match profile.opening_capital {
                    Some(v) => (v, SOURCE_PROFILE),
                    None => {
                        return Err(AppError::request_validation(
                            "missing",
                            vec![json!("body"), json!("opening_balance_minor")],
                            "the business profile reports no opening capital; give opening_balance_minor",
                        ))
                    }
                }
            }
        };
        if !(0..=MAX_OPENING_BALANCE_MINOR).contains(&opening) {
            return Err(AppError::request_validation(
                "less_than_equal",
                vec![json!("body"), json!("opening_balance_minor")],
                format!(
                    "the opening balance must be 0 to {MAX_OPENING_BALANCE_MINOR} cents of mock money, got {opening}"
                ),
            ));
        }
        let made = bank_call(bank.clone(), move |b| {
            Ok::<_, NessieError>(create_sandbox_objects(b, project_id, opening))
        })
        .await?
        .map_err(|e| bank_error(&e))?;
        let now = utcnow();
        let mut replaced = existing
            .as_ref()
            .and_then(|r| r.sandbox.as_ref())
            .map(|s| s.replaced.clone())
            .unwrap_or_default();
        if let (Some(old), Some(reason)) = (
            existing.as_ref().and_then(|r| r.sandbox.as_ref()),
            replace_reason,
        ) {
            replaced.push(ReplacedSandbox {
                simulation: old.simulation.clone(),
                customer_id: old.customer_id.clone(),
                account_id: old.account_id.clone(),
                reason,
                replaced_at: now,
            });
        }
        let [landlord, contractor, supplier] = made.merchants.clone();
        let sb = SandboxData {
            status: if made.error.is_none() {
                SANDBOX_READY
            } else {
                SANDBOX_INCOMPLETE
            }
            .into(),
            simulation: state.simulation().as_str().to_owned(),
            bank_ref: state.bank_ref().to_owned(),
            customer_id: made.customer_id.clone(),
            account_id: made.account.as_ref().map(|a| a.record.id.clone()),
            landlord_merchant_id: landlord,
            contractor_merchant_id: contractor,
            supplier_merchant_id: supplier,
            opening_balance_minor: opening,
            opening_balance_source: source.into(),
            failure: made.error.as_ref().map(|e| e.to_string()),
            replaced,
            set_up_at: now,
        };
        let rec = NessieSimulationRecord {
            record_id: existing
                .as_ref()
                .map_or_else(ss_core::new_id, |r| r.record_id),
            project_id,
            kind: KIND_SANDBOX.into(),
            record_key: SANDBOX_KEY.into(),
            approval_id: None,
            sandbox: Some(sb.clone()),
            payment: None,
            created_at: existing.as_ref().map_or(now, |r| r.created_at),
            updated_at: now,
        };
        rec.validate()?;
        NESSIE_RECORDS.upsert(tx, &rec).await?;
        match (made.error, made.account) {
            (None, Some(account)) => {
                SetUp::Created(Box::new(sandbox_view(project_id, &sb, true, account)))
            }
            (error, _) => {
                let message = error.as_ref().map_or_else(String::new, |e| e.to_string());
                SetUp::Incomplete(
                    AppError::provider_unavailable(format!(
                        "the sandbox set-up stopped part way ({message}); what was created is recorded and unused. POST /nessie/sandbox again to set up a fresh sandbox. Mock money only."
                    ))
                    .code("NESSIE_SANDBOX_INCOMPLETE")
                    .details(json!({
                        "simulation": state.simulation(),
                        "disclaimer": state.simulation().disclaimer(),
                        "customer_id": sb.customer_id,
                        "account_id": sb.account_id,
                        "http_status": error.as_ref().and_then(NessieError::http_status),
                    }))
                    .with_retryable(true),
                )
            }
        }
    });
    Ok(out)
}

/// `GET /nessie/sandbox`: the stored sandbox, checked with the bank (no transaction is open
/// during the bank call). 404 when none was set up, or when it is gone.
pub async fn read_sandbox(app: &App, project_id: Uuid) -> AppResult<SandboxView> {
    let state = app.nessie();
    let bank = state.bank()?;
    let rec = crate::in_tx!(app, |tx| { sandbox_record(tx, project_id).await? });
    let Some(rec) = rec else {
        return Err(AppError::not_found("no Nessie sandbox is set up for this project")
            .code("NESSIE_SANDBOX_NOT_SET_UP"));
    };
    let sb = rec
        .sandbox
        .clone()
        .ok_or_else(|| AppError::internal("sandbox record without sandbox data"))?;
    let gone = |reason: String| {
        AppError::not_found(format!(
            "the project's Nessie sandbox is gone: {reason}; POST /nessie/sandbox to set it up again"
        ))
        .code("NESSIE_SANDBOX_GONE")
        .details(json!({"reason": reason}))
    };
    if let Some(reason) = sandbox_unusable(&state, &sb) {
        return Err(gone(reason));
    }
    let account_id = sb.account_id.clone().unwrap_or_default();
    match bank_call(bank, move |b| b.get_account(&account_id)).await? {
        Ok(account) => Ok(sandbox_view(project_id, &sb, false, account)),
        Err(e) if is_not_found(&e) => Err(gone(
            "the bank no longer knows the stored sandbox account".into(),
        )),
        Err(e) => Err(bank_error(&e)),
    }
}

/// The project's usable sandbox and its account as the bank reports it now (the bank call runs
/// after the read transaction ended).
async fn sandbox_with_account(
    state: &NessieState,
    rec: Option<NessieSimulationRecord>,
) -> AppResult<(NessieSimulationRecord, SandboxData, BankRecord<Account>)> {
    let (rec, sb) = usable_sandbox(state, rec)?;
    let bank = state.bank()?;
    let account_id = sb.account_id.clone().unwrap_or_default();
    match bank_call(bank, move |b| b.get_account(&account_id)).await? {
        Ok(account) => Ok((rec, sb, account)),
        Err(e) if is_not_found(&e) => Err(AppError::conflict(
            "the project's Nessie sandbox is gone: the bank no longer knows the stored sandbox account; POST /nessie/sandbox to set it up again",
        )
        .code("NESSIE_SANDBOX_GONE")
        .details(json!({"reason": "the bank no longer knows the stored sandbox account"}))),
        Err(e) => Err(bank_error(&e)),
    }
}

// ---- Plan and affordability ----------------------------------------------------------------------------

pub async fn plan_view(app: &App, project_id: Uuid, property_id: Uuid) -> AppResult<PlanView> {
    let state = app.nessie();
    state.bank()?;
    let inputs = crate::in_tx!(app, |tx| { plan_inputs(tx, project_id, property_id).await? });
    let plan = build_plan(&state, &inputs)?;
    Ok(PlanView::new(&inputs, plan))
}

#[derive(Debug, Clone, Serialize)]
pub struct AffordabilityView {
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    pub inputs: PlanInputsView,
    /// `BUSINESS_PROFILE` or `NONE`.
    pub working_capital_reserve_source: &'static str,
    pub report: AffordabilityReport,
}

pub async fn affordability_view(
    app: &App,
    project_id: Uuid,
    property_id: Uuid,
) -> AppResult<AffordabilityView> {
    let state = app.nessie();
    state.bank()?;
    let (inputs, rec, reserve) = crate::in_tx!(app, |tx| {
        let inputs = plan_inputs(tx, project_id, property_id).await?;
        let rec = sandbox_record(tx, project_id).await?;
        let reserve =
            match crate::workflows::property_analysis::current_profile(tx, project_id).await {
                Ok(p) => p.working_capital_reserve,
                Err(e) if e.error_code() == "PROFILE_REQUIRED" => None,
                Err(e) => return Err(e),
            };
        (inputs, rec, reserve)
    });
    let plan = build_plan(&state, &inputs)?;
    let (_, _, account) = sandbox_with_account(&state, rec).await?;
    let report =
        ss_nessie::check_affordability(&account, &plan, &inputs.scenario, reserve).map_err(plan_error)?;
    Ok(AffordabilityView {
        simulation: report.simulation,
        disclaimer: report.disclaimer,
        inputs: inputs.view(),
        working_capital_reserve_source: if reserve.is_some() {
            "BUSINESS_PROFILE"
        } else {
            "NONE"
        },
        report,
    })
}

// ---- Approval request -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalView {
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    /// False when an open approval for exactly this payment already existed.
    pub created: bool,
    pub property_id: Uuid,
    pub plan_item_id: String,
    pub attempt_id: String,
    pub approval: ApprovalRequest,
    pub next_step: String,
}

/// The payload without the attempt id (and the description that names it): the part that
/// decides whether two approvals are for the same payment.
pub fn base_payload_hash(payload: &JsonMap) -> String {
    let mut base = payload.clone();
    base.remove("attempt_id");
    base.remove("description");
    payload_hash(&base)
}

/// The attempt id of `round` for a base payload: the first 12 hex characters of
/// `SHA-256("second-site:nessie-attempt:{base_hash}:{round}")`. Deterministic, so a repeated
/// request finds the same approval; the round moves on when an approval was rejected or expired.
pub fn attempt_id_for(base_hash: &str, round: u32) -> String {
    let d = hex::encode(Sha256::digest(format!(
        "second-site:nessie-attempt:{base_hash}:{round}"
    )));
    d.chars().take(12).collect()
}

fn is_dead(a: &ApprovalRequest) -> bool {
    match a.approval_status {
        ApprovalStatus::Rejected | ApprovalStatus::Cancelled | ApprovalStatus::Expired => true,
        ApprovalStatus::Pending | ApprovalStatus::Approved => a.expires_at < UtcDateTime::now(),
        ApprovalStatus::Executed => false,
    }
}

/// Refuses (409) when a non-refused attempt of the plan item exists (D2).
fn refuse_if_attempted(existing: &Option<NessieSimulationRecord>) -> AppResult<()> {
    if let Some(p) = existing.as_ref().and_then(|r| r.payment.as_ref()) {
        if p.attempt_status != ATTEMPT_REFUSED {
            return Err(AppError::conflict(format!(
                "plan item {} already has a simulated payment attempt ({}); only one per plan item is made",
                p.plan_item_id, p.attempt_status
            ))
            .code("NESSIE_ALREADY_ATTEMPTED")
            .details(json!({
                "attempt_status": p.attempt_status,
                "approval_id": p.approval_id,
                "attempt_id": p.attempt_id,
                "outcome": p.outcome,
            })));
        }
    }
    Ok(())
}

/// The recurring bill the first month's payment pays: from the bill item's stored attempt.
async fn occupancy_bill_id(
    tx: &mut dyn Tx,
    project_id: Uuid,
    property_id: Uuid,
    plan_item_id: &str,
) -> AppResult<Option<String>> {
    if plan_item_id != ITEM_FIRST_MONTH {
        return Ok(None);
    }
    let bill = attempt_record(tx, project_id, property_id, ITEM_OCCUPANCY_BILL).await?;
    match bill.as_ref().and_then(|r| r.payment.as_ref()) {
        Some(p) if p.attempt_status == ATTEMPT_SENT && p.nessie_bill_id.is_some() => {
            Ok(p.nessie_bill_id.clone())
        }
        _ => Err(AppError::conflict(format!(
            "{ITEM_FIRST_MONTH} pays the recurring bill of {ITEM_OCCUPANCY_BILL}: execute that item first"
        ))
        .code("NESSIE_BILL_REQUIRED")),
    }
}

/// `POST .../payments/{plan_item_id}/approval`.
pub async fn request_approval(
    app: &App,
    project_id: Uuid,
    owner_id: Uuid,
    property_id: Uuid,
    plan_item_id: &str,
) -> AppResult<ApprovalView> {
    let state = app.nessie();
    state.bank()?;
    // Phase 1: the stored inputs, then ask the bank that the sandbox account exists (no
    // transaction is open during that call).
    let item = plan_item_id.to_owned();
    let rec = crate::in_tx!(app, |tx| {
        let inputs = plan_inputs(tx, project_id, property_id).await?;
        let plan = build_plan(&state, &inputs)?;
        if plan.payment(&item).is_none() {
            return Err(plan_error(PlanError::UnknownItem(item.clone())));
        }
        let rec = sandbox_record(tx, project_id).await?;
        usable_sandbox(&state, rec.clone())?;
        occupancy_bill_id(tx, project_id, property_id, &item).await?;
        rec
    });
    let (checked, _, _) = sandbox_with_account(&state, rec).await?;
    // Phase 2: everything again under the plan item's lock, then the approval.
    let view = crate::in_tx!(app, |tx| {
        tx.advisory_xact_lock(plan_item_lock_key(project_id, property_id, &item))
            .await?;
        let (rec, sb) = usable_sandbox(&state, sandbox_record(tx, project_id).await?)?;
        if rec != checked {
            return Err(AppError::conflict(
                "the project's Nessie sandbox changed while the approval was prepared; try again",
            )
            .code("NESSIE_SANDBOX_CHANGED"));
        }
        refuse_if_attempted(&attempt_record(tx, project_id, property_id, &item).await?)?;
        let inputs = plan_inputs(tx, project_id, property_id).await?;
        let plan = build_plan(&state, &inputs)?;
        let bill = occupancy_bill_id(tx, project_id, property_id, &item).await?;
        let targets = targets_of(&sb, bill);
        let probe = plan
            .intent_for(&item, &targets, "00000000")
            .map_err(plan_error)?;
        let base = base_payload_hash(&probe.approval_payload());
        let mut found: Option<(bool, ApprovalRequest, String)> = None;
        for round in 0..1000u32 {
            let attempt_id = attempt_id_for(&base, round);
            let intent = plan
                .intent_for(&item, &targets, &attempt_id)
                .map_err(plan_error)?;
            let payload = intent.approval_payload();
            let key = format!(
                "nessie:{project_id}:{property_id}:{}:{item}:{}",
                plan.scenario_id,
                payload_hash(&payload)
            );
            let existing = APPROVALS
                .list(tx, Query::new().eq("idempotency_key", key.as_str()).limit(1))
                .await?
                .into_iter()
                .next();
            match existing {
                Some(a) if a.project_id != project_id => {
                    return Err(AppError::conflict("idempotency key already used by another project")
                        .code("IDEMPOTENCY_KEY_CONFLICT"))
                }
                Some(a) if is_dead(&a) => continue,
                Some(a) => {
                    found = Some((false, a, attempt_id));
                    break;
                }
                None => {
                    let spec = ApprovalSpec::new(
                        project_id,
                        owner_id,
                        APPROVAL_AGENT,
                        action_type(intent.action),
                        payload,
                        intent.summary(),
                        key,
                    );
                    let a = app.approvals().request(tx, spec).await?;
                    found = Some((true, a, attempt_id));
                    break;
                }
            }
        }
        let Some((created, approval, attempt_id)) = found else {
            return Err(AppError::conflict(
                "too many rejected or expired approvals for this plan item",
            ));
        };
        ApprovalView {
            simulation: plan.simulation,
            disclaimer: plan.disclaimer,
            created,
            property_id,
            plan_item_id: item.clone(),
            next_step: format!(
                "Decide it with POST /api/v1/projects/{project_id}/approvals/{}/decision (decision APPROVED or REJECTED, action_payload_hash {}), then POST .../payments/{item}/execute with this approval_id. Simulation only: nothing real is paid or committed.",
                approval.approval_id, approval.action_payload_hash
            ),
            attempt_id,
            approval,
        }
    });
    Ok(view)
}

// ---- Execute ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ExecuteView {
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    /// [`ATTEMPT_SENT`]: the bank answered with the approved object; `record.outcome` says what
    /// it reported (a pending payment is pending, not completed).
    pub attempt_status: &'static str,
    pub approval_id: Uuid,
    pub approval_status: String,
    pub record: SimulatedPaymentRecord,
}

/// Outcome of an execute, committed before the answer (D1).
pub enum Executed {
    Sent(Box<ExecuteView>),
    /// 422: refused by the bank; the approval is not spent.
    Refused(AppError),
    /// 502: may have happened; the approval is spent.
    MayExist(AppError),
}

fn failure_of(outcome: AttemptOutcome, e: &NessieError) -> AttemptFailure {
    AttemptFailure {
        outcome: match outcome {
            AttemptOutcome::NotSent => "NOT_SENT",
            AttemptOutcome::Refused => ATTEMPT_REFUSED,
            AttemptOutcome::MayExist => ATTEMPT_MAY_EXIST,
        }
        .into(),
        http_status: e.http_status(),
        problem_slug: e.problem_slug().map(str::to_owned),
        kind: e.kind().map(|k| k.as_str().to_owned()),
        message: e.to_string(),
    }
}

fn authorization_error(e: AuthorizationError) -> AppError {
    let code = match e {
        AuthorizationError::PayloadMismatch => "APPROVAL_PAYLOAD_MISMATCH",
        AuthorizationError::WrongBank { .. } => "NESSIE_WRONG_BANK",
        _ => "APPROVAL_REQUIRED",
    };
    AppError::approval_required(e.to_string()).code(code)
}

/// `POST .../payments/{plan_item_id}/execute`, in one transaction: the plan item's lock, the
/// D2 check, `require_approved` for the payload rebuilt now from the current plan, the crate's
/// check, the claim, the bank call, the recorded outcome and `mark_executed`; then commit.
pub async fn execute_payment(
    app: &App,
    project_id: Uuid,
    property_id: Uuid,
    plan_item_id: &str,
    approval_id: Uuid,
) -> AppResult<Executed> {
    let state = app.nessie();
    let bank = state.bank()?;
    let item = plan_item_id.to_owned();
    let out = crate::in_tx!(app, |tx| {
        let inputs = plan_inputs(tx, project_id, property_id).await?;
        let plan = build_plan(&state, &inputs)?;
        let planned = plan
            .payment(&item)
            .cloned()
            .ok_or_else(|| plan_error(PlanError::UnknownItem(item.clone())))?;
        let (sandbox_rec, sb) = usable_sandbox(&state, sandbox_record(tx, project_id).await?)?;
        let bill = occupancy_bill_id(tx, project_id, property_id, &item).await?;
        tx.advisory_xact_lock(plan_item_lock_key(project_id, property_id, &item))
            .await?;
        let existing = attempt_record(tx, project_id, property_id, &item).await?;
        refuse_if_attempted(&existing)?;
        // The approval's attempt id: the only value taken from the stored approval; everything
        // else in the payload is rebuilt now from the current plan and sandbox.
        let stored = APPROVALS
            .require_in_project(tx, approval_id, project_id, &format!("ApprovalRequest {approval_id} not found"))
            .await?;
        let attempt_id = stored
            .action_payload
            .get("attempt_id")
            .and_then(Value::as_str)
            .filter(|a| validate_attempt_id(a).is_ok())
            .map(str::to_owned)
            .ok_or_else(|| {
                AppError::approval_required("approval is not for a Nessie simulated payment")
                    .code("APPROVAL_PAYLOAD_MISMATCH")
            })?;
        let intent = plan
            .intent_for(&item, &targets_of(&sb, bill), &attempt_id)
            .map_err(plan_error)?;
        let payload = intent.approval_payload();
        let approved = app
            .approvals()
            .require_approved(
                tx,
                project_id,
                Some(approval_id),
                action_type(planned.approval_action),
                &payload,
            )
            .await?;
        let evidence = approval_evidence(&approved);
        let auth = PaymentAuthorization::verify(&evidence, intent.clone(), chrono::Utc::now())
            .map_err(authorization_error)?;
        let now = utcnow();
        let earlier = existing
            .as_ref()
            .and_then(|r| r.payment.as_ref())
            .map(|p| {
                let mut list = p.earlier_refusals.clone();
                if let Some(f) = &p.failure {
                    list.push(EarlierRefusal {
                        approval_id: p.approval_id,
                        attempt_id: p.attempt_id.clone(),
                        failure: f.clone(),
                        attempted_at: p.attempted_at,
                    });
                }
                list
            })
            .unwrap_or_default();
        let mut rec = NessieSimulationRecord {
            record_id: existing.as_ref().map_or_else(ss_core::new_id, |r| r.record_id),
            project_id,
            kind: KIND_PAYMENT.into(),
            record_key: attempt_key(property_id, &item),
            approval_id: Some(approval_id),
            sandbox: None,
            payment: Some(PaymentAttemptData {
                attempt_status: ATTEMPT_IN_FLIGHT.into(),
                simulation: intent.simulation.as_str().to_owned(),
                bank_ref: state.bank_ref().to_owned(),
                sandbox_record_id: sandbox_rec.record_id,
                property_id,
                scenario_id: plan.scenario_id,
                plan_item_id: item.clone(),
                attempt_id: attempt_id.clone(),
                approval_id,
                operation: intent.operation.name().to_owned(),
                amount_minor: intent.amount_minor,
                outcome: None,
                provider_status: None,
                nessie_transaction_id: None,
                nessie_bill_id: None,
                record: None,
                failure: None,
                earlier_refusals: earlier,
                attempted_at: now,
                refreshed_at: None,
            }),
            created_at: existing.as_ref().map_or(now, |r| r.created_at),
            updated_at: now,
        };
        rec.validate()?;
        // The claim, before the bank call.
        NESSIE_RECORDS.upsert(tx, &rec).await?;
        let paying = bank.clone();
        let result = crate::integrations::blocking(move || paying.execute(auth)).await?;
        let label = json!({
            "simulation": intent.simulation,
            "disclaimer": intent.simulation.disclaimer(),
            "approval_id": approval_id,
            "plan_item_id": item,
            "attempt_id": attempt_id,
        });
        let mut p = rec
            .payment
            .take()
            .ok_or_else(|| AppError::internal("attempt record without payment data"))?;
        let executed = match result {
            Ok(record) => {
                p.attempt_status = ATTEMPT_SENT.into();
                p.outcome = Some(record.outcome.as_str().to_owned());
                p.provider_status = Some(record.provider_status.clone());
                p.nessie_transaction_id = record.nessie_transaction_id.clone();
                p.nessie_bill_id = record.nessie_bill_id.clone();
                p.record = Some(StoredPaymentRecord::of(&record));
                rec.payment = Some(p);
                rec.updated_at = utcnow();
                NESSIE_RECORDS.upsert(tx, &rec).await?;
                let spent = app.approvals().mark_executed(tx, approved).await?;
                Executed::Sent(Box::new(ExecuteView {
                    simulation: record.simulation,
                    disclaimer: record.disclaimer,
                    attempt_status: ATTEMPT_SENT,
                    approval_id,
                    approval_status: spent.approval_status.as_str().to_owned(),
                    record,
                }))
            }
            Err(e) if e.outcome == AttemptOutcome::NotSent => {
                // Nothing reached the bank: roll back the claim; the approval is unchanged.
                return Err(AppError::conflict(format!(
                    "the simulated payment was not sent: {}",
                    e.error
                ))
                .code("NESSIE_NOT_SENT")
                .details(label));
            }
            Err(e) if e.outcome == AttemptOutcome::Refused => {
                let failure = failure_of(e.outcome, &e.error);
                p.attempt_status = ATTEMPT_REFUSED.into();
                p.failure = Some(failure.clone());
                rec.payment = Some(p);
                rec.updated_at = utcnow();
                NESSIE_RECORDS.upsert(tx, &rec).await?;
                let mut details = label;
                if let Some(d) = details.as_object_mut() {
                    d.insert("attempt_status".into(), json!(ATTEMPT_REFUSED));
                    d.insert("approval_status".into(), json!("APPROVED"));
                    d.insert("http_status".into(), json!(failure.http_status));
                    d.insert("problem_slug".into(), json!(failure.problem_slug));
                }
                Executed::Refused(
                    AppError::conflict(format!(
                        "{} refused the simulated payment{}: nothing was created, and the approval is not spent (fix the cause, then execute it again). {}",
                        bank_name(intent.simulation),
                        failure
                            .problem_slug
                            .as_deref()
                            .map(|s| format!(" ({s})"))
                            .unwrap_or_default(),
                        e.error
                    ))
                    .code("NESSIE_PAYMENT_REFUSED")
                    .details(details)
                    .with_retryable(false),
                )
            }
            Err(e) => {
                let failure = failure_of(e.outcome, &e.error);
                p.attempt_status = ATTEMPT_MAY_EXIST.into();
                p.failure = Some(failure.clone());
                rec.payment = Some(p);
                rec.updated_at = utcnow();
                NESSIE_RECORDS.upsert(tx, &rec).await?;
                let spent = app.approvals().mark_executed(tx, approved).await?;
                let mut details = label;
                if let Some(d) = details.as_object_mut() {
                    d.insert("attempt_status".into(), json!(ATTEMPT_MAY_EXIST));
                    d.insert("approval_status".into(), json!(spent.approval_status.as_str()));
                    d.insert("http_status".into(), json!(failure.http_status));
                    d.insert("kind".into(), json!(failure.kind));
                }
                Executed::MayExist(
                    AppError::provider_unavailable(format!(
                        "The simulated payment may have been made in {} (mock money): {}. The approval is spent. Check the sandbox account's transactions (the description names plan item {} and attempt {}) before trying again; it will not be sent twice.",
                        bank_name(intent.simulation),
                        e.error,
                        intent.plan_item_id,
                        intent.attempt_id
                    ))
                    .code("NESSIE_PAYMENT_MAY_EXIST")
                    .details(details)
                    .with_retryable(false),
                )
            }
        };
        executed
    });
    Ok(out)
}

fn bank_name(sim: Simulation) -> &'static str {
    match sim {
        Simulation::NessieSandbox => "the Capital One Nessie sandbox",
        Simulation::FixtureBank => "the fixture test bank",
    }
}

// ---- Refresh -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct RefreshView {
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    pub attempt_status: String,
    pub record: SimulatedPaymentRecord,
    pub transaction: BankRecord<Transaction>,
}

/// `POST .../payments/{plan_item_id}/refresh`: the stored transaction of this plan item, read
/// again from the bank. The transaction id comes from the stored record, never from the client.
pub async fn refresh_payment(
    app: &App,
    project_id: Uuid,
    property_id: Uuid,
    plan_item_id: &str,
) -> AppResult<RefreshView> {
    let state = app.nessie();
    let bank = state.bank()?;
    let item = plan_item_id.to_owned();
    let (rec, sb) = crate::in_tx!(app, |tx| {
        let rec = attempt_record(tx, project_id, property_id, &item).await?;
        let sandbox = sandbox_record(tx, project_id).await?;
        (rec, sandbox)
    });
    let Some(rec) = rec else {
        return Err(AppError::not_found(format!(
            "plan item {item} has no recorded simulated payment"
        ))
        .code("NESSIE_NO_ATTEMPT"));
    };
    let p = rec
        .payment
        .clone()
        .ok_or_else(|| AppError::internal("attempt record without payment data"))?;
    let (_, sb) = usable_sandbox(&state, sb)?;
    if p.bank_ref != state.bank_ref() {
        return Err(AppError::conflict(
            "this attempt was made in another bank instance than the one this server uses",
        )
        .code("NESSIE_SANDBOX_GONE"));
    }
    let stored = match (&p.record, &p.nessie_transaction_id) {
        (Some(r), Some(_)) if p.attempt_status == ATTEMPT_SENT => r.restore()?,
        _ => {
            let why = match p.attempt_status.as_str() {
                ATTEMPT_MAY_EXIST => "its outcome is unknown (MAY_EXIST): check the sandbox account's transactions",
                ATTEMPT_REFUSED => "the bank refused it, so nothing exists",
                _ => "it created a recurring bill, not a transaction",
            };
            return Err(AppError::conflict(format!(
                "plan item {item} has no transaction to refresh: {why}"
            ))
            .code("NESSIE_NOTHING_TO_REFRESH")
            .details(json!({"attempt_status": p.attempt_status})));
        }
    };
    let txn_id = p.nessie_transaction_id.clone().unwrap_or_default();
    let txn = match bank_call(bank, move |b| b.get_transaction(&txn_id)).await? {
        Ok(t) => t,
        Err(e) => return Err(bank_error(&e)),
    };
    // Only a transaction of this project's sandbox account.
    if txn.record.payer_id.as_deref() != sb.account_id.as_deref() {
        return Err(AppError::conflict(
            "the transaction does not belong to this project's sandbox account",
        )
        .code("NESSIE_FOREIGN_TRANSACTION"));
    }
    let refreshed = stored.refreshed(&txn).map_err(|e| bank_error(&e))?;
    let attempt_status = p.attempt_status.clone();
    let record_for_store = StoredPaymentRecord::of(&refreshed);
    crate::in_tx!(app, |tx| {
        tx.advisory_xact_lock(plan_item_lock_key(project_id, property_id, &item))
            .await?;
        if let Some(mut current) = attempt_record(tx, project_id, property_id, &item).await? {
            if let Some(cp) = current.payment.as_mut() {
                if cp.approval_id == p.approval_id
                    && cp.nessie_transaction_id == p.nessie_transaction_id
                {
                    cp.outcome = Some(record_for_store.outcome.clone());
                    cp.provider_status = Some(record_for_store.provider_status.clone());
                    cp.record = Some(record_for_store.clone());
                    cp.refreshed_at = Some(utcnow());
                    current.updated_at = utcnow();
                    NESSIE_RECORDS.upsert(tx, &current).await?;
                }
            }
        }
    });
    Ok(RefreshView {
        simulation: refreshed.simulation,
        disclaimer: refreshed.disclaimer,
        attempt_status,
        record: refreshed,
        transaction: txn,
    })
}

/// The stored attempt documents of a project (tests and diagnostics).
pub async fn records_of(tx: &mut dyn Tx, project_id: Uuid) -> AppResult<Vec<NessieSimulationRecord>> {
    NESSIE_RECORDS
        .list(tx, Query::new().eq("project_id", project_id))
        .await
}

/// `{"simulation", "disclaimer"}` of the bank this server uses (for error details).
pub fn label_of(state: &NessieState) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("simulation".into(), json!(state.simulation()));
    m.insert("disclaimer".into(), json!(state.simulation().disclaimer()));
    m
}
