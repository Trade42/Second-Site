//! Typed models of the Nessie v2 objects this crate uses (OpenAPI 2.1.3 component schemas, see
//! `apps/backend/tests-golden/nessie/schemas.json`).
//!
//! - **Responses** tolerate unknown fields (it is a third-party API that may grow), and an enum
//!   value this crate does not know is kept verbatim as `Unknown(..)` rather than failing or
//!   being guessed. In particular an unknown transaction status is never read as completed.
//!   Only the fields this crate relies on are required (ids, kinds, statuses, amounts); the rest
//!   default when absent.
//! - **Requests** are exact: `deny_unknown_fields`, optional fields omitted when unset, and a
//!   `validate()` that applies the documented limits (lengths, patterns, minimums) before
//!   anything is sent.
//!
//! Amounts are integer cents (`*_cents`), which are minor units. A bill's `payment_amount` is a
//! dollar float; convert it only with [`crate::money`].

use serde::{Deserialize, Deserializer, Serialize};

use crate::money::{self, DollarConversion, MoneyError};

macro_rules! open_enum {
    ($(#[$m:meta])* pub enum $name:ident { $($(#[$vm:meta])* $var:ident = $s:literal,)* }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vm])* $var,)*
            /// A value this crate does not know, kept verbatim.
            Unknown(String),
        }

        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $($name::$var => $s,)*
                    $name::Unknown(s) => s,
                }
            }

            pub fn from_wire(s: &str) -> Self {
                match s {
                    $($s => $name::$var,)*
                    other => $name::Unknown(other.to_owned()),
                }
            }

            pub fn is_known(&self) -> bool {
                !matches!(self, $name::Unknown(_))
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Ok(Self::from_wire(&s))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

open_enum! {
    /// `AccountDTO.type`.
    pub enum AccountType {
        Checking = "Checking",
        Savings = "Savings",
        CreditCard = "Credit Card",
    }
}

open_enum! {
    /// `TransactionDTO.kind`.
    pub enum TransactionKind {
        Deposit = "deposit",
        Withdrawal = "withdrawal",
        Transfer = "transfer",
        Purchase = "purchase",
        BillPayment = "bill_payment",
        Fee = "fee",
        InterestCredit = "interest_credit",
        Refund = "refund",
        Chargeback = "chargeback",
    }
}

open_enum! {
    /// `TransactionDTO.status`: `create -> pending -> completed | declined`, or `cancelled`.
    pub enum TransactionStatus {
        Pending = "pending",
        Completed = "completed",
        Declined = "declined",
        Cancelled = "cancelled",
    }
}

open_enum! {
    /// `TransactionDTO.medium`.
    pub enum Medium {
        Balance = "balance",
        Rewards = "rewards",
    }
}

open_enum! {
    /// `BillDTO.status` / `BillCreateDTO.status`.
    pub enum BillStatus {
        Pending = "pending",
        Cancelled = "cancelled",
        Completed = "completed",
        Recurring = "recurring",
    }
}

/// `items: array | null` in every list body: null reads as empty.
fn null_as_empty<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn check_len(field: &str, value: &str, min: usize, max: usize) -> Result<(), String> {
    let n = chars(value);
    if n < min || n > max {
        Err(format!(
            "{field} must be {min} to {max} characters, got {n}"
        ))
    } else {
        Ok(())
    }
}

fn check_id(field: &str, value: &str) -> Result<(), String> {
    // Ids go into URL paths: refuse anything that could change the path or add a query.
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Err(format!(
            "{field} must be a non-empty Nessie id of letters, digits, '_' or '-'"
        ))
    } else {
        Ok(())
    }
}

/// Validates an id before it is placed in a request path.
pub fn validate_id(field: &str, value: &str) -> Result<(), String> {
    check_id(field, value)
}

fn check_amount(field: &str, cents: i64) -> Result<(), String> {
    if cents < 1 {
        Err(format!("{field} must be at least 1 cent, got {cents}"))
    } else {
        Ok(())
    }
}

fn check_description(d: &Option<String>) -> Result<(), String> {
    match d {
        Some(s) => check_len("description", s, 0, 200),
        None => Ok(()),
    }
}

// ---- Address / Geocode (shared by requests and responses) ---------------------------------------

/// `AddressDTO`. All five fields are required on input; a response missing one reads as "".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    #[serde(default)]
    pub street_number: String,
    #[serde(default)]
    pub street_name: String,
    #[serde(default)]
    pub city: String,
    /// Two-letter state code.
    #[serde(default)]
    pub state: String,
    /// Five digits.
    #[serde(default)]
    pub zip: String,
}

impl Address {
    pub fn validate(&self) -> Result<(), String> {
        check_len("address.street_number", &self.street_number, 0, 12)?;
        check_len("address.street_name", &self.street_name, 0, 80)?;
        check_len("address.city", &self.city, 0, 80)?;
        check_len("address.state", &self.state, 2, 2)?;
        if self.zip.len() != 5 || !self.zip.bytes().all(|b| b.is_ascii_digit()) {
            return Err("address.zip must be five digits".into());
        }
        Ok(())
    }
}

/// `GeocodeDTO`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Geocode {
    pub lat: f64,
    pub lng: f64,
}

impl Geocode {
    pub fn validate(&self) -> Result<(), String> {
        if !(self.lat.is_finite() && (-90.0..=90.0).contains(&self.lat)) {
            return Err("geocode.lat must be within -90..90".into());
        }
        if !(self.lng.is_finite() && (-180.0..=180.0).contains(&self.lng)) {
            return Err("geocode.lng must be within -180..180".into());
        }
        Ok(())
    }
}

// ---- Customers ------------------------------------------------------------------------------------

/// `CustomerCreateDTO` (POST /customers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomerCreate {
    pub first_name: String,
    pub last_name: String,
    pub address: Address,
}

impl CustomerCreate {
    pub fn validate(&self) -> Result<(), String> {
        check_len("first_name", &self.first_name, 1, 80)?;
        check_len("last_name", &self.last_name, 1, 80)?;
        self.address.validate()
    }
}

/// `CustomerDTO`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Customer {
    pub id: String,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: String,
    #[serde(default)]
    pub address: Option<Address>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

// ---- Accounts -------------------------------------------------------------------------------------

/// `AccountCreateDTO` (POST /customers/{id}/accounts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountCreate {
    #[serde(rename = "type")]
    pub account_type: AccountType,
    pub nickname: String,
    /// Opening balance of mock money, cents, `>= 0`.
    pub balance_cents: i64,
    /// Credit cards only (`>= 1`; Nessie defaults it to 100000). Rejected for other types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_limit_cents: Option<i64>,
    /// Reward points, `>= 0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewards: Option<i64>,
}

impl AccountCreate {
    pub fn validate(&self) -> Result<(), String> {
        if !self.account_type.is_known() {
            return Err(format!("unknown account type {}", self.account_type));
        }
        check_len("nickname", &self.nickname, 1, 40)?;
        if self.balance_cents < 0 {
            return Err("balance_cents must not be negative".into());
        }
        match (self.credit_limit_cents, &self.account_type) {
            (Some(l), AccountType::CreditCard) if l < 1 => {
                return Err("credit_limit_cents must be at least 1".into())
            }
            (Some(_), t) if *t != AccountType::CreditCard => {
                return Err("credit_limit_cents is only for Credit Card accounts".into())
            }
            _ => {}
        }
        if self.rewards.is_some_and(|r| r < 0) {
            return Err("rewards must not be negative".into());
        }
        Ok(())
    }
}

/// `AccountDTO`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    #[serde(default)]
    pub customer_id: Option<String>,
    #[serde(rename = "type")]
    pub account_type: AccountType,
    #[serde(default)]
    pub nickname: String,
    /// Posted money; for a credit card, the amount owed.
    pub balance_cents: i64,
    /// Sum of pending debits (holds).
    #[serde(default)]
    pub pending_debits_cents: Option<i64>,
    /// Spendable now: balance minus holds, or remaining credit on a card.
    pub available_cents: i64,
    #[serde(default)]
    pub credit_limit_cents: Option<i64>,
    #[serde(default)]
    pub account_number: Option<String>,
    #[serde(default)]
    pub rewards: Option<i64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

// ---- Merchants ------------------------------------------------------------------------------------

/// `MerchantCreateDTO` (POST /merchants).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MerchantCreate {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geocode: Option<Geocode>,
}

impl MerchantCreate {
    pub fn validate(&self) -> Result<(), String> {
        check_len("name", &self.name, 1, 80)?;
        if let Some(c) = &self.category {
            check_len("category", c, 0, 40)?;
        }
        if let Some(a) = &self.address {
            a.validate()?;
        }
        if let Some(g) = &self.geocode {
            g.validate()?;
        }
        Ok(())
    }
}

/// `MerchantDTO`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Merchant {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub address: Option<Address>,
    #[serde(default)]
    pub geocode: Option<Geocode>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

// ---- Money movements ------------------------------------------------------------------------------

/// `PurchaseCreateDTO` (POST /accounts/{id}/purchases; the path account pays).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurchaseCreate {
    pub merchant_id: String,
    pub amount_cents: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl PurchaseCreate {
    pub fn validate(&self) -> Result<(), String> {
        check_id("merchant_id", &self.merchant_id)?;
        check_amount("amount_cents", self.amount_cents)?;
        check_description(&self.description)
    }
}

/// `TransferCreateDTO` (POST /accounts/{id}/transfers). Modelled for completeness; the move-in
/// plan does not use transfers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferCreate {
    pub payee_id: String,
    pub amount_cents: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl TransferCreate {
    pub fn validate(&self) -> Result<(), String> {
        check_id("payee_id", &self.payee_id)?;
        check_amount("amount_cents", self.amount_cents)?;
        check_description(&self.description)
    }
}

/// `DepositCreateDTO` (POST /accounts/{id}/deposits): mock money arriving in a sandbox account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepositCreate {
    pub amount_cents: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl DepositCreate {
    pub fn validate(&self) -> Result<(), String> {
        check_amount("amount_cents", self.amount_cents)?;
        check_description(&self.description)
    }
}

/// `WithdrawalCreateDTO` (POST /accounts/{id}/withdrawals). Modelled for completeness; not used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawalCreate {
    pub amount_cents: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl WithdrawalCreate {
    pub fn validate(&self) -> Result<(), String> {
        check_amount("amount_cents", self.amount_cents)?;
        check_description(&self.description)
    }
}

/// `TransactionDTO`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    pub id: String,
    pub kind: TransactionKind,
    pub status: TransactionStatus,
    #[serde(default)]
    pub medium: Option<Medium>,
    pub amount_cents: i64,
    #[serde(default)]
    pub payer_id: Option<String>,
    #[serde(default)]
    pub payee_id: Option<String>,
    #[serde(default)]
    pub merchant_id: Option<String>,
    #[serde(default)]
    pub bill_id: Option<String>,
    #[serde(default)]
    pub parent_transaction_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `""`, `approve` or `decline`.
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub decision_reason: Option<String>,
    /// `""`, `owner`, `hook` or `on_error`.
    #[serde(default)]
    pub decision_source: Option<String>,
    #[serde(default)]
    pub decision_deadline: Option<String>,
    /// When the pending window closes.
    #[serde(default)]
    pub settle_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

// ---- Bills ----------------------------------------------------------------------------------------

/// `BillCreateDTO` (POST /accounts/{id}/bills). `payment_amount` is in **dollars**; build it with
/// [`BillCreate::new_minor`] so the amount comes from integer cents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillCreate {
    pub status: BillStatus,
    pub payee: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    pub payment_amount: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment_date: Option<String>,
    /// Day of the month, 1 to 31.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurring_date: Option<i64>,
}

impl BillCreate {
    pub fn new_minor(
        status: BillStatus,
        payee: String,
        nickname: Option<String>,
        amount_minor: i64,
        recurring_date: Option<i64>,
    ) -> Result<Self, MoneyError> {
        if amount_minor < 0 {
            return Err(MoneyError::Negative(amount_minor.to_string()));
        }
        Ok(Self {
            status,
            payee,
            nickname,
            payment_amount: money::minor_to_dollars(amount_minor)?,
            payment_date: None,
            recurring_date,
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.status.is_known() {
            return Err(format!("unknown bill status {}", self.status));
        }
        check_len("payee", &self.payee, 1, 80)?;
        if let Some(n) = &self.nickname {
            check_len("nickname", n, 0, 80)?;
        }
        money::non_negative_dollars_to_minor(self.payment_amount)
            .map_err(|e| format!("payment_amount: {e}"))?;
        if let Some(d) = self.recurring_date {
            if !(1..=31).contains(&d) {
                return Err("recurring_date must be a day of the month, 1 to 31".into());
            }
        }
        Ok(())
    }
}

/// `BillDTO`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bill {
    pub id: String,
    #[serde(default)]
    pub account_id: Option<String>,
    pub status: BillStatus,
    #[serde(default)]
    pub payee: String,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub creation_date: Option<String>,
    #[serde(default)]
    pub payment_date: Option<String>,
    #[serde(default)]
    pub recurring_date: Option<i64>,
    #[serde(default)]
    pub upcoming_payment_date: Option<String>,
    /// Dollars (a float). Use [`Bill::payment_amount_minor`].
    pub payment_amount: f64,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl Bill {
    /// The bill amount in cents, with the rounding made visible (`exact == false`).
    pub fn payment_amount_minor(&self) -> Result<DollarConversion, MoneyError> {
        money::non_negative_dollars_to_minor(self.payment_amount)
    }
}

/// `BillPaymentCreateDTO` (POST /bills/{id}/payments). This crate always sends the approved
/// amount explicitly rather than relying on "defaults to the bill amount".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillPaymentCreate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_cents: Option<i64>,
}

impl BillPaymentCreate {
    pub fn validate(&self) -> Result<(), String> {
        match self.amount_cents {
            Some(c) => check_amount("amount_cents", c),
            None => Ok(()),
        }
    }
}

// ---- Lists, events, errors, health ------------------------------------------------------------------

/// `ListResponse<X>DTOBody`: one page and an opaque cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    #[serde(default, deserialize_with = "null_as_empty")]
    pub items: Vec<T>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// `TransactionListResponseBody` (GET /accounts/{id}/transactions: newest first, no cursor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionList {
    #[serde(default, deserialize_with = "null_as_empty")]
    pub items: Vec<Transaction>,
}

/// `EventDTO`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub seq: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub tx_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// `EventsListResponseBody`: pass `next_cursor` back as `after`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventPage {
    #[serde(default, deserialize_with = "null_as_empty")]
    pub items: Vec<Event>,
    #[serde(default)]
    pub next_cursor: Option<i64>,
}

/// `ErrorModel`: RFC 9457 problem+json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    #[serde(rename = "type", default)]
    pub problem_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub errors: Vec<ProblemFieldError>,
}

impl Problem {
    /// The stable slug to switch on: the last path segment of `type`
    /// (`https://nessieisreal.com/problems/insufficient_funds` -> `insufficient_funds`).
    pub fn slug(&self) -> &str {
        let t = self.problem_type.trim_end_matches('/');
        t.rsplit('/').next().unwrap_or(t)
    }
}

/// `ErrorDetail`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProblemFieldError {
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub value: serde_json::Value,
}

/// `HealthDTO` (GET /health, public: it says nothing about whether a key works).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub uptime_seconds: Option<i64>,
}
