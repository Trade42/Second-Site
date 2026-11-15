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
//! **Single payment per plan item (D2).** Every flow that could lead to a payment takes the
//! advisory lock [`plan_item_lock_key`] and refuses (409 `NESSIE_ALREADY_ATTEMPTED`, without
//! reaching the bank and without waiting for a bank call) while the plan item's attempt row exists
//! and was not refused by the bank: `IN_FLIGHT` (being sent), `SENT` or `MAY_EXIST`. A refusal by
//! the bank marks the row `REFUSED` (kept, with the refusal history) and the next attempt takes it
//! over; any other outcome keeps it.
//!
//! **Execute in two phases (nessie-3, F2).** Phase 1 is one short transaction: every check, then
//! the claim (`IN_FLIGHT`, with the approval id, the attempt id and an [`AttemptClaim`]) is
//! COMMITTED before anything is sent. The bank call then runs with no transaction, row lock,
//! advisory lock or pool connection held. Phase 2 is a second short transaction: the outcome is
//! recorded on the claim and the approval is spent or left usable (D1): a refusal from the crate's
//! refusal list leaves the approval APPROVED and the item payable again; an answer with the
//! approved object is `SENT` and spends it; everything unclear is `MAY_EXIST` and spends it.
//!
//! **Stale claims.** A claim still `IN_FLIGHT` when its call can no longer be running is an
//! unknown outcome: the next request that reads it under the plan item's lock records it
//! `MAY_EXIST` and spends its approval in its own committed transaction, and it is never retried.
//! A claim of this server instance is dead exactly when the instance's in-flight registry no
//! longer holds it (the registry entry lives until the bank call has returned and phase 2 has
//! ended). A claim of another instance (another server process, or this one before a restart) is
//! dead only after its `settle_after` time: the flow never starts the bank call later than
//! `send_by`, the crate's whole-request deadline ends the call by `send_by + timeout_s`, and a
//! margin ([`DEFAULT_CLAIM_MARGIN`]) covers clock differences between servers and DNS time. If
//! phase 2 cannot be written (the database is down after the bank answered, or the process
//! stops), the claim stays `IN_FLIGHT` and so becomes `MAY_EXIST` by this rule. Refresh can turn a
//! `MAY_EXIST` purchase into `SENT` when the sandbox account's transactions show exactly one
//! purchase whose description carries the plan item and attempt id and that matches the approved
//! payment; otherwise it stays `MAY_EXIST`.
//!
//! **Lock order.** Execute phase 1: plan-item advisory lock, then the approval row
//! (`require_approved`, `FOR UPDATE`), then the attempt row (its insert also takes a key-share
//! lock on the project row); commit. Phase 2: plan-item advisory lock, the attempt row, then the
//! approval update (`mark_executed`). A stale-claim settlement: plan-item lock, attempt row,
//! approval row. None emits an event, so none takes the project's event lock. Approval request:
//! plan-item advisory lock, then the approval insert and the `approval.requested` emit (event
//! lock), as every other emitter takes it last. Set-up: the project's sandbox advisory lock
//! ([`sandbox_lock_key`]), the bank calls, then the sandbox row. The approval decision route takes
//! the approval row and then the event lock, so no cycle exists with these orders.
//!
//! **What is held during each bank call.** Execute: nothing (the claim is committed; the in-flight
//! registry entry is process memory). Set-up (unchanged, a one-transaction flow): the request
//! transaction (one pool connection) and the project's sandbox advisory lock, which only another
//! set-up of the same project waits for; with the in-memory store that transaction holds the
//! store's global lock, so every other request waits for a set-up's bank calls. A crash during a
//! set-up leaves the sandbox row as it was (or absent) and the objects already created at the bank
//! unrecorded and unused; the next set-up creates new ones. Plan, affordability, sandbox reads, the
//! approval request's account check and refresh make their bank calls with no transaction open.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use ss_core::{utcnow, Checker, Contract, IntegrationStatus, UtcDateTime, Validate};
use ss_domain::contracts::{BillOfMaterials, CostScenario, ProviderStatusEntry};
use ss_domain::integrations::base::{status_entry, ProviderErrorKind};
use ss_nessie::approval::{description_prefix, validate_attempt_id};
use ss_nessie::config::{ENV_API_KEY, ENV_BASE_URL, ENV_MODE};
use ss_nessie::models::{
    Account, AccountCreate, AccountType, Address, CustomerCreate, Transaction, TransactionKind,
};
use ss_nessie::money::format_usd;
use ss_nessie::plan::{stand_in_merchant, ITEM_FIRST_MONTH, ITEM_OCCUPANCY_BILL};
use ss_nessie::{
    plan_move_in_payments, select_bank, AffordabilityReport, ApprovalAction, ApprovalEvidence,
    AttemptOutcome, AuthorizationError, BankRecord, BomTotals, FixtureBank, MoveInPaymentPlan,
    NessieBank, NessieConfig, NessieError, PayeeRole, PaymentAuthorization, PaymentError,
    PaymentIntent, PlanError, SandboxTargets, SimulatedOutcome, SimulatedPaymentRecord, Simulation,
};
use uuid::Uuid;

use crate::app::{App, ProviderStatus};
use crate::config::Settings;
use crate::contracts::{payload_hash, ActionType, ApprovalRequest, ApprovalStatus, JsonMap};
use crate::error::{AppError, AppResult};
use crate::logging;
use crate::services::approvals::{ApprovalSpec, ApprovedAction};
use crate::services::design::lock_key;
use crate::store::repo::{
    superseded, Document, ProjectScoped, Repo, APPROVALS, BOMS, COST_SCENARIOS,
};
use crate::store::schema::{self, TableDef};
use crate::store::{Filter, Query, Row, SqlValue, Tx};

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

/// Claimed (committed in phase 1) for a bank call that is in progress, or whose outcome was not
/// recorded; a claim whose call can no longer be running is settled as [`ATTEMPT_MAY_EXIST`].
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

/// `outcome_source`: the outcome is the bank's answer, recorded in phase 2.
pub const SOURCE_BANK_ANSWER: &str = "BANK_ANSWER";
/// `outcome_source`: the claim's outcome was never recorded (the server stopped, or could not
/// write to the database, between the claim and the outcome); settled as `MAY_EXIST`.
pub const SOURCE_UNRECORDED_CLAIM: &str = "UNRECORDED_CLAIM";
/// `outcome_source`: the bank's answer arrived after another server had settled the claim.
pub const SOURCE_LATE_BANK_ANSWER: &str = "BANK_ANSWER_AFTER_SETTLEMENT";
/// `outcome_source`: a refresh found the payment among the sandbox account's transactions.
pub const SOURCE_REFRESH_FOUND: &str = "REFRESH_FOUND_TRANSACTION";

/// The bank call starts within this time after the claim is stored, or it is not made.
pub const DEFAULT_SEND_WINDOW: Duration = Duration::from_secs(5);
/// Added before another server may treat a claim as dead: clock differences between servers,
/// and DNS resolution, which the client's whole-request deadline does not bound.
pub const DEFAULT_CLAIM_MARGIN: Duration = Duration::from_secs(60);
/// The newest transactions of the sandbox account a refresh reads to settle a `MAY_EXIST`.
pub const REFRESH_SCAN_LIMIT: u32 = 500;

const SCENARIO_RULE: &str = "The property's live (not superseded) cost scenario; when several are live, the latest by created_at. Superseded scenarios are ignored.";
const BOM_RULE: &str = "The property's latest bill of materials by created_at (any design version of this property in this project), or none.";

// ---- The bank this service uses ------------------------------------------------------------------

/// The times a claim carries (see the module documentation, "Stale claims").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimTiming {
    /// The bank call starts within this time after the claim is stored, or it is not made.
    pub send_window: Duration,
    /// The bank's whole-request deadline (`NessieConfig::timeout_s` for the live client).
    pub call_deadline: Duration,
    /// Added before another server may treat a claim as dead.
    pub margin: Duration,
}

impl ClaimTiming {
    /// The defaults for a bank whose whole-request deadline is `call_deadline`.
    pub fn for_deadline(call_deadline: Duration) -> Self {
        Self {
            send_window: DEFAULT_SEND_WINDOW,
            call_deadline,
            margin: DEFAULT_CLAIM_MARGIN,
        }
    }

    /// From the claim until no call for it can still be running, as another server sees it.
    pub fn settle_after(&self) -> Duration {
        self.send_window + self.call_deadline + self.margin
    }

    /// How long a graceful shutdown waits for the flows between their two phases.
    pub fn shutdown_wait(&self) -> Duration {
        self.send_window + self.call_deadline + Duration::from_secs(5)
    }
}

/// The live client's deadline as the crate computes it (`timeout_s`, 20 s when invalid).
fn deadline_of(config: &NessieConfig) -> Duration {
    match Duration::try_from_secs_f64(config.timeout_s) {
        Ok(d) if !d.is_zero() => d,
        _ => Duration::from_secs(20),
    }
}

/// The claims this server instance has between phase 1 and the end of phase 2 (their bank call
/// in progress, or their outcome being recorded).
#[derive(Debug, Default)]
pub struct Flights {
    live: Mutex<HashSet<Uuid>>,
}

impl Flights {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<Uuid>> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Registers `claim_id` until the last clone of the returned guard is dropped.
    fn start(self: &Arc<Self>, claim_id: Uuid) -> Arc<FlightGuard> {
        self.lock().insert(claim_id);
        Arc::new(FlightGuard {
            flights: self.clone(),
            claim_id,
        })
    }

    pub fn contains(&self, claim_id: Uuid) -> bool {
        self.lock().contains(&claim_id)
    }

    /// How many flows are between their two phases.
    pub fn count(&self) -> usize {
        self.lock().len()
    }
}

/// Holds a claim in [`Flights`]; one clone lives in the bank call, one in the flow.
#[derive(Debug)]
pub struct FlightGuard {
    flights: Arc<Flights>,
    claim_id: Uuid,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.flights.lock().remove(&self.claim_id);
    }
}

/// The bank and what identifies it, so stored sandbox ids are only used with the bank that made
/// them; and this server instance's identity and in-flight claims.
pub struct NessieState {
    bank: Arc<dyn NessieBank>,
    /// `fixture:<random per process>` for the fixture bank (it forgets everything on restart and
    /// reuses the same ids), `sandbox:<base URL>` for the Capital One sandbox.
    bank_ref: String,
    config_error: Option<String>,
    /// Random per state (one per process in production): the claims it stores carry it.
    instance_id: Uuid,
    flights: Arc<Flights>,
    timing: ClaimTiming,
}

impl NessieState {
    fn new(
        bank: Arc<dyn NessieBank>,
        bank_ref: String,
        config_error: Option<String>,
        timing: ClaimTiming,
    ) -> Self {
        Self {
            bank,
            bank_ref,
            config_error,
            instance_id: Uuid::new_v4(),
            flights: Arc::new(Flights::default()),
            timing,
        }
    }

    /// `select_bank`: the live client only with `NESSIE_PROVIDER_MODE=live` and a key.
    pub fn from_config(config: &NessieConfig) -> Self {
        let bank = select_bank(config);
        let bank_ref = match bank.simulation() {
            Simulation::FixtureBank => format!("fixture:{}", Uuid::new_v4().simple()),
            Simulation::NessieSandbox => format!("sandbox:{}", config.base_url),
        };
        Self::new(
            bank,
            bank_ref,
            None,
            ClaimTiming::for_deadline(deadline_of(config)),
        )
    }

    /// A fresh in-memory fixture bank.
    pub fn fixture() -> Self {
        Self::from_config(&NessieConfig::default())
    }

    /// A given bank (tests: a live client pointed at a local stub). `bank_ref` names it. The claim
    /// timing assumes the crate's default deadline (20 s); see [`NessieState::with_claim_timing`].
    pub fn with_bank(bank: Arc<dyn NessieBank>, bank_ref: impl Into<String>) -> Self {
        let deadline = deadline_of(&NessieConfig::default());
        Self::new(
            bank,
            bank_ref.into(),
            None,
            ClaimTiming::for_deadline(deadline),
        )
    }

    /// The same state with other claim times (tests, or a bank with another deadline).
    pub fn with_claim_timing(mut self, timing: ClaimTiming) -> Self {
        self.timing = timing;
        self
    }

    /// Invalid `NESSIE_*` settings: `/ready` says BLOCKED and every route answers 503.
    pub fn misconfigured(error: impl Into<String>) -> Self {
        let deadline = deadline_of(&NessieConfig::default());
        Self::new(
            Arc::new(FixtureBank::new()),
            "misconfigured".into(),
            Some(error.into()),
            ClaimTiming::for_deadline(deadline),
        )
    }

    pub fn instance_id(&self) -> Uuid {
        self.instance_id
    }

    pub fn timing(&self) -> ClaimTiming {
        self.timing
    }

    pub fn flights(&self) -> &Arc<Flights> {
        &self.flights
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
        self.state.read().unwrap_or_else(|p| p.into_inner()).clone()
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

/// The crate's configuration from the service's settings: `NESSIE_PROVIDER_MODE`,
/// `NESSIE_API_KEY` and `NESSIE_BASE_URL` come through the settings loader, so they follow the
/// same sources and precedence as every other name (`config.rs`: the process environment, then
/// `../../.env`, then `.env`). The key stays a [`crate::config::Secret`] until the crate wraps it.
pub fn nessie_config(settings: &Settings) -> Result<NessieConfig, String> {
    NessieConfig::from_lookup(|name| match name {
        ENV_MODE => settings.nessie_provider_mode.clone(),
        ENV_API_KEY => settings
            .nessie_api_key
            .as_ref()
            .map(|k| k.expose().to_owned()),
        ENV_BASE_URL => settings.nessie_base_url.clone(),
        _ => None,
    })
}

/// Selects the bank from the settings ([`nessie_config`]) and registers the `/ready` entry. The
/// key is never logged: only the settings error text is, and it names the variable, not its
/// value.
pub fn register(app: &App) {
    let state = match nessie_config(&app.settings) {
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
            None => Arc::new(NessieState::misconfigured(
                "the Nessie integration is not registered",
            )),
        }
    }
}

/// Graceful shutdown: waits until no execute of the current bank is between its two phases, at
/// most `max`. Returns how many were still in flight then (their claims become `MAY_EXIST` by the
/// stale-claim rule).
pub async fn wait_for_flows_within(app: &App, max: Duration) -> usize {
    let state = app.nessie();
    let until = Instant::now() + max;
    loop {
        let n = state.flights().count();
        if n == 0 || Instant::now() >= until {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// [`wait_for_flows_within`] for [`ClaimTiming::shutdown_wait`] (30 s with the defaults).
pub async fn wait_for_flows(app: &App) -> usize {
    let max = app.nessie().timing().shutdown_wait();
    wait_for_flows_within(app, max).await
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
    let digest = Sha256::digest(format!(
        "second-site:nessie:{project_id}:{property_id}:{plan_item_id}"
    ));
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
        let simulation =
            Simulation::from_wire(&self.simulation).ok_or_else(|| bad("simulation"))?;
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

/// Who stored an `IN_FLIGHT` claim and until when its bank call may be running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptClaim {
    /// This claim (a fresh id per execute that reached the claim).
    pub claim_id: Uuid,
    /// The server instance that stored it ([`NessieState::instance_id`]).
    pub instance_id: Uuid,
    pub claimed_at: UtcDateTime,
    /// The bank call is never started after this time.
    pub send_by: UtcDateTime,
    /// After this time no bank call for this claim can still be running (`send_by` plus the
    /// bank's whole-request deadline plus [`DEFAULT_CLAIM_MARGIN`]).
    pub settle_after: UtcDateTime,
}

/// One simulated payment attempt of a plan item (kind `payment_attempt`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentAttemptData {
    /// [`ATTEMPT_IN_FLIGHT`] (claimed: being sent, or its outcome not recorded),
    /// [`ATTEMPT_SENT`], [`ATTEMPT_REFUSED`] or [`ATTEMPT_MAY_EXIST`].
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
    /// The claim this attempt was made under (absent on rows written before nessie-3).
    #[serde(default)]
    pub claim: Option<AttemptClaim>,
    /// How the stored outcome was decided: [`SOURCE_BANK_ANSWER`], [`SOURCE_UNRECORDED_CLAIM`],
    /// [`SOURCE_LATE_BANK_ANSWER`] or [`SOURCE_REFRESH_FOUND`] (absent while `IN_FLIGHT`).
    #[serde(default)]
    pub outcome_source: Option<String>,
    /// When a stale claim was settled as `MAY_EXIST`.
    #[serde(default)]
    pub settled_at: Option<UtcDateTime>,
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
                if self.sandbox.is_none()
                    || self.payment.is_some()
                    || self.record_key != SANDBOX_KEY
                {
                    c.value_error(
                        "a sandbox record carries sandbox data only, under record_key 'sandbox'",
                    );
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

fn later(t: UtcDateTime, d: Duration) -> UtcDateTime {
    let base = t.as_datetime();
    let shifted = chrono::TimeDelta::from_std(d)
        .ok()
        .and_then(|d| base.checked_add_signed(d))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC);
    UtcDateTime::from(shifted)
}

/// True when an `IN_FLIGHT` claim's bank call can no longer be running and its phase 2 will never
/// come (module documentation, "Stale claims"): for a claim of this instance, when the in-flight
/// registry no longer holds it; for another instance's claim, after its `settle_after`. Never
/// true while this instance still holds the claim.
pub fn claim_is_dead(state: &NessieState, p: &PaymentAttemptData, now: UtcDateTime) -> bool {
    if p.attempt_status != ATTEMPT_IN_FLIGHT {
        return false;
    }
    match &p.claim {
        Some(c) if c.instance_id == state.instance_id() => !state.flights().contains(c.claim_id),
        Some(c) => now > c.settle_after,
        // Every IN_FLIGHT row is written with its claim; without one, the attempt time and this
        // server's own timing stand in for it.
        None => now > later(p.attempted_at, state.timing().settle_after()),
    }
}

/// APPROVED -> EXECUTED for the approval of an attempt that may have been made: the transition and
/// the version-checked update of `ApprovalService::mark_executed`, for a claim settled without the
/// [`ApprovedAction`] of its phase 1 (that one lived in the flow that stopped). An approval past
/// its `expires_at` is spent too: phase 1 found it unexpired before anything was sent. Any other
/// status is already unusable and is left as it is.
async fn spend_approval(tx: &mut dyn Tx, approval_id: Uuid) -> AppResult<ApprovalStatus> {
    let Some((a, version)) = APPROVALS.get_for_update(tx, approval_id).await? else {
        return Err(AppError::internal(format!(
            "ApprovalRequest {approval_id} of a simulated payment attempt not found"
        )));
    };
    if a.approval_status != ApprovalStatus::Approved {
        return Ok(a.approval_status);
    }
    let spent = a.transition(ApprovalStatus::Executed)?;
    APPROVALS.update_versioned(tx, &spent, version).await?;
    Ok(ApprovalStatus::Executed)
}

/// Settles a dead claim as `MAY_EXIST` and spends its approval, in the caller's transaction (which
/// holds the plan item's lock and commits). It is never retried.
async fn settle_dead_claim(
    tx: &mut dyn Tx,
    mut rec: NessieSimulationRecord,
) -> AppResult<NessieSimulationRecord> {
    let now = utcnow();
    let p = rec
        .payment
        .as_mut()
        .ok_or_else(|| AppError::internal("attempt record without payment data"))?;
    p.attempt_status = ATTEMPT_MAY_EXIST.into();
    p.failure = Some(AttemptFailure {
        outcome: ATTEMPT_MAY_EXIST.into(),
        http_status: None,
        problem_slug: None,
        kind: None,
        message: "the outcome of this attempt was never recorded: the server stopped, or could not write to the database, after the claim was stored and before the bank's answer was recorded, so the bank may have received the simulated payment".into(),
    });
    p.outcome_source = Some(SOURCE_UNRECORDED_CLAIM.into());
    p.settled_at = Some(now);
    let (approval_id, item, attempt) =
        (p.approval_id, p.plan_item_id.clone(), p.attempt_id.clone());
    rec.updated_at = now;
    NESSIE_RECORDS.upsert(tx, &rec).await?;
    let status = spend_approval(tx, approval_id).await?;
    logging::warning(
        "app.integrations.nessie",
        "a simulated payment claim whose outcome was never recorded is now MAY_EXIST",
        &[
            ("plan_item_id", Value::String(item)),
            ("attempt_id", Value::String(attempt)),
            ("approval_status", Value::String(status.as_str().to_owned())),
        ],
    );
    Ok(rec)
}

/// The plan item's attempt row as it stands, with a dead claim settled first. The caller holds
/// the plan item's lock and commits its transaction even when it then refuses, so a settlement
/// is durable.
async fn current_attempt(
    tx: &mut dyn Tx,
    state: &NessieState,
    project_id: Uuid,
    property_id: Uuid,
    plan_item_id: &str,
) -> AppResult<Option<NessieSimulationRecord>> {
    let rec = attempt_record(tx, project_id, property_id, plan_item_id).await?;
    match rec {
        Some(r)
            if r.payment
                .as_ref()
                .is_some_and(|p| claim_is_dead(state, p, utcnow())) =>
        {
            Ok(Some(settle_dead_claim(tx, r).await?))
        }
        other => Ok(other),
    }
}

fn claim_id_of(rec: &NessieSimulationRecord) -> Option<Uuid> {
    rec.payment
        .as_ref()
        .and_then(|p| p.claim.as_ref())
        .map(|c| c.claim_id)
}

/// Why a stored sandbox cannot be used with the current bank.
fn sandbox_unusable(state: &NessieState, sb: &SandboxData) -> Option<String> {
    if sb.bank_ref != state.bank_ref() {
        return Some(match Simulation::from_wire(&sb.simulation) {
            Some(Simulation::FixtureBank) if state.simulation() == Simulation::FixtureBank => {
                "the fixture bank was reset (the server restarted), so the sandbox it held is gone"
                    .into()
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
    let mut live: Vec<CostScenario> =
        superseded::live_for_property::<CostScenario>(tx, property_id)
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
    plan_move_in_payments(&inputs.scenario, bom.as_ref(), state.simulation(), None)
        .map_err(plan_error)
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
pub async fn set_up_sandbox(
    app: &App,
    project_id: Uuid,
    requested: Option<i64>,
) -> AppResult<SetUp> {
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
                            replace_reason =
                                Some("the bank no longer knows the stored sandbox account".into())
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
        return Err(
            AppError::not_found("no Nessie sandbox is set up for this project")
                .code("NESSIE_SANDBOX_NOT_SET_UP"),
        );
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
    let inputs = crate::in_tx!(app, |tx| {
        plan_inputs(tx, project_id, property_id).await?
    });
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
    let report = ss_nessie::check_affordability(&account, &plan, &inputs.scenario, reserve)
        .map_err(plan_error)?;
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

/// D2: the 409 for a plan item whose attempt row exists and was not refused by the bank (`None`
/// otherwise). It never waits for a bank call. Its details carry the attempt's `simulation` label
/// and disclaimer (F7).
fn attempted_error(
    state: &NessieState,
    existing: &Option<NessieSimulationRecord>,
) -> Option<AppError> {
    let p = existing.as_ref()?.payment.as_ref()?;
    if p.attempt_status == ATTEMPT_REFUSED {
        return None;
    }
    let sim = Simulation::from_wire(&p.simulation).unwrap_or_else(|| state.simulation());
    let more = match p.attempt_status.as_str() {
        ATTEMPT_IN_FLIGHT => format!(
            ". It is being sent to {} now; nothing else is sent for this plan item, and its outcome is recorded when the bank answers",
            bank_name(sim)
        ),
        ATTEMPT_MAY_EXIST => format!(
            ". Its outcome is unknown: the simulated payment may have been made in {} (mock money). Check the sandbox account's transactions (the description names plan item {} and attempt {}) before trying again; it is never sent again",
            bank_name(sim),
            p.plan_item_id,
            p.attempt_id
        ),
        _ => String::new(),
    };
    Some(
        AppError::conflict(format!(
            "plan item {} already has a simulated payment attempt ({}); only one per plan item is made{more}",
            p.plan_item_id, p.attempt_status
        ))
        .code("NESSIE_ALREADY_ATTEMPTED")
        .details(json!({
            "simulation": sim,
            "disclaimer": sim.disclaimer(),
            "attempt_status": p.attempt_status,
            "approval_id": p.approval_id,
            "attempt_id": p.attempt_id,
            "outcome": p.outcome,
            "outcome_source": p.outcome_source,
        })),
    )
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
    // Phase 2: everything again under the plan item's lock, then the approval. A refusal for an
    // existing attempt is committed (with a stale claim settled just now) before it is returned.
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
        let existing = current_attempt(tx, &state, project_id, property_id, &item).await?;
        if let Some(refused) = attempted_error(&state, &existing) {
            Err(refused)
        } else {
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
                    .list(
                        tx,
                        Query::new().eq("idempotency_key", key.as_str()).limit(1),
                    )
                    .await?
                    .into_iter()
                    .next();
                match existing {
                    Some(a) if a.project_id != project_id => {
                        return Err(AppError::conflict(
                            "idempotency key already used by another project",
                        )
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
            Ok(ApprovalView {
            simulation: plan.simulation,
            disclaimer: plan.disclaimer,
            created,
            property_id,
            plan_item_id: item.clone(),
            next_step: format!(
                "Decide it with POST /api/v1/approvals/{}/decision (decision APPROVED or REJECTED, action_payload_hash {}), then POST .../payments/{item}/execute with this approval_id. Simulation only: nothing real is paid or committed.",
                approval.approval_id, approval.action_payload_hash
            ),
            attempt_id,
            approval,
        })
        }
    });
    view
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

/// Outcome of an execute, committed (phase 2) before the answer (D1).
pub enum Executed {
    Sent(Box<ExecuteView>),
    /// 422: refused by the bank; the approval is not spent.
    Refused(AppError),
    /// 502: may have happened; the approval is spent (or, when the outcome could not be recorded,
    /// will be spent when the claim is settled).
    MayExist(AppError),
    /// 409 `NESSIE_NOT_SENT`: nothing reached the bank; the claim was released and the approval
    /// is unchanged.
    NotSent(AppError),
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

/// What phase 1 committed and hands to the bank call and phase 2.
struct Claimed {
    project_id: Uuid,
    property_id: Uuid,
    item: String,
    approval_id: Uuid,
    /// The row before the claim (an earlier refusal), put back when nothing is sent.
    existing: Option<NessieSimulationRecord>,
    record_id: Uuid,
    claim_id: Uuid,
    approved: ApprovedAction,
    intent: PaymentIntent,
    /// Holds the claim in this instance's in-flight registry until phase 2 has ended.
    flight: Arc<FlightGuard>,
    /// When the claim was built (before its commit): the send window counts from here.
    started: Instant,
}

impl Claimed {
    fn label(&self) -> Value {
        json!({
            "simulation": self.intent.simulation,
            "disclaimer": self.intent.simulation.disclaimer(),
            "approval_id": self.approval_id,
            "plan_item_id": self.item,
            "attempt_id": self.intent.attempt_id,
        })
    }
}

/// What the bank call gave.
enum CallResult {
    /// The call was not started: the send window had passed, or its task never ran.
    NotStarted(String),
    /// The crate's answer.
    Answer(Result<Box<SimulatedPaymentRecord>, PaymentError>),
    /// The call's task failed after it may have sent the request.
    Lost(String),
}

impl CallResult {
    /// The stored outcome this answer becomes (for the answer when it cannot be recorded).
    fn status(&self) -> &'static str {
        match self {
            CallResult::NotStarted(_) => "NOT_SENT",
            CallResult::Answer(Ok(_)) => ATTEMPT_SENT,
            CallResult::Answer(Err(e)) => match e.outcome {
                AttemptOutcome::NotSent => "NOT_SENT",
                AttemptOutcome::Refused => ATTEMPT_REFUSED,
                AttemptOutcome::MayExist => ATTEMPT_MAY_EXIST,
            },
            CallResult::Lost(_) => ATTEMPT_MAY_EXIST,
        }
    }
}

/// `POST .../payments/{plan_item_id}/execute` in two phases (module documentation): phase 1
/// checks everything and commits the claim; the bank call holds nothing; phase 2 records the
/// outcome and spends the approval or leaves it usable. When phase 2 cannot be written, the answer
/// is 502 `NESSIE_PAYMENT_MAY_EXIST` and the claim stays `IN_FLIGHT` until it is settled.
pub async fn execute_payment(
    app: &App,
    project_id: Uuid,
    property_id: Uuid,
    plan_item_id: &str,
    approval_id: Uuid,
) -> AppResult<Executed> {
    let state = app.nessie();
    let bank = state.bank()?;
    let (claimed, auth) = claim_payment(
        app,
        &state,
        project_id,
        property_id,
        plan_item_id,
        approval_id,
    )
    .await?;
    let result = call_bank(&state, bank, auth, claimed.flight.clone(), claimed.started).await;
    match record_outcome(app, &claimed, &result).await {
        Ok(executed) => Ok(executed),
        Err(e) => {
            logging::warning(
                "app.integrations.nessie",
                "the outcome of a simulated payment could not be recorded; its claim stays IN_FLIGHT and becomes MAY_EXIST",
                &[
                    ("plan_item_id", Value::String(claimed.item.clone())),
                    ("attempt_id", Value::String(claimed.intent.attempt_id.clone())),
                    ("bank_answer", Value::String(result.status().to_owned())),
                    ("error", Value::String(e.to_string())),
                ],
            );
            Ok(Executed::MayExist(unrecorded_problem(&claimed, &result)))
        }
    }
    // `claimed.flight` is dropped here, after phase 2: from now on this instance treats an
    // unrecorded claim as dead.
}

/// Phase 1, one short transaction, in today's order: the plan rebuilt from stored rows, the
/// project's sandbox, the plan item's lock, D2 (with a dead claim settled first), the approval
/// for the exact rebuilt payload, the crate's authorization; then the claim, committed.
async fn claim_payment(
    app: &App,
    state: &Arc<NessieState>,
    project_id: Uuid,
    property_id: Uuid,
    plan_item_id: &str,
    approval_id: Uuid,
) -> AppResult<(Claimed, PaymentAuthorization)> {
    let item = plan_item_id.to_owned();
    let claimed = crate::in_tx!(app, |tx| {
        let inputs = plan_inputs(tx, project_id, property_id).await?;
        let plan = build_plan(state, &inputs)?;
        let planned = plan
            .payment(&item)
            .cloned()
            .ok_or_else(|| plan_error(PlanError::UnknownItem(item.clone())))?;
        let (sandbox_rec, sb) = usable_sandbox(state, sandbox_record(tx, project_id).await?)?;
        let bill = occupancy_bill_id(tx, project_id, property_id, &item).await?;
        tx.advisory_xact_lock(plan_item_lock_key(project_id, property_id, &item))
            .await?;
        let existing = current_attempt(tx, state, project_id, property_id, &item).await?;
        if let Some(refused) = attempted_error(state, &existing) {
            // Committed: a claim settled just now stays settled.
            Err(refused)
        } else {
            // The approval's attempt id: the only value taken from the stored approval;
            // everything else in the payload is rebuilt now from the current plan and sandbox.
            let stored = APPROVALS
                .require_in_project(
                    tx,
                    approval_id,
                    project_id,
                    &format!("ApprovalRequest {approval_id} not found"),
                )
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
            let started = Instant::now();
            let timing = state.timing();
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
            let claim = AttemptClaim {
                claim_id: Uuid::new_v4(),
                instance_id: state.instance_id(),
                claimed_at: now,
                send_by: later(now, timing.send_window),
                settle_after: later(now, timing.settle_after()),
            };
            let rec = NessieSimulationRecord {
                record_id: existing
                    .as_ref()
                    .map_or_else(ss_core::new_id, |r| r.record_id),
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
                    claim: Some(claim.clone()),
                    outcome_source: None,
                    settled_at: None,
                }),
                created_at: existing.as_ref().map_or(now, |r| r.created_at),
                updated_at: now,
            };
            rec.validate()?;
            // Registered before the commit, so a request of this instance that sees the committed
            // claim also sees it in flight; dropped (removed) if the commit fails.
            let flight = state.flights().start(claim.claim_id);
            NESSIE_RECORDS.upsert(tx, &rec).await?;
            Ok((
                Claimed {
                    project_id,
                    property_id,
                    item: item.clone(),
                    approval_id,
                    existing,
                    record_id: rec.record_id,
                    claim_id: claim.claim_id,
                    approved,
                    intent,
                    flight,
                    started,
                },
                auth,
            ))
        }
    });
    claimed
}

/// The bank call, with no transaction, lock or connection held. It is not started after the send
/// window; its task holds the claim in the in-flight registry until the call has returned.
async fn call_bank(
    state: &NessieState,
    bank: Arc<dyn NessieBank>,
    auth: PaymentAuthorization,
    flight: Arc<FlightGuard>,
    started: Instant,
) -> CallResult {
    let window = state.timing().send_window;
    let sending = Arc::new(AtomicBool::new(false));
    let marker = sending.clone();
    let joined = crate::integrations::blocking(move || {
        let _flight = flight;
        if started.elapsed() > window {
            return None;
        }
        marker.store(true, Ordering::SeqCst);
        Some(bank.execute(auth))
    })
    .await;
    match joined {
        Ok(Some(answer)) => CallResult::Answer(answer.map(Box::new)),
        Ok(None) => CallResult::NotStarted(format!(
            "the bank call could not start within {} ms of the claim",
            window.as_millis()
        )),
        Err(e) if !sending.load(Ordering::SeqCst) => {
            CallResult::NotStarted(format!("the bank call did not run: {e}"))
        }
        Err(e) => CallResult::Lost(format!("the bank call's task failed: {e}")),
    }
}

/// Phase 2, a second short transaction: the outcome on the claim, the approval spent or left
/// usable. A claim another server settled meanwhile (`MAY_EXIST`) keeps its state unless the bank
/// answered with the approved object.
async fn record_outcome(app: &App, c: &Claimed, result: &CallResult) -> AppResult<Executed> {
    let out = crate::in_tx!(app, |tx| {
        tx.advisory_xact_lock(plan_item_lock_key(c.project_id, c.property_id, &c.item))
            .await?;
        let current = attempt_record(tx, c.project_id, c.property_id, &c.item).await?;
        let Some(rec) = current.filter(|r| claim_id_of(r) == Some(c.claim_id)) else {
            return Err(AppError::internal(
                "the claim of this simulated payment attempt is no longer stored",
            ));
        };
        let status = rec
            .payment
            .as_ref()
            .map(|p| p.attempt_status.clone())
            .unwrap_or_default();
        match status.as_str() {
            ATTEMPT_IN_FLIGHT => record_answer(app, tx, c, rec, result).await?,
            ATTEMPT_MAY_EXIST => record_late_answer(tx, c, rec, result).await?,
            other => {
                return Err(AppError::internal(format!(
                    "unexpected attempt status {other} for a claim in flight"
                )))
            }
        }
    });
    Ok(out)
}

fn sent_view(
    record: &SimulatedPaymentRecord,
    approval_id: Uuid,
    status: ApprovalStatus,
) -> Executed {
    Executed::Sent(Box::new(ExecuteView {
        simulation: record.simulation,
        disclaimer: record.disclaimer,
        attempt_status: ATTEMPT_SENT,
        approval_id,
        approval_status: status.as_str().to_owned(),
        record: record.clone(),
    }))
}

fn mark_sent(p: &mut PaymentAttemptData, record: &SimulatedPaymentRecord, source: &str) {
    p.attempt_status = ATTEMPT_SENT.into();
    p.outcome = Some(record.outcome.as_str().to_owned());
    p.provider_status = Some(record.provider_status.clone());
    p.nessie_transaction_id = record.nessie_transaction_id.clone();
    p.nessie_bill_id = record.nessie_bill_id.clone();
    p.record = Some(StoredPaymentRecord::of(record));
    p.outcome_source = Some(source.into());
}

fn may_exist_problem(c: &Claimed, cause: &str, details: Value) -> AppError {
    AppError::provider_unavailable(format!(
        "The simulated payment may have been made in {} (mock money): {cause}. The approval is spent. Check the sandbox account's transactions (the description names plan item {} and attempt {}) before trying again; it will not be sent twice.",
        bank_name(c.intent.simulation),
        c.intent.plan_item_id,
        c.intent.attempt_id
    ))
    .code("NESSIE_PAYMENT_MAY_EXIST")
    .details(details)
    .with_retryable(false)
}

fn with(mut details: Value, more: Value) -> Value {
    if let (Some(d), Some(m)) = (details.as_object_mut(), more.as_object()) {
        for (k, v) in m {
            d.insert(k.clone(), v.clone());
        }
    }
    details
}

/// Puts the row back as it was before the claim (nothing reached the bank).
async fn release_claim(tx: &mut dyn Tx, c: &Claimed) -> AppResult<()> {
    match &c.existing {
        Some(old) => NESSIE_RECORDS.upsert(tx, old).await?,
        None => {
            tx.delete_where(
                &schema::NESSIE_SIMULATION_RECORDS,
                &[Filter::Eq("id", SqlValue::Uuid(c.record_id))],
            )
            .await?;
        }
    }
    Ok(())
}

async fn record_answer(
    app: &App,
    tx: &mut dyn Tx,
    c: &Claimed,
    mut rec: NessieSimulationRecord,
    result: &CallResult,
) -> AppResult<Executed> {
    let label = c.label();
    let mut p = rec
        .payment
        .take()
        .ok_or_else(|| AppError::internal("attempt record without payment data"))?;
    let not_sent = |why: String| {
        Executed::NotSent(
            AppError::conflict(format!("the simulated payment was not sent: {why}"))
                .code("NESSIE_NOT_SENT")
                .details(label.clone()),
        )
    };
    let executed = match result {
        CallResult::Answer(Ok(record)) => {
            mark_sent(&mut p, record, SOURCE_BANK_ANSWER);
            rec.payment = Some(p);
            rec.updated_at = utcnow();
            NESSIE_RECORDS.upsert(tx, &rec).await?;
            let spent = app
                .approvals()
                .mark_executed(tx, c.approved.clone())
                .await?;
            sent_view(record, c.approval_id, spent.approval_status)
        }
        CallResult::NotStarted(why) => {
            release_claim(tx, c).await?;
            not_sent(why.clone())
        }
        CallResult::Answer(Err(e)) if e.outcome == AttemptOutcome::NotSent => {
            // Nothing reached the bank: the claim is released; the approval is unchanged.
            release_claim(tx, c).await?;
            not_sent(e.error.to_string())
        }
        CallResult::Answer(Err(e)) if e.outcome == AttemptOutcome::Refused => {
            let failure = failure_of(e.outcome, &e.error);
            p.attempt_status = ATTEMPT_REFUSED.into();
            p.failure = Some(failure.clone());
            p.outcome_source = Some(SOURCE_BANK_ANSWER.into());
            rec.payment = Some(p);
            rec.updated_at = utcnow();
            NESSIE_RECORDS.upsert(tx, &rec).await?;
            let details = with(
                label,
                json!({
                    "attempt_status": ATTEMPT_REFUSED,
                    "approval_status": "APPROVED",
                    "http_status": failure.http_status,
                    "problem_slug": failure.problem_slug,
                }),
            );
            Executed::Refused(
                AppError::conflict(format!(
                    "{} refused the simulated payment{}: nothing was created, and the approval is not spent (fix the cause, then execute it again). {}",
                    bank_name(c.intent.simulation),
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
        CallResult::Answer(Err(_)) | CallResult::Lost(_) => {
            let (failure, cause) = match result {
                CallResult::Answer(Err(e)) => {
                    (failure_of(e.outcome, &e.error), e.error.to_string())
                }
                CallResult::Lost(why) => (
                    AttemptFailure {
                        outcome: ATTEMPT_MAY_EXIST.into(),
                        http_status: None,
                        problem_slug: None,
                        kind: None,
                        message: why.clone(),
                    },
                    why.clone(),
                ),
                _ => return Err(AppError::internal("unreachable bank outcome")),
            };
            p.attempt_status = ATTEMPT_MAY_EXIST.into();
            p.failure = Some(failure.clone());
            p.outcome_source = Some(SOURCE_BANK_ANSWER.into());
            rec.payment = Some(p);
            rec.updated_at = utcnow();
            NESSIE_RECORDS.upsert(tx, &rec).await?;
            let spent = app
                .approvals()
                .mark_executed(tx, c.approved.clone())
                .await?;
            let details = with(
                label,
                json!({
                    "attempt_status": ATTEMPT_MAY_EXIST,
                    "approval_status": spent.approval_status.as_str(),
                    "http_status": failure.http_status,
                    "kind": failure.kind,
                }),
            );
            Executed::MayExist(may_exist_problem(c, &cause, details))
        }
    };
    Ok(executed)
}

/// The bank answered after another server had settled this claim as `MAY_EXIST` (its approval
/// spent): an answer with the approved object makes it `SENT`; anything else leaves it as it is.
async fn record_late_answer(
    tx: &mut dyn Tx,
    c: &Claimed,
    mut rec: NessieSimulationRecord,
    result: &CallResult,
) -> AppResult<Executed> {
    let label = c.label();
    if let CallResult::Answer(Ok(record)) = result {
        let mut p = rec
            .payment
            .take()
            .ok_or_else(|| AppError::internal("attempt record without payment data"))?;
        mark_sent(&mut p, record, SOURCE_LATE_BANK_ANSWER);
        rec.payment = Some(p);
        rec.updated_at = utcnow();
        NESSIE_RECORDS.upsert(tx, &rec).await?;
        let status = spend_approval(tx, c.approval_id).await?;
        return Ok(sent_view(record, c.approval_id, status));
    }
    let status = spend_approval(tx, c.approval_id).await?;
    let details = with(
        label,
        json!({
            "attempt_status": ATTEMPT_MAY_EXIST,
            "approval_status": status.as_str(),
            "late_bank_answer": result.status(),
        }),
    );
    Ok(Executed::MayExist(may_exist_problem(
        c,
        &format!(
            "the bank's answer ({}) came after this attempt had already been recorded as of unknown outcome, so it stays MAY_EXIST",
            result.status()
        ),
        details,
    )))
}

/// The answer when phase 2 could not be written: the claim stays `IN_FLIGHT` and becomes
/// `MAY_EXIST` (its approval spent) when it is next read.
fn unrecorded_problem(c: &Claimed, result: &CallResult) -> AppError {
    let answer = match result {
        CallResult::Answer(Ok(r)) => format!(
            "the bank answered with the simulated payment (status {}{})",
            r.provider_status,
            r.nessie_transaction_id
                .as_deref()
                .map(|t| format!(", transaction {t}"))
                .unwrap_or_default()
        ),
        CallResult::Answer(Err(e)) => format!("the bank's answer: {}", e.error),
        CallResult::NotStarted(why) => format!("it was not sent ({why})"),
        CallResult::Lost(why) => why.clone(),
    };
    let lead = if result.status() == "NOT_SENT" {
        format!(
            "The simulated payment was not sent to {} ({answer}), but releasing its claim could not be recorded (the database could not be written)",
            bank_name(c.intent.simulation)
        )
    } else {
        format!(
            "The simulated payment may have been made in {} (mock money): {answer}, but its outcome could not be recorded (the database could not be written)",
            bank_name(c.intent.simulation)
        )
    };
    let details = with(
        c.label(),
        json!({
            "attempt_status": ATTEMPT_IN_FLIGHT,
            "recorded": false,
            "bank_answer": result.status(),
            "approval_status": "APPROVED",
            "nessie_transaction_id": match result {
                CallResult::Answer(Ok(r)) => r.nessie_transaction_id.clone(),
                _ => None,
            },
        }),
    );
    AppError::provider_unavailable(format!(
        "{lead}. This attempt stays blocked: it is shown as MAY_EXIST, and its approval spent, the next time it is read, and it will not be sent twice. Check the sandbox account's transactions (the description names plan item {} and attempt {}) before trying again.",
        c.intent.plan_item_id,
        c.intent.attempt_id
    ))
    .code("NESSIE_PAYMENT_MAY_EXIST")
    .details(details)
    .with_retryable(false)
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
/// A claim still `IN_FLIGHT` is settled first when its call can no longer be running. A
/// `MAY_EXIST` purchase is looked for among the sandbox account's newest transactions by its
/// description, which names the plan item and the attempt id ([`find_unknown_payment`]).
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
    let mut p = rec
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
    if p.attempt_status == ATTEMPT_IN_FLIGHT {
        let now = crate::in_tx!(app, |tx| {
            tx.advisory_xact_lock(plan_item_lock_key(project_id, property_id, &item))
                .await?;
            current_attempt(tx, &state, project_id, property_id, &item).await?
        });
        p = now
            .and_then(|r| r.payment)
            .ok_or_else(|| AppError::internal("attempt record without payment data"))?;
        if p.attempt_status == ATTEMPT_IN_FLIGHT {
            return Err(AppError::conflict(format!(
                "plan item {item} has no transaction to refresh yet: its simulated payment is being sent now; refresh again when the execute has answered"
            ))
            .code("NESSIE_NOTHING_TO_REFRESH")
            .details(json!({"attempt_status": ATTEMPT_IN_FLIGHT})));
        }
    }
    if p.attempt_status == ATTEMPT_MAY_EXIST {
        return find_unknown_payment(app, bank, project_id, property_id, &item, &p, &sb).await;
    }
    let stored = match (&p.record, &p.nessie_transaction_id) {
        (Some(r), Some(_)) if p.attempt_status == ATTEMPT_SENT => r.restore()?,
        _ => {
            let why = match p.attempt_status.as_str() {
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

/// A `MAY_EXIST` attempt, settled where the bank can tell. Only a purchase can be found: its
/// Nessie description starts with [`description_prefix`] (the plan item and the attempt id), while
/// a recurring bill or a bill payment carries no description. The newest
/// [`REFRESH_SCAN_LIMIT`] transactions of the project's sandbox account are read with the crate's
/// `list_account_transactions` (nothing is listed or matched by name). Exactly one purchase whose
/// description names this attempt, from the approved account to the approved merchant for the
/// approved amount, makes the attempt `SENT` (its approval stays spent); none, several or a
/// mismatch leave it `MAY_EXIST` (409 `NESSIE_NOTHING_TO_REFRESH`), because absence from the list
/// does not prove that the payment was not made.
async fn find_unknown_payment(
    app: &App,
    bank: Arc<dyn NessieBank>,
    project_id: Uuid,
    property_id: Uuid,
    item: &str,
    p: &PaymentAttemptData,
    sb: &SandboxData,
) -> AppResult<RefreshView> {
    let unknown = |why: String, extra: Value| {
        AppError::conflict(format!(
            "plan item {item} has no transaction to refresh: its outcome is unknown (MAY_EXIST), {why}. Check the sandbox account's transactions (the description names plan item {item} and attempt {}) before trying again; it is never sent again",
            p.attempt_id
        ))
        .code("NESSIE_NOTHING_TO_REFRESH")
        .details(with(json!({"attempt_status": ATTEMPT_MAY_EXIST}), extra))
    };
    if p.operation != "purchase" {
        let what = if p.operation == "pay_bill" {
            "a bill payment"
        } else {
            "a recurring bill"
        };
        return Err(unknown(
            format!(
                "and the bank cannot tell: {what} carries no description that names the attempt"
            ),
            json!({}),
        ));
    }
    let approval_id = p.approval_id;
    let approval =
        crate::in_tx!(app, |tx| { APPROVALS.get(tx, approval_id).await? }).ok_or_else(|| {
            AppError::internal("the approval of a simulated payment attempt is not stored")
        })?;
    let field = |k: &str| {
        approval
            .action_payload
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let (Some(account), Some(merchant)) = (field("account_id"), field("merchant_id")) else {
        return Err(AppError::internal(
            "the approved payload of a purchase names no account or merchant",
        ));
    };
    let approved_amount = approval
        .action_payload
        .get("amount_minor")
        .and_then(Value::as_i64);
    if sb.account_id.as_deref() != Some(account.as_str()) {
        return Err(unknown(
            "and the approved account is not this project's current sandbox account".into(),
            json!({}),
        ));
    }
    let prefix = description_prefix(item, &p.attempt_id);
    let acct = account.clone();
    let listed = match bank_call(bank, move |b| {
        b.list_account_transactions(&acct, REFRESH_SCAN_LIMIT)
    })
    .await?
    {
        Ok(l) => l,
        Err(e) if is_not_found(&e) => {
            return Err(AppError::conflict(
                "the project's Nessie sandbox is gone: the bank no longer knows the stored sandbox account; POST /nessie/sandbox to set it up again",
            )
            .code("NESSIE_SANDBOX_GONE")
            .details(json!({"reason": "the bank no longer knows the stored sandbox account"})))
        }
        Err(e) => return Err(bank_error(&e)),
    };
    let read = listed.record.len();
    let found: Vec<&Transaction> = listed
        .record
        .iter()
        .filter(|t| {
            t.description
                .as_deref()
                .is_some_and(|d| d.starts_with(&prefix))
        })
        .collect();
    let txn = match found.as_slice() {
        [] => {
            return Err(unknown(
                format!("and no transaction whose description names this attempt is among the {read} newest transactions of the sandbox account (at most {REFRESH_SCAN_LIMIT} are read), which does not prove that it was not made"),
                json!({"transactions_read": read, "found": 0}),
            ))
        }
        [t] => (*t).clone(),
        many => {
            return Err(unknown(
                format!(
                    "and {} transactions carry its description, so the bank cannot tell which one this attempt made",
                    many.len()
                ),
                json!({"transactions_read": read, "found": many.len()}),
            ))
        }
    };
    let mismatch = if txn.kind.as_str() != TransactionKind::Purchase.as_str() {
        Some("kind")
    } else if txn.payer_id.as_deref() != Some(account.as_str()) {
        Some("payer")
    } else if txn.merchant_id.as_deref() != Some(merchant.as_str()) {
        Some("merchant")
    } else if approved_amount != Some(txn.amount_cents) || txn.amount_cents != p.amount_minor {
        Some("amount")
    } else {
        None
    };
    if let Some(what) = mismatch {
        return Err(unknown(
            format!(
                "and the transaction {} that names this attempt differs from the approved payment in its {what}",
                txn.id
            ),
            json!({"transactions_read": read, "found": 1, "mismatch": what, "nessie_transaction_id": txn.id}),
        ));
    }
    let action_type = match approval.action_type {
        ActionType::Purchase => ApprovalAction::Purchase,
        ActionType::FinancialCommitment => ApprovalAction::FinancialCommitment,
        _ => {
            return Err(AppError::internal(
                "the approval of a simulated payment attempt is not for a payment",
            ))
        }
    };
    let sim = listed.simulation;
    let base = SimulatedPaymentRecord {
        simulation: sim,
        disclaimer: sim.disclaimer(),
        approval_id,
        project_id,
        property_id,
        scenario_id: p.scenario_id,
        plan_item_id: item.to_owned(),
        attempt_id: p.attempt_id.clone(),
        operation: "purchase",
        action_type,
        moves_money: true,
        amount_minor: p.amount_minor,
        currency: field("currency").unwrap_or_else(|| "USD".into()),
        outcome: SimulatedOutcome::Unknown,
        provider_status: String::new(),
        nessie_transaction_id: Some(txn.id.clone()),
        nessie_bill_id: None,
        note: String::new(),
    };
    let transaction = BankRecord::new(sim, txn);
    // The crate's own outcome and note for the transaction's status.
    let record = base.refreshed(&transaction).map_err(|e| bank_error(&e))?;
    let stored = StoredPaymentRecord::of(&record);
    let attempt_id = p.attempt_id.clone();
    let settled = crate::in_tx!(app, |tx| {
        tx.advisory_xact_lock(plan_item_lock_key(project_id, property_id, item))
            .await?;
        let mut done = false;
        if let Some(mut current) = attempt_record(tx, project_id, property_id, item).await? {
            if let Some(cp) = current.payment.as_mut() {
                if cp.attempt_status == ATTEMPT_MAY_EXIST
                    && cp.approval_id == approval_id
                    && cp.attempt_id == attempt_id
                {
                    cp.attempt_status = ATTEMPT_SENT.into();
                    cp.outcome = Some(stored.outcome.clone());
                    cp.provider_status = Some(stored.provider_status.clone());
                    cp.nessie_transaction_id = stored.nessie_transaction_id.clone();
                    cp.record = Some(stored.clone());
                    cp.outcome_source = Some(SOURCE_REFRESH_FOUND.into());
                    cp.refreshed_at = Some(utcnow());
                    current.updated_at = utcnow();
                    NESSIE_RECORDS.upsert(tx, &current).await?;
                    done = true;
                }
            }
        }
        done
    });
    if !settled {
        return Err(AppError::conflict(
            "the attempt changed while it was refreshed; refresh again",
        )
        .code("NESSIE_ATTEMPT_CHANGED"));
    }
    Ok(RefreshView {
        simulation: record.simulation,
        disclaimer: record.disclaimer,
        attempt_status: ATTEMPT_SENT.into(),
        record,
        transaction,
    })
}

/// The stored attempt documents of a project (tests and diagnostics).
pub async fn records_of(
    tx: &mut dyn Tx,
    project_id: Uuid,
) -> AppResult<Vec<NessieSimulationRecord>> {
    NESSIE_RECORDS
        .list(tx, Query::new().eq("project_id", project_id))
        .await
}
