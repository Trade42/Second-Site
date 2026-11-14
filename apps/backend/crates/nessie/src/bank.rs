//! The [`NessieBank`] trait, the simulated-payment record, and bank selection.
//!
//! The trait is sealed: its only implementations are [`crate::LiveNessieBank`] (the Capital One
//! Nessie sandbox over HTTP) and [`crate::FixtureBank`] (in memory, labelled as fixture data).
//! Nothing else can implement it, so nothing else can pretend to be either.
//!
//! Money only moves through [`NessieBank::execute`], which takes a [`PaymentAuthorization`]. The
//! other methods create and read sandbox objects (customers, accounts, merchants, bills) and
//! fund a sandbox account with mock money ([`NessieBank::deposit`]); none of them pays anyone.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::ProviderErrorKind;
use uuid::Uuid;

use crate::approval::{ApprovalAction, AuthorizationError, PaymentAuthorization, PaymentIntent};
use crate::approval::PaymentOperation;
use crate::config::NessieConfig;
use crate::error::NessieError;
use crate::models::{
    Account, AccountCreate, Bill, BillCreate, BillPaymentCreate, BillStatus, Customer,
    CustomerCreate, DepositCreate, Merchant, MerchantCreate, PurchaseCreate, Transaction,
    TransactionStatus,
};
use crate::money::format_usd;
use crate::{BankRecord, FixtureBank, LiveNessieBank, Simulation};

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// A simulated bank: the Capital One Nessie sandbox, or the in-memory fixture bank.
pub trait NessieBank: sealed::Sealed + Send + Sync + std::fmt::Debug {
    /// Which bank this is; every record it returns carries the same marker.
    fn simulation(&self) -> Simulation;

    /// The `/ready` entry (provider `nessie`).
    fn status(&self) -> ProviderStatusEntry;

    /// One authenticated read (`GET /accounts?limit=1`) to prove the key works. The fixture
    /// bank has no key and always succeeds.
    fn verify(&self) -> Result<(), NessieError>;

    fn create_customer(&self, body: &CustomerCreate) -> Result<BankRecord<Customer>, NessieError>;
    fn get_customer(&self, id: &str) -> Result<BankRecord<Customer>, NessieError>;
    /// Also deletes the customer's accounts; Nessie cancels their pending transactions first.
    fn delete_customer(&self, id: &str) -> Result<(), NessieError>;

    fn create_account(
        &self,
        customer_id: &str,
        body: &AccountCreate,
    ) -> Result<BankRecord<Account>, NessieError>;
    fn get_account(&self, id: &str) -> Result<BankRecord<Account>, NessieError>;
    fn delete_account(&self, id: &str) -> Result<(), NessieError>;
    /// Newest first, including transactions where the account is the payee (`limit` 1..5000).
    fn list_account_transactions(
        &self,
        account_id: &str,
        limit: u32,
    ) -> Result<BankRecord<Vec<Transaction>>, NessieError>;

    fn create_merchant(&self, body: &MerchantCreate) -> Result<BankRecord<Merchant>, NessieError>;
    fn get_merchant(&self, id: &str) -> Result<BankRecord<Merchant>, NessieError>;
    fn delete_merchant(&self, id: &str) -> Result<(), NessieError>;

    fn get_bill(&self, id: &str) -> Result<BankRecord<Bill>, NessieError>;
    fn delete_bill(&self, id: &str) -> Result<(), NessieError>;

    /// Mock money arriving in a sandbox account (`POST /accounts/{id}/deposits`). Nobody is
    /// paid, so no approval is needed; the balance it produces is only what was put there.
    fn deposit(
        &self,
        account_id: &str,
        body: &DepositCreate,
    ) -> Result<BankRecord<Transaction>, NessieError>;

    fn get_transaction(&self, id: &str) -> Result<BankRecord<Transaction>, NessieError>;
    /// Ends the pending window early (`POST /transactions/{id}/settle`).
    fn settle_transaction(&self, id: &str) -> Result<BankRecord<Transaction>, NessieError>;
    /// Cancels a pending transaction and releases its hold.
    fn cancel_transaction(&self, id: &str) -> Result<BankRecord<Transaction>, NessieError>;

    /// The only way to make a simulated payment. Consumes the authorization; the request body
    /// is built from the approved intent. Never retried (Nessie has no idempotency key).
    fn execute(&self, auth: PaymentAuthorization) -> Result<SimulatedPaymentRecord, PaymentError>;
}

/// What Nessie reported for a simulated payment. Every value says it is a simulation; only
/// `SIMULATED_COMPLETED` means Nessie reported the (mock) money as moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum SimulatedOutcome {
    /// Created and pending; it settles on its own (or by `settle`).
    #[serde(rename = "SIMULATED_PENDING")]
    SimulatedPending,
    /// Nessie reported the transaction as completed (mock money moved in the sandbox).
    #[serde(rename = "SIMULATED_COMPLETED")]
    SimulatedCompleted,
    #[serde(rename = "SIMULATED_DECLINED")]
    SimulatedDeclined,
    #[serde(rename = "SIMULATED_CANCELLED")]
    SimulatedCancelled,
    /// A recurring bill was recorded in the sandbox; no money moved.
    #[serde(rename = "SIMULATED_BILL_RECORDED")]
    SimulatedBillRecorded,
    /// Nessie reported a status this crate does not know; it is kept verbatim in
    /// `provider_status` and treated as unknown, never as completed.
    #[serde(rename = "UNKNOWN")]
    Unknown,
}

impl SimulatedOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            SimulatedOutcome::SimulatedPending => "SIMULATED_PENDING",
            SimulatedOutcome::SimulatedCompleted => "SIMULATED_COMPLETED",
            SimulatedOutcome::SimulatedDeclined => "SIMULATED_DECLINED",
            SimulatedOutcome::SimulatedCancelled => "SIMULATED_CANCELLED",
            SimulatedOutcome::SimulatedBillRecorded => "SIMULATED_BILL_RECORDED",
            SimulatedOutcome::Unknown => "UNKNOWN",
        }
    }

    pub fn from_transaction_status(s: &TransactionStatus) -> Self {
        match s {
            TransactionStatus::Pending => SimulatedOutcome::SimulatedPending,
            TransactionStatus::Completed => SimulatedOutcome::SimulatedCompleted,
            TransactionStatus::Declined => SimulatedOutcome::SimulatedDeclined,
            TransactionStatus::Cancelled => SimulatedOutcome::SimulatedCancelled,
            TransactionStatus::Unknown(_) => SimulatedOutcome::Unknown,
        }
    }
}

/// The record of one simulated payment, ready to store and show.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SimulatedPaymentRecord {
    pub simulation: Simulation,
    pub disclaimer: &'static str,
    pub approval_id: Uuid,
    pub project_id: Uuid,
    pub property_id: Uuid,
    pub scenario_id: Uuid,
    pub plan_item_id: String,
    /// `purchase`, `create_recurring_bill` or `pay_bill`.
    pub operation: &'static str,
    pub action_type: ApprovalAction,
    pub moves_money: bool,
    pub amount_minor: i64,
    pub currency: String,
    pub outcome: SimulatedOutcome,
    /// The bank's own status string, verbatim (transaction or bill status).
    pub provider_status: String,
    pub nessie_transaction_id: Option<String>,
    pub nessie_bill_id: Option<String>,
    /// Human-readable, and honest about what the status means.
    pub note: String,
}

impl SimulatedPaymentRecord {
    /// The same record after re-reading the transaction (`get_transaction`).
    pub fn refreshed(&self, tx: &BankRecord<Transaction>) -> Result<Self, NessieError> {
        if tx.simulation != self.simulation {
            return Err(NessieError::InvalidRequest(
                "transaction comes from a different bank than the record".into(),
            ));
        }
        if self.nessie_transaction_id.as_deref() != Some(tx.record.id.as_str()) {
            return Err(NessieError::InvalidRequest(
                "transaction id does not match the record".into(),
            ));
        }
        let mut next = self.clone();
        next.outcome = SimulatedOutcome::from_transaction_status(&tx.record.status);
        next.provider_status = tx.record.status.as_str().to_owned();
        next.note = transaction_note(self.simulation, next.outcome, &tx.record);
        Ok(next)
    }
}

/// What happened to a simulated payment that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AttemptOutcome {
    /// Nothing was sent (authorization or validation failure). The approval is unused.
    #[serde(rename = "NOT_SENT")]
    NotSent,
    /// The bank refused it with a 4xx; Nessie refuses before a transaction exists, so nothing
    /// was created. The approval is released in this process.
    #[serde(rename = "REFUSED")]
    Refused,
    /// The request may have been carried out (timeout, transport failure, 5xx, or a success
    /// answer this crate could not read). Nessie has no idempotency key: list the account's
    /// transactions before trying again, and treat the approval as spent.
    #[serde(rename = "MAY_EXIST")]
    MayExist,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("simulated payment for approval {approval_id} failed ({outcome:?}): {error}")]
pub struct PaymentError {
    pub approval_id: Uuid,
    pub outcome: AttemptOutcome,
    pub error: NessieError,
}

/// Approval ids this bank instance has used. The durable single-use guard is the server's
/// `mark_executed`; this one stops a second use inside one process even if that is forgotten.
#[derive(Debug, Default)]
pub(crate) struct ApprovalLedger {
    used: Mutex<HashSet<Uuid>>,
}

impl ApprovalLedger {
    pub(crate) fn reserve(&self, id: Uuid) -> Result<(), AuthorizationError> {
        let mut used = self.used.lock().unwrap_or_else(|p| p.into_inner());
        if used.insert(id) {
            Ok(())
        } else {
            Err(AuthorizationError::AlreadyUsed(id))
        }
    }

    pub(crate) fn release(&self, id: Uuid) {
        let mut used = self.used.lock().unwrap_or_else(|p| p.into_inner());
        used.remove(&id);
    }
}

/// The Nessie request an approved intent becomes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PreparedCall {
    Purchase {
        account_id: String,
        body: PurchaseCreate,
    },
    CreateBill {
        account_id: String,
        body: BillCreate,
    },
    PayBill {
        bill_id: String,
        body: BillPaymentCreate,
    },
}

/// Builds the request from the approved intent and validates it; nothing is sent here.
pub(crate) fn prepare_call(intent: &PaymentIntent) -> Result<PreparedCall, NessieError> {
    let call = match &intent.operation {
        PaymentOperation::Purchase {
            account_id,
            merchant_id,
        } => {
            let body = PurchaseCreate {
                merchant_id: merchant_id.clone(),
                amount_cents: intent.amount_minor,
                description: Some(intent.description.clone()),
            };
            body.validate().map_err(NessieError::InvalidRequest)?;
            PreparedCall::Purchase {
                account_id: account_id.clone(),
                body,
            }
        }
        PaymentOperation::CreateRecurringBill {
            account_id,
            payee,
            nickname,
            recurring_day,
        } => {
            let body = BillCreate::new_minor(
                BillStatus::Recurring,
                payee.clone(),
                Some(nickname.clone()),
                intent.amount_minor,
                Some(i64::from(*recurring_day)),
            )?;
            body.validate().map_err(NessieError::InvalidRequest)?;
            PreparedCall::CreateBill {
                account_id: account_id.clone(),
                body,
            }
        }
        PaymentOperation::PayBill { bill_id } => {
            let body = BillPaymentCreate {
                amount_cents: Some(intent.amount_minor),
            };
            body.validate().map_err(NessieError::InvalidRequest)?;
            PreparedCall::PayBill {
                bill_id: bill_id.clone(),
                body,
            }
        }
    };
    Ok(call)
}

/// The answer to a prepared call.
pub(crate) enum CallAnswer {
    Transaction(Transaction),
    Bill(Bill),
}

/// Shared `execute` flow: bank check, single-use reservation, request preparation, the call,
/// and the honest record. `send` performs the one request.
pub(crate) fn execute_with(
    bank: Simulation,
    ledger: &ApprovalLedger,
    auth: PaymentAuthorization,
    send: impl FnOnce(&PreparedCall) -> Result<CallAnswer, NessieError>,
) -> Result<SimulatedPaymentRecord, PaymentError> {
    let (approval_id, intent) = auth.into_parts();
    let fail = |outcome, error| PaymentError {
        approval_id,
        outcome,
        error,
    };
    if intent.simulation != bank {
        return Err(fail(
            AttemptOutcome::NotSent,
            AuthorizationError::WrongBank {
                approved: intent.simulation,
                bank,
            }
            .into(),
        ));
    }
    let call = prepare_call(&intent).map_err(|e| fail(AttemptOutcome::NotSent, e))?;
    ledger
        .reserve(approval_id)
        .map_err(|e| fail(AttemptOutcome::NotSent, e.into()))?;
    let answer = match send(&call) {
        Ok(a) => a,
        // Refused locally before anything was sent.
        Err(e @ (NessieError::InvalidRequest(_) | NessieError::Money(_))) => {
            ledger.release(approval_id);
            return Err(fail(AttemptOutcome::NotSent, e));
        }
        Err(e) if e.is_client_refusal() => {
            ledger.release(approval_id);
            return Err(fail(AttemptOutcome::Refused, e));
        }
        Err(e) => return Err(fail(AttemptOutcome::MayExist, e)),
    };
    record_for(bank, approval_id, &intent, answer).map_err(|e| fail(AttemptOutcome::MayExist, e))
}

fn bad_response(message: String) -> NessieError {
    NessieError::provider(ProviderErrorKind::BadResponse, message)
}

fn transaction_note(sim: Simulation, outcome: SimulatedOutcome, tx: &Transaction) -> String {
    let bank = match sim {
        Simulation::NessieSandbox => "the Capital One Nessie sandbox",
        Simulation::FixtureBank => "the fixture test bank",
    };
    let amount = format_usd(tx.amount_cents);
    match outcome {
        SimulatedOutcome::SimulatedPending => format!(
            "Simulated {} of {amount} is pending in {bank}. Mock money: nothing has been paid.",
            tx.kind
        ),
        SimulatedOutcome::SimulatedCompleted => format!(
            "{bank} reported the simulated {} of {amount} as completed. Mock money: nothing real was paid.",
            tx.kind
        ),
        SimulatedOutcome::SimulatedDeclined => format!(
            "{bank} declined the simulated {} of {amount}{}.",
            tx.kind,
            tx.decision_reason
                .as_deref()
                .filter(|r| !r.is_empty())
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ),
        SimulatedOutcome::SimulatedCancelled => format!(
            "The simulated {} of {amount} was cancelled in {bank}.",
            tx.kind
        ),
        SimulatedOutcome::SimulatedBillRecorded | SimulatedOutcome::Unknown => format!(
            "{bank} reported status {:?} for the simulated {} of {amount}; its meaning is unknown, so it is not treated as completed.",
            tx.status.as_str(),
            tx.kind
        ),
    }
}

fn record_for(
    sim: Simulation,
    approval_id: Uuid,
    intent: &PaymentIntent,
    answer: CallAnswer,
) -> Result<SimulatedPaymentRecord, NessieError> {
    let base = |outcome, provider_status: String, tx_id, bill_id, note| SimulatedPaymentRecord {
        simulation: sim,
        disclaimer: sim.disclaimer(),
        approval_id,
        project_id: intent.project_id,
        property_id: intent.property_id,
        scenario_id: intent.scenario_id,
        plan_item_id: intent.plan_item_id.clone(),
        operation: intent.operation.name(),
        action_type: intent.action,
        moves_money: intent.operation.moves_money(),
        amount_minor: intent.amount_minor,
        currency: intent.currency.clone(),
        outcome,
        provider_status,
        nessie_transaction_id: tx_id,
        nessie_bill_id: bill_id,
        note,
    };
    match (answer, &intent.operation) {
        (CallAnswer::Transaction(tx), PaymentOperation::Purchase { .. })
        | (CallAnswer::Transaction(tx), PaymentOperation::PayBill { .. }) => {
            if tx.amount_cents != intent.amount_minor {
                return Err(bad_response(format!(
                    "the bank reported amount_cents {} for an approved amount of {}",
                    tx.amount_cents, intent.amount_minor
                )));
            }
            let outcome = SimulatedOutcome::from_transaction_status(&tx.status);
            let note = transaction_note(sim, outcome, &tx);
            let bill_id = tx.bill_id.clone();
            Ok(base(
                outcome,
                tx.status.as_str().to_owned(),
                Some(tx.id),
                bill_id,
                note,
            ))
        }
        (CallAnswer::Bill(bill), PaymentOperation::CreateRecurringBill { .. }) => {
            let amount = bill
                .payment_amount_minor()
                .map_err(|e| bad_response(format!("bill payment_amount: {e}")))?;
            if !amount.exact || amount.minor != intent.amount_minor {
                return Err(bad_response(format!(
                    "the bank reported a bill amount of {} dollars for an approved amount of {} cents",
                    bill.payment_amount, intent.amount_minor
                )));
            }
            let (outcome, note) = match bill.status {
                BillStatus::Unknown(ref s) => (
                    SimulatedOutcome::Unknown,
                    format!("The bank reported bill status {s:?}; its meaning is unknown."),
                ),
                ref s => (
                    SimulatedOutcome::SimulatedBillRecorded,
                    format!(
                        "Simulated recurring bill of {} recorded with status {} in {}. No money moved; nothing was committed.",
                        format_usd(intent.amount_minor),
                        s,
                        match sim {
                            Simulation::NessieSandbox => "the Capital One Nessie sandbox",
                            Simulation::FixtureBank => "the fixture test bank",
                        }
                    ),
                ),
            };
            Ok(base(
                outcome,
                bill.status.as_str().to_owned(),
                None,
                Some(bill.id),
                note,
            ))
        }
        _ => Err(bad_response(
            "the bank answered with a different object than the request creates".into(),
        )),
    }
}

/// Live client when `NESSIE_PROVIDER_MODE=live` and `NESSIE_API_KEY` is set; otherwise the
/// fixture bank (whose `/ready` entry then says why).
pub fn select_bank(config: &NessieConfig) -> Arc<dyn NessieBank> {
    match (&config.api_key, config.live_selected()) {
        (Some(key), true) => Arc::new(LiveNessieBank::new(
            &config.base_url,
            key.clone(),
            config.timeout_s,
        )),
        _ => Arc::new(FixtureBank::selected_for(config)),
    }
}
