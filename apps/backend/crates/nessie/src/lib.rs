//! `ss-nessie`: the Capital One Nessie v2 sandbox integration of the Second Site backend.
//!
//! Nessie is a mock bank for hackathon projects ("The API patterns are real; the money isn't",
//! <https://v2.nessieisreal.com>). This crate lets the product simulate the payments that follow
//! choosing a property (security deposit, first month's rent and NNN, build-out and furnishing
//! purchases, the recurring rent) against a sandbox account, and check them against what that
//! sandbox account holds.
//!
//! Truth rules, enforced in types, strings and tests:
//! - Nessie money is mock money. Every record this crate returns is wrapped in a [`BankRecord`]
//!   or carries a [`Simulation`] marker and a disclaimer saying it is a simulation in the
//!   Capital One Nessie sandbox, or fixture data from the in-memory test bank. Nothing here is a
//!   real balance, payment, paid deposit or signed commitment. Amounts are integer minor units
//!   (US cents).
//! - A simulated payment is still a consequential action: [`NessieBank::execute`] takes a
//!   [`PaymentAuthorization`], which can only be built from an APPROVED, unexpired approval for
//!   exactly the payload being sent, and is consumed by the call. The Nessie request body is
//!   derived from the approved payload itself.
//! - A Nessie `pending` answer is recorded as pending ([`SimulatedOutcome::SimulatedPending`]),
//!   never as completed unless Nessie reports `completed`.
//! - Without an API key, or in fixture mode, [`select_bank`] returns the [`FixtureBank`], whose
//!   ids start with `fixture_` and whose records say they are fixture data.
//!
//! The crate depends on `ss-core` and `ss-domain` only. It does no I/O at construction and reads
//! the environment only through [`NessieConfig::from_env`].

pub mod affordability;
pub mod approval;
pub mod bank;
pub mod config;
pub mod error;
pub mod fixture;
pub mod live;
pub mod models;
pub mod money;
pub mod plan;
pub mod status;

use serde::{Deserialize, Serialize};

pub use affordability::{check_affordability, AffordabilityReport, AffordabilityVerdict};
pub use approval::{
    ApprovalAction, ApprovalEvidence, AuthorizationError, PaymentAuthorization, PaymentIntent,
    PaymentOperation,
};
pub use bank::{
    select_bank, AttemptOutcome, NessieBank, PaymentError, SimulatedOutcome,
    SimulatedPaymentRecord,
};
pub use config::{ApiKey, NessieConfig, ProviderMode, DEFAULT_BASE_URL};
pub use error::{NessieError, ProblemSummary};
pub use fixture::FixtureBank;
pub use live::LiveNessieBank;
pub use plan::{
    plan_move_in_payments, BomTotals, MoveInPaymentPlan, NotSimulatedLine, PayeeRole, PlanError,
    PlannedOperation, PlannedPayment, PlannedReserve, SandboxTargets,
};
pub use status::{nessie_status, Readiness, PROVIDER};

/// Shown with everything that comes from the Capital One Nessie sandbox.
pub const NESSIE_SANDBOX_DISCLAIMER: &str = "Simulation in the Capital One Nessie sandbox: mock \
money only. This is not a real bank balance, payment, paid deposit or signed commitment.";

/// Shown with everything that comes from the in-memory fixture bank.
pub const FIXTURE_BANK_DISCLAIMER: &str = "Fixture data from Second Site's in-memory test bank: \
not the Capital One Nessie sandbox and not a bank. Mock money only; nothing was paid.";

/// Which simulated bank a record comes from (or, for a plan, which bank it targets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Simulation {
    /// The Capital One Nessie v2 sandbox (mock money).
    #[serde(rename = "CAPITAL_ONE_NESSIE_SANDBOX")]
    NessieSandbox,
    /// Second Site's deterministic in-memory fixture bank (fixture data, no network).
    #[serde(rename = "FIXTURE_IN_MEMORY_BANK")]
    FixtureBank,
}

impl Simulation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Simulation::NessieSandbox => "CAPITAL_ONE_NESSIE_SANDBOX",
            Simulation::FixtureBank => "FIXTURE_IN_MEMORY_BANK",
        }
    }

    pub const fn disclaimer(self) -> &'static str {
        match self {
            Simulation::NessieSandbox => NESSIE_SANDBOX_DISCLAIMER,
            Simulation::FixtureBank => FIXTURE_BANK_DISCLAIMER,
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "CAPITAL_ONE_NESSIE_SANDBOX" => Some(Simulation::NessieSandbox),
            "FIXTURE_IN_MEMORY_BANK" => Some(Simulation::FixtureBank),
            _ => None,
        }
    }
}

impl std::fmt::Display for Simulation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A record returned by a [`NessieBank`], labelled with the bank it came from.
///
/// `simulation` and `disclaimer` are always present when serialized, so a record can never be
/// shown without saying it is mock money.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BankRecord<T> {
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    pub record: T,
}

impl<T> BankRecord<T> {
    pub fn new(simulation: Simulation, record: T) -> Self {
        Self {
            simulation,
            disclaimer: simulation.disclaimer(),
            record,
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> BankRecord<U> {
        BankRecord {
            simulation: self.simulation,
            disclaimer: self.disclaimer,
            record: f(self.record),
        }
    }
}
