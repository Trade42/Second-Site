//! The in-memory fixture bank: deterministic, no network, labelled as fixture data.
//!
//! It imitates the documented Nessie v2 behaviour closely enough to exercise the product flow:
//! balances in cents with `pending_debits_cents` holds and `available_cents`; a debit needs
//! available funds (or credit) and is refused with a 422 problem `insufficient_funds` /
//! `credit_limit_exceeded` before any transaction exists; new transactions are `pending` and
//! move money only when settled. Unlike the hosted sandbox it never settles on its own (no
//! clock): a transaction stays pending until [`NessieBank::settle_transaction`].
//!
//! It can never be mistaken for the sandbox: every record is a [`BankRecord`] marked
//! `FIXTURE_IN_MEMORY_BANK` with the fixture disclaimer, every id starts with `fixture_`,
//! account numbers start with `FIXTURE-`, errors name the provider `nessie_fixture`, and
//! timestamps come from a fixed fixture clock starting at 2026-01-01T00:00:00Z.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Duration, TimeZone, Utc};
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{kind_for_http_status, ProviderError};

use crate::approval::PaymentAuthorization;
use crate::bank::{
    execute_with, sealed, ApprovalLedger, CallAnswer, NessieBank, PaymentError, PreparedCall,
    SimulatedPaymentRecord,
};
use crate::config::{NessieConfig, ProviderMode};
use crate::error::{NessieError, ProblemSummary};
use crate::models::{
    validate_id, Account, AccountCreate, AccountType, Bill, BillStatus, Customer, CustomerCreate,
    DepositCreate, Medium, Merchant, MerchantCreate, Transaction, TransactionKind,
    TransactionStatus,
};
use crate::status::{nessie_status, Readiness};
use crate::{BankRecord, Simulation};

/// The provider name in fixture errors.
pub const FIXTURE_PROVIDER: &str = "nessie_fixture";

/// Credit limit Nessie gives a card created without one ($1,000).
pub const DEFAULT_CREDIT_LIMIT_CENTS: i64 = 100_000;

#[derive(Debug, Default)]
struct State {
    seq: u64,
    customers: BTreeMap<String, Customer>,
    accounts: BTreeMap<String, Account>,
    merchants: BTreeMap<String, Merchant>,
    bills: BTreeMap<String, Bill>,
    /// In creation order.
    transactions: Vec<Transaction>,
}

impl State {
    fn tick(&mut self) -> String {
        self.seq += 1;
        let epoch: DateTime<Utc> = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
        let secs = i64::try_from(self.seq).unwrap_or(i64::MAX / 2);
        (epoch + Duration::seconds(secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    fn new_id(&mut self, prefix: &str) -> (String, String) {
        let now = self.tick();
        (format!("fixture_{prefix}_{:04}", self.seq), now)
    }
}

/// The fixture bank. Cheap to create; each instance is an independent bank.
#[derive(Debug)]
pub struct FixtureBank {
    mode: ProviderMode,
    credentials_present: bool,
    readiness: Readiness,
    state: Mutex<State>,
    ledger: ApprovalLedger,
}

impl Default for FixtureBank {
    fn default() -> Self {
        Self::new()
    }
}

fn problem(status: u16, slug: &str, title: &str, detail: impl Into<String>) -> NessieError {
    let detail = detail.into();
    NessieError::Provider {
        error: ProviderError::new(
            FIXTURE_PROVIDER,
            kind_for_http_status(status),
            format!("HTTP {status} {slug}: {title}: {detail}"),
        ),
        http_status: Some(status),
        problem: Some(ProblemSummary {
            http_status: status,
            slug: slug.to_owned(),
            title: title.to_owned(),
            detail: Some(detail),
        }),
    }
}

fn not_found(what: &str, id: &str) -> NessieError {
    problem(404, "not_found", "Not Found", format!("{what} {id} not found"))
}

fn check_id(field: &str, id: &str) -> Result<(), NessieError> {
    validate_id(field, id).map_err(NessieError::InvalidRequest)
}

fn recompute(a: &mut Account) {
    let pending = a.pending_debits_cents.unwrap_or(0);
    a.available_cents = match a.account_type {
        AccountType::CreditCard => {
            a.credit_limit_cents.unwrap_or(DEFAULT_CREDIT_LIMIT_CENTS) - a.balance_cents - pending
        }
        _ => a.balance_cents - pending,
    };
}

fn is_debit(kind: &TransactionKind) -> bool {
    matches!(
        kind,
        TransactionKind::Purchase
            | TransactionKind::BillPayment
            | TransactionKind::Withdrawal
            | TransactionKind::Transfer
            | TransactionKind::Fee
            | TransactionKind::Chargeback
    )
}

impl FixtureBank {
    /// A fixture bank selected by `NESSIE_PROVIDER_MODE=fixture`.
    pub fn new() -> Self {
        Self {
            mode: ProviderMode::Fixture,
            credentials_present: false,
            readiness: Readiness::Fixture,
            state: Mutex::new(State::default()),
            ledger: ApprovalLedger::default(),
        }
    }

    /// The fixture bank chosen for this configuration; its `/ready` entry says why (fixture
    /// mode, or live mode without a key: `NOT_CONFIGURED`).
    pub fn selected_for(config: &NessieConfig) -> Self {
        let readiness = if config.mode == ProviderMode::Live {
            Readiness::NotConfigured
        } else {
            Readiness::Fixture
        };
        Self {
            mode: config.mode,
            credentials_present: config.api_key.is_some(),
            readiness,
            ..Self::new()
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn rec<T>(&self, t: T) -> BankRecord<T> {
        BankRecord::new(Simulation::FixtureBank, t)
    }

    /// Holds `amount` on a debit, or refuses it before a transaction exists.
    fn hold(account: &mut Account, amount: i64) -> Result<(), NessieError> {
        if account.available_cents < amount {
            let (slug, what) = if account.account_type == AccountType::CreditCard {
                ("credit_limit_exceeded", "credit")
            } else {
                ("insufficient_funds", "cents")
            };
            return Err(problem(
                422,
                slug,
                "Unprocessable Entity",
                format!(
                    "Account {} has {} {what} available.",
                    account.id, account.available_cents
                ),
            ));
        }
        account.pending_debits_cents = Some(account.pending_debits_cents.unwrap_or(0) + amount);
        recompute(account);
        Ok(())
    }

    fn release(account: &mut Account, amount: i64) {
        account.pending_debits_cents =
            Some((account.pending_debits_cents.unwrap_or(0) - amount).max(0));
        recompute(account);
    }

    #[allow(clippy::too_many_arguments)]
    fn new_transaction(
        st: &mut State,
        kind: TransactionKind,
        amount: i64,
        payer: Option<String>,
        payee: Option<String>,
        merchant: Option<String>,
        bill: Option<String>,
        description: Option<String>,
    ) -> Transaction {
        let (id, now) = st.new_id("txn");
        let tx = Transaction {
            id,
            kind,
            status: TransactionStatus::Pending,
            medium: Some(Medium::Balance),
            amount_cents: amount,
            payer_id: payer,
            payee_id: payee,
            merchant_id: merchant,
            bill_id: bill,
            parent_transaction_id: None,
            description,
            decision: Some(String::new()),
            decision_reason: None,
            decision_source: Some(String::new()),
            decision_deadline: None,
            settle_at: None,
            created_at: Some(now.clone()),
            updated_at: Some(now),
        };
        st.transactions.push(tx.clone());
        tx
    }

    fn apply(&self, call: &PreparedCall) -> Result<CallAnswer, NessieError> {
        let mut st = self.lock();
        match call {
            PreparedCall::Purchase { account_id, body } => {
                if !st.merchants.contains_key(&body.merchant_id) {
                    return Err(not_found("merchant", &body.merchant_id));
                }
                let account = st
                    .accounts
                    .get_mut(account_id)
                    .ok_or_else(|| not_found("account", account_id))?;
                Self::hold(account, body.amount_cents)?;
                let tx = Self::new_transaction(
                    &mut st,
                    TransactionKind::Purchase,
                    body.amount_cents,
                    Some(account_id.clone()),
                    None,
                    Some(body.merchant_id.clone()),
                    None,
                    body.description.clone(),
                );
                Ok(CallAnswer::Transaction(tx))
            }
            PreparedCall::CreateBill { account_id, body } => {
                if !st.accounts.contains_key(account_id) {
                    return Err(not_found("account", account_id));
                }
                let (id, now) = st.new_id("bill");
                let bill = Bill {
                    id: id.clone(),
                    account_id: Some(account_id.clone()),
                    status: body.status.clone(),
                    payee: body.payee.clone(),
                    nickname: body.nickname.clone(),
                    creation_date: Some(now.get(..10).unwrap_or(&now).to_owned()),
                    payment_date: body.payment_date.clone(),
                    recurring_date: body.recurring_date,
                    upcoming_payment_date: None,
                    payment_amount: body.payment_amount,
                    created_at: Some(now.clone()),
                    updated_at: Some(now),
                };
                st.bills.insert(id, bill.clone());
                Ok(CallAnswer::Bill(bill))
            }
            PreparedCall::PayBill { bill_id, body } => {
                let bill = st
                    .bills
                    .get(bill_id)
                    .cloned()
                    .ok_or_else(|| not_found("bill", bill_id))?;
                let account_id = bill.account_id.clone().unwrap_or_default();
                let amount = match body.amount_cents {
                    Some(a) => a,
                    None => bill.payment_amount_minor()?.minor,
                };
                let account = st
                    .accounts
                    .get_mut(&account_id)
                    .ok_or_else(|| not_found("account", &account_id))?;
                Self::hold(account, amount)?;
                let tx = Self::new_transaction(
                    &mut st,
                    TransactionKind::BillPayment,
                    amount,
                    Some(account_id),
                    None,
                    None,
                    Some(bill_id.clone()),
                    Some(format!("Bill payment to {}", bill.payee)),
                );
                Ok(CallAnswer::Transaction(tx))
            }
        }
    }

    fn find_tx<'a>(st: &'a mut State, id: &str) -> Result<&'a mut Transaction, NessieError> {
        st.transactions
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| not_found("transaction", id))
    }

    /// Cancels a pending transaction and releases its hold (shared by cancel and deletes).
    fn cancel_in(st: &mut State, id: &str, reason: Option<&str>) -> Result<Transaction, NessieError> {
        let now = st.tick();
        let tx = Self::find_tx(st, id)?;
        if tx.status != TransactionStatus::Pending {
            return Err(problem(
                409,
                "not_pending",
                "Conflict",
                format!("Transaction {id} is {}, not pending.", tx.status),
            ));
        }
        tx.status = TransactionStatus::Cancelled;
        tx.decision_reason = reason.map(str::to_owned);
        tx.updated_at = Some(now);
        let tx = tx.clone();
        if is_debit(&tx.kind) {
            if let Some(acct) = tx.payer_id.as_deref().and_then(|p| st.accounts.get_mut(p)) {
                Self::release(acct, tx.amount_cents);
            }
        }
        Ok(tx)
    }
}

impl sealed::Sealed for FixtureBank {}

impl NessieBank for FixtureBank {
    fn simulation(&self) -> Simulation {
        Simulation::FixtureBank
    }

    fn status(&self) -> ProviderStatusEntry {
        nessie_status(self.mode, self.credentials_present, self.readiness, None)
    }

    fn verify(&self) -> Result<(), NessieError> {
        Ok(())
    }

    fn create_customer(&self, body: &CustomerCreate) -> Result<BankRecord<Customer>, NessieError> {
        body.validate().map_err(NessieError::InvalidRequest)?;
        let mut st = self.lock();
        let (id, now) = st.new_id("cust");
        let c = Customer {
            id: id.clone(),
            first_name: body.first_name.clone(),
            last_name: body.last_name.clone(),
            address: Some(body.address.clone()),
            created_at: Some(now.clone()),
            updated_at: Some(now),
        };
        st.customers.insert(id, c.clone());
        Ok(self.rec(c))
    }

    fn get_customer(&self, id: &str) -> Result<BankRecord<Customer>, NessieError> {
        check_id("customer id", id)?;
        let st = self.lock();
        let c = st
            .customers
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("customer", id))?;
        Ok(self.rec(c))
    }

    fn delete_customer(&self, id: &str) -> Result<(), NessieError> {
        check_id("customer id", id)?;
        let owned: Vec<String> = {
            let st = self.lock();
            if !st.customers.contains_key(id) {
                return Err(not_found("customer", id));
            }
            st.accounts
                .values()
                .filter(|a| a.customer_id.as_deref() == Some(id))
                .map(|a| a.id.clone())
                .collect()
        };
        for acct in owned {
            self.delete_account(&acct)?;
        }
        self.lock().customers.remove(id);
        Ok(())
    }

    fn create_account(
        &self,
        customer_id: &str,
        body: &AccountCreate,
    ) -> Result<BankRecord<Account>, NessieError> {
        check_id("customer id", customer_id)?;
        body.validate().map_err(NessieError::InvalidRequest)?;
        let mut st = self.lock();
        if !st.customers.contains_key(customer_id) {
            return Err(not_found("customer", customer_id));
        }
        let (id, now) = st.new_id("acct");
        let credit_limit = (body.account_type == AccountType::CreditCard)
            .then(|| body.credit_limit_cents.unwrap_or(DEFAULT_CREDIT_LIMIT_CENTS));
        let mut a = Account {
            id: id.clone(),
            customer_id: Some(customer_id.to_owned()),
            account_type: body.account_type.clone(),
            nickname: body.nickname.clone(),
            balance_cents: body.balance_cents,
            pending_debits_cents: Some(0),
            available_cents: 0,
            credit_limit_cents: credit_limit,
            account_number: Some(format!("FIXTURE-{:06}", st.seq)),
            rewards: Some(body.rewards.unwrap_or(0)),
            created_at: Some(now.clone()),
            updated_at: Some(now),
        };
        recompute(&mut a);
        st.accounts.insert(id, a.clone());
        Ok(self.rec(a))
    }

    fn get_account(&self, id: &str) -> Result<BankRecord<Account>, NessieError> {
        check_id("account id", id)?;
        let st = self.lock();
        let a = st
            .accounts
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("account", id))?;
        Ok(self.rec(a))
    }

    fn delete_account(&self, id: &str) -> Result<(), NessieError> {
        check_id("account id", id)?;
        let mut st = self.lock();
        if !st.accounts.contains_key(id) {
            return Err(not_found("account", id));
        }
        let pending: Vec<String> = st
            .transactions
            .iter()
            .filter(|t| {
                t.status == TransactionStatus::Pending
                    && (t.payer_id.as_deref() == Some(id) || t.payee_id.as_deref() == Some(id))
            })
            .map(|t| t.id.clone())
            .collect();
        for tx in pending {
            Self::cancel_in(&mut st, &tx, Some("account_deleted"))?;
        }
        st.accounts.remove(id);
        Ok(())
    }

    fn list_account_transactions(
        &self,
        account_id: &str,
        limit: u32,
    ) -> Result<BankRecord<Vec<Transaction>>, NessieError> {
        check_id("account id", account_id)?;
        if !(1..=5000).contains(&limit) {
            return Err(NessieError::InvalidRequest(
                "limit must be 1 to 5000".into(),
            ));
        }
        let st = self.lock();
        if !st.accounts.contains_key(account_id) {
            return Err(not_found("account", account_id));
        }
        let items = st
            .transactions
            .iter()
            .rev()
            .filter(|t| {
                t.payer_id.as_deref() == Some(account_id)
                    || t.payee_id.as_deref() == Some(account_id)
            })
            .take(limit as usize)
            .cloned()
            .collect();
        Ok(self.rec(items))
    }

    fn create_merchant(&self, body: &MerchantCreate) -> Result<BankRecord<Merchant>, NessieError> {
        body.validate().map_err(NessieError::InvalidRequest)?;
        let mut st = self.lock();
        let (id, now) = st.new_id("merch");
        let m = Merchant {
            id: id.clone(),
            name: body.name.clone(),
            category: body.category.clone(),
            address: body.address.clone(),
            geocode: body.geocode,
            created_at: Some(now.clone()),
            updated_at: Some(now),
        };
        st.merchants.insert(id, m.clone());
        Ok(self.rec(m))
    }

    fn get_merchant(&self, id: &str) -> Result<BankRecord<Merchant>, NessieError> {
        check_id("merchant id", id)?;
        let st = self.lock();
        let m = st
            .merchants
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("merchant", id))?;
        Ok(self.rec(m))
    }

    fn delete_merchant(&self, id: &str) -> Result<(), NessieError> {
        check_id("merchant id", id)?;
        let mut st = self.lock();
        st.merchants
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| not_found("merchant", id))
    }

    fn get_bill(&self, id: &str) -> Result<BankRecord<Bill>, NessieError> {
        check_id("bill id", id)?;
        let st = self.lock();
        let b = st
            .bills
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("bill", id))?;
        Ok(self.rec(b))
    }

    fn delete_bill(&self, id: &str) -> Result<(), NessieError> {
        check_id("bill id", id)?;
        let mut st = self.lock();
        st.bills
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| not_found("bill", id))
    }

    fn deposit(
        &self,
        account_id: &str,
        body: &DepositCreate,
    ) -> Result<BankRecord<Transaction>, NessieError> {
        check_id("account id", account_id)?;
        body.validate().map_err(NessieError::InvalidRequest)?;
        let mut st = self.lock();
        if !st.accounts.contains_key(account_id) {
            return Err(not_found("account", account_id));
        }
        // A deposit is money arriving: the path account is the payee, and nothing is held.
        let tx = Self::new_transaction(
            &mut st,
            TransactionKind::Deposit,
            body.amount_cents,
            None,
            Some(account_id.to_owned()),
            None,
            None,
            body.description.clone(),
        );
        Ok(self.rec(tx))
    }

    fn get_transaction(&self, id: &str) -> Result<BankRecord<Transaction>, NessieError> {
        check_id("transaction id", id)?;
        let mut st = self.lock();
        let tx = Self::find_tx(&mut st, id)?.clone();
        Ok(self.rec(tx))
    }

    fn settle_transaction(&self, id: &str) -> Result<BankRecord<Transaction>, NessieError> {
        check_id("transaction id", id)?;
        let mut st = self.lock();
        let now = st.tick();
        let tx = Self::find_tx(&mut st, id)?;
        if tx.status != TransactionStatus::Pending {
            // Already settled or ended: answer with the transaction as it is.
            let tx = tx.clone();
            return Ok(self.rec(tx));
        }
        tx.status = TransactionStatus::Completed;
        tx.decision = Some("approve".into());
        tx.decision_source = Some("owner".into());
        tx.updated_at = Some(now);
        let tx = tx.clone();
        if is_debit(&tx.kind) {
            if let Some(a) = tx.payer_id.as_deref().and_then(|p| st.accounts.get_mut(p)) {
                a.pending_debits_cents =
                    Some((a.pending_debits_cents.unwrap_or(0) - tx.amount_cents).max(0));
                if a.account_type == AccountType::CreditCard {
                    a.balance_cents += tx.amount_cents;
                } else {
                    a.balance_cents -= tx.amount_cents;
                }
                recompute(a);
            }
        } else if let Some(a) = tx.payee_id.as_deref().and_then(|p| st.accounts.get_mut(p)) {
            if a.account_type == AccountType::CreditCard {
                a.balance_cents -= tx.amount_cents;
            } else {
                a.balance_cents += tx.amount_cents;
            }
            recompute(a);
        }
        if let Some(bill) = tx.bill_id.as_deref().and_then(|b| st.bills.get_mut(b)) {
            // Assumption (not documented): a paid one-off bill becomes `completed`; a
            // recurring bill stays `recurring`.
            if bill.status != BillStatus::Recurring {
                bill.status = BillStatus::Completed;
            }
        }
        Ok(self.rec(tx))
    }

    fn cancel_transaction(&self, id: &str) -> Result<BankRecord<Transaction>, NessieError> {
        check_id("transaction id", id)?;
        let mut st = self.lock();
        let tx = Self::cancel_in(&mut st, id, None)?;
        Ok(self.rec(tx))
    }

    fn execute(&self, auth: PaymentAuthorization) -> Result<SimulatedPaymentRecord, PaymentError> {
        execute_with(Simulation::FixtureBank, &self.ledger, auth, |call| {
            self.apply(call)
        })
    }
}
