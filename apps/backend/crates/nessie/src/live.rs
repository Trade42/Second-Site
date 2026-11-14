//! The live client for the Capital One Nessie v2 sandbox (blocking `ureq`, as ss-server's other
//! live providers; call it from async code on a blocking thread).
//!
//! - The key goes only where the API requires it: `Authorization: Bearer <key>` ("Keys never
//!   go in the query string"). It is never in a URL, a log line (ureq masks the
//!   `Authorization` value in its debug log), an error message or `Debug` output; every message
//!   built from a response is scrubbed of it as well.
//! - One timeout for connect, read and write each (default 20 s); redirects are not followed;
//!   any non-2xx is an error, with kinds mapped as the other providers (401/403 AUTH, 404
//!   NOT_FOUND, 429 RATE_LIMITED, other >= 500 UNAVAILABLE, else BAD_RESPONSE; timeout TIMEOUT,
//!   transport UNAVAILABLE) plus Nessie's RFC 9457 problem slug.
//! - Only GETs are retried (RATE_LIMITED, TIMEOUT, UNAVAILABLE; 3 attempts, 0.5 s doubling
//!   backoff). Creates are sent once: Nessie has no idempotency key, so "a create that timed out
//!   may still have happened, and sending it again is a second transaction".
//! - `/ready` reports `LIVE` only after an authenticated call succeeded (see [`crate::status`]).

use std::io::Read;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{
    kind_for_http_status, ProviderError, ProviderErrorKind, RetryPolicy,
};

use crate::approval::PaymentAuthorization;
use crate::bank::{
    execute_with, sealed, ApprovalLedger, CallAnswer, NessieBank, PaymentError, PreparedCall,
    SimulatedPaymentRecord,
};
use crate::config::{normalize_base_url, ApiKey, NessieConfig, ProviderMode};
use crate::error::{truncate, NessieError, ProblemSummary, PROVIDER_NAME};
use crate::models::{
    validate_id, Account, AccountCreate, Bill, Customer, CustomerCreate, DepositCreate, Health,
    Merchant, MerchantCreate, Page, Problem, Transaction, TransactionList,
};
use crate::status::{nessie_status, Readiness};
use crate::{BankRecord, Simulation};

/// Success bodies larger than this are refused (BAD_RESPONSE) rather than buffered unbounded.
pub const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;
/// Error bodies are read up to this size for the problem details.
pub const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
struct HealthState {
    readiness: Readiness,
    detail: Option<String>,
}

/// The live Nessie client. Construct with [`LiveNessieBank::new`] or
/// [`LiveNessieBank::from_config`]; nothing is sent until a method is called.
pub struct LiveNessieBank {
    base_url: String,
    key: ApiKey,
    agent: ureq::Agent,
    policy: RetryPolicy,
    ledger: ApprovalLedger,
    health: Mutex<HealthState>,
}

impl std::fmt::Debug for LiveNessieBank {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveNessieBank")
            .field("base_url", &self.base_url)
            .field("key", &self.key)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

fn timeout(policy: &RetryPolicy) -> Duration {
    if policy.timeout_s.is_finite() && policy.timeout_s > 0.0 {
        Duration::from_secs_f64(policy.timeout_s)
    } else {
        Duration::from_secs(20)
    }
}

fn is_timeout(t: &ureq::Transport) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(t);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

fn read_limited(resp: ureq::Response, limit: u64) -> Result<Vec<u8>, std::io::Error> {
    let mut body = Vec::new();
    resp.into_reader().take(limit + 1).read_to_end(&mut body)?;
    Ok(body)
}

fn path_id(field: &str, id: &str) -> Result<(), NessieError> {
    validate_id(field, id).map_err(NessieError::InvalidRequest)
}

impl LiveNessieBank {
    /// `base_url` as normalized by [`NessieConfig`] (an invalid one makes every call fail with
    /// BAD_RESPONSE rather than panicking).
    pub fn new(base_url: &str, key: ApiKey, timeout_s: f64) -> Self {
        Self::with_policy(
            base_url,
            key,
            RetryPolicy {
                attempts: 3,
                backoff_s: 0.5,
                timeout_s,
            },
        )
    }

    pub fn with_policy(base_url: &str, key: ApiKey, policy: RetryPolicy) -> Self {
        let t = timeout(&policy);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(t)
            .timeout_read(t)
            .timeout_write(t)
            .redirects(0)
            .user_agent(concat!(
                "second-site-backend/",
                env!("CARGO_PKG_VERSION"),
                " (ss-nessie)"
            ))
            .build();
        Self {
            base_url: normalize_base_url(base_url).unwrap_or_default(),
            key,
            agent,
            policy,
            ledger: ApprovalLedger::default(),
            health: Mutex::new(HealthState {
                readiness: Readiness::ConfiguredUnverified,
                detail: None,
            }),
        }
    }

    /// The live client for a configuration that selects it.
    pub fn from_config(config: &NessieConfig) -> Result<Self, String> {
        let key = config
            .api_key
            .clone()
            .ok_or_else(|| "NESSIE_API_KEY is not set".to_owned())?;
        let base = normalize_base_url(&config.base_url)?;
        Ok(Self::new(&base, key, config.timeout_s))
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn health_lock(&self) -> MutexGuard<'_, HealthState> {
        self.health.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn scrub(&self, s: &str) -> String {
        let k = self.key.expose();
        if k.is_empty() {
            s.to_owned()
        } else {
            s.replace(k, "***")
        }
    }

    fn error(&self, kind: ProviderErrorKind, message: &str) -> NessieError {
        NessieError::Provider {
            error: ProviderError::new(PROVIDER_NAME, kind, self.scrub(message)),
            http_status: None,
            problem: None,
        }
    }

    fn status_error(&self, code: u16, body: Option<Vec<u8>>) -> NessieError {
        let problem = body
            .as_deref()
            .and_then(|b| serde_json::from_slice::<Problem>(b).ok())
            .filter(|p| !p.problem_type.is_empty() || !p.title.is_empty())
            .map(|p| {
                Box::new(ProblemSummary {
                    http_status: code,
                    slug: truncate(&self.scrub(p.slug()), 80),
                    title: truncate(&self.scrub(&p.title), 120),
                    detail: p.detail.as_deref().map(|d| truncate(&self.scrub(d), 300)),
                })
            });
        let mut message = format!("HTTP {code}");
        if let Some(p) = &problem {
            message.push_str(&format!(" {}: {}", p.slug, p.title));
            if let Some(d) = &p.detail {
                message.push_str(&format!(" ({d})"));
            }
        }
        NessieError::Provider {
            error: ProviderError::new(PROVIDER_NAME, kind_for_http_status(code), message),
            http_status: Some(code),
            problem,
        }
    }

    fn transport_error(&self, t: &ureq::Transport) -> NessieError {
        if is_timeout(t) {
            return self.error(ProviderErrorKind::Timeout, "timed out");
        }
        // The kind and the I/O cause only: never the URL (ureq's Display includes it).
        let mut msg = t.kind().to_string();
        if let Some(src) = std::error::Error::source(t) {
            msg.push_str(": ");
            msg.push_str(&src.to_string());
        }
        self.error(ProviderErrorKind::Unavailable, &msg)
    }

    fn note_outcome(&self, result: &Result<Vec<u8>, NessieError>) {
        let mut h = self.health_lock();
        match result {
            Ok(_) => {
                h.readiness = Readiness::Verified;
                h.detail = Some(format!(
                    "Last success {}.",
                    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
                ));
            }
            Err(e) => match (e.kind(), e.http_status()) {
                (Some(ProviderErrorKind::Auth), _) => {
                    h.readiness = Readiness::KeyRejected;
                    h.detail = e.http_status().map(|s| format!("Last answer HTTP {s}."));
                }
                (Some(ProviderErrorKind::Timeout), _)
                | (Some(ProviderErrorKind::Unavailable), _) => {
                    h.readiness = Readiness::LastCallFailed;
                    h.detail = e.kind().map(|k| format!("Last failure {}.", k.as_str()));
                }
                // Other refusals (404, 422, ...) say nothing about the key; leave it as it was.
                _ => {}
            },
        }
    }

    /// One request; the body is JSON. `authenticated: false` only for the public `/health`.
    fn send_once(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&str>,
        authenticated: bool,
    ) -> Result<Vec<u8>, NessieError> {
        if self.base_url.is_empty() {
            return Err(self.error(
                ProviderErrorKind::BadResponse,
                "the configured base URL is not an http(s) URL",
            ));
        }
        let url = format!("{}{path}", self.base_url);
        let mut req = self
            .agent
            .request(method, &url)
            .set("Accept", "application/json");
        if authenticated {
            req = req.set("Authorization", &format!("Bearer {}", self.key.expose()));
        }
        for (k, v) in query {
            req = req.query(k, v);
        }
        let result = match body {
            Some(text) => req
                .set("Content-Type", "application/json")
                .send_string(text),
            None => req.call(),
        };
        let out = match result {
            Ok(resp) if (200..300).contains(&resp.status()) => read_limited(resp, MAX_BODY_BYTES)
                .map_err(|e| {
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) {
                        self.error(ProviderErrorKind::Timeout, "timed out")
                    } else {
                        self.error(
                            ProviderErrorKind::Unavailable,
                            &format!("reading the response failed: {e}"),
                        )
                    }
                })
                .and_then(|b| {
                    if b.len() as u64 > MAX_BODY_BYTES {
                        Err(self.error(
                            ProviderErrorKind::BadResponse,
                            &format!("response larger than {MAX_BODY_BYTES} bytes"),
                        ))
                    } else {
                        Ok(b)
                    }
                }),
            Ok(resp) => Err(self.status_error(resp.status(), None)),
            Err(ureq::Error::Status(code, resp)) => {
                let body = read_limited(resp, MAX_ERROR_BODY_BYTES).ok();
                Err(self.status_error(code, body))
            }
            Err(ureq::Error::Transport(t)) => Err(self.transport_error(&t)),
        };
        if authenticated {
            self.note_outcome(&out);
        }
        out
    }

    fn parse<T: DeserializeOwned>(&self, what: &str, body: &[u8]) -> Result<T, NessieError> {
        serde_json::from_slice(body).map_err(|e| {
            self.error(
                ProviderErrorKind::BadResponse,
                &format!("response is not a valid {what}: {e}"),
            )
        })
    }

    /// GET with the retry policy.
    fn get<T: DeserializeOwned>(
        &self,
        what: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, NessieError> {
        let mut last = None;
        for attempt in 0..self.policy.attempts.max(1) {
            match self.send_once("GET", path, query, None, true) {
                Ok(body) => return self.parse(what, &body),
                Err(e) if !e.retryable() => return Err(e),
                Err(e) => last = Some(e),
            }
            if attempt + 1 < self.policy.attempts {
                let pause = self.policy.backoff_s * f64::from(2u32.saturating_pow(attempt));
                if pause.is_finite() && pause > 0.0 {
                    std::thread::sleep(Duration::from_secs_f64(pause));
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            self.error(
                ProviderErrorKind::Unavailable,
                "retry policy allows no attempts",
            )
        }))
    }

    /// POST, sent once.
    fn post<T: DeserializeOwned, B: Serialize>(
        &self,
        what: &str,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, NessieError> {
        // Serialized straight from the struct: field order is the declared (documented) order,
        // whatever serde_json features other crates enable.
        let text = match body {
            Some(b) => Some(
                serde_json::to_string(b)
                    .map_err(|e| NessieError::InvalidRequest(format!("request body: {e}")))?,
            ),
            None => None,
        };
        let out = self.send_once("POST", path, &[], text.as_deref(), true)?;
        self.parse(what, &out)
    }

    /// DELETE, sent once (204 No Content).
    fn delete(&self, path: &str) -> Result<(), NessieError> {
        self.send_once("DELETE", path, &[], None, true).map(|_| ())
    }

    fn rec<T>(&self, t: T) -> BankRecord<T> {
        BankRecord::new(Simulation::NessieSandbox, t)
    }

    /// `GET /health` (public, sent without the key). It says the API is up, not that the key
    /// works, so it never changes the `/ready` status.
    pub fn health(&self) -> Result<Health, NessieError> {
        let body = self.send_once("GET", "/health", &[], None, false)?;
        self.parse("HealthDTO", &body)
    }

    /// `GET /accounts` (one page, oldest first; `limit` 1..100).
    pub fn list_accounts(
        &self,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<BankRecord<Page<Account>>, NessieError> {
        if !(1..=100).contains(&limit) {
            return Err(NessieError::InvalidRequest("limit must be 1 to 100".into()));
        }
        let mut q = vec![("limit", limit.to_string())];
        if let Some(c) = cursor {
            q.push(("cursor", c.to_owned()));
        }
        self.get("ListResponseAccountDTOBody", "/accounts", &q)
            .map(|p| self.rec(p))
    }
}

impl sealed::Sealed for LiveNessieBank {}

impl NessieBank for LiveNessieBank {
    fn simulation(&self) -> Simulation {
        Simulation::NessieSandbox
    }

    fn status(&self) -> ProviderStatusEntry {
        let h = self.health_lock().clone();
        nessie_status(ProviderMode::Live, true, h.readiness, h.detail.as_deref())
    }

    fn verify(&self) -> Result<(), NessieError> {
        self.list_accounts(1, None).map(|_| ())
    }

    fn create_customer(&self, body: &CustomerCreate) -> Result<BankRecord<Customer>, NessieError> {
        body.validate().map_err(NessieError::InvalidRequest)?;
        self.post("CustomerDTO", "/customers", Some(body))
            .map(|c| self.rec(c))
    }

    fn get_customer(&self, id: &str) -> Result<BankRecord<Customer>, NessieError> {
        path_id("customer id", id)?;
        self.get("CustomerDTO", &format!("/customers/{id}"), &[])
            .map(|c| self.rec(c))
    }

    fn delete_customer(&self, id: &str) -> Result<(), NessieError> {
        path_id("customer id", id)?;
        self.delete(&format!("/customers/{id}"))
    }

    fn create_account(
        &self,
        customer_id: &str,
        body: &AccountCreate,
    ) -> Result<BankRecord<Account>, NessieError> {
        path_id("customer id", customer_id)?;
        body.validate().map_err(NessieError::InvalidRequest)?;
        self.post(
            "AccountDTO",
            &format!("/customers/{customer_id}/accounts"),
            Some(body),
        )
        .map(|a| self.rec(a))
    }

    fn get_account(&self, id: &str) -> Result<BankRecord<Account>, NessieError> {
        path_id("account id", id)?;
        self.get("AccountDTO", &format!("/accounts/{id}"), &[])
            .map(|a| self.rec(a))
    }

    fn delete_account(&self, id: &str) -> Result<(), NessieError> {
        path_id("account id", id)?;
        self.delete(&format!("/accounts/{id}"))
    }

    fn list_account_transactions(
        &self,
        account_id: &str,
        limit: u32,
    ) -> Result<BankRecord<Vec<Transaction>>, NessieError> {
        path_id("account id", account_id)?;
        if !(1..=5000).contains(&limit) {
            return Err(NessieError::InvalidRequest(
                "limit must be 1 to 5000".into(),
            ));
        }
        self.get::<TransactionList>(
            "TransactionListResponseBody",
            &format!("/accounts/{account_id}/transactions"),
            &[("limit", limit.to_string())],
        )
        .map(|l| self.rec(l.items))
    }

    fn create_merchant(&self, body: &MerchantCreate) -> Result<BankRecord<Merchant>, NessieError> {
        body.validate().map_err(NessieError::InvalidRequest)?;
        self.post("MerchantDTO", "/merchants", Some(body))
            .map(|m| self.rec(m))
    }

    fn get_merchant(&self, id: &str) -> Result<BankRecord<Merchant>, NessieError> {
        path_id("merchant id", id)?;
        self.get("MerchantDTO", &format!("/merchants/{id}"), &[])
            .map(|m| self.rec(m))
    }

    fn delete_merchant(&self, id: &str) -> Result<(), NessieError> {
        path_id("merchant id", id)?;
        self.delete(&format!("/merchants/{id}"))
    }

    fn get_bill(&self, id: &str) -> Result<BankRecord<Bill>, NessieError> {
        path_id("bill id", id)?;
        self.get("BillDTO", &format!("/bills/{id}"), &[])
            .map(|b| self.rec(b))
    }

    fn delete_bill(&self, id: &str) -> Result<(), NessieError> {
        path_id("bill id", id)?;
        self.delete(&format!("/bills/{id}"))
    }

    fn deposit(
        &self,
        account_id: &str,
        body: &DepositCreate,
    ) -> Result<BankRecord<Transaction>, NessieError> {
        path_id("account id", account_id)?;
        body.validate().map_err(NessieError::InvalidRequest)?;
        self.post(
            "TransactionDTO",
            &format!("/accounts/{account_id}/deposits"),
            Some(body),
        )
        .map(|t| self.rec(t))
    }

    fn get_transaction(&self, id: &str) -> Result<BankRecord<Transaction>, NessieError> {
        path_id("transaction id", id)?;
        self.get("TransactionDTO", &format!("/transactions/{id}"), &[])
            .map(|t| self.rec(t))
    }

    fn settle_transaction(&self, id: &str) -> Result<BankRecord<Transaction>, NessieError> {
        path_id("transaction id", id)?;
        self.post::<Transaction, ()>(
            "TransactionDTO",
            &format!("/transactions/{id}/settle"),
            None,
        )
        .map(|t| self.rec(t))
    }

    fn cancel_transaction(&self, id: &str) -> Result<BankRecord<Transaction>, NessieError> {
        path_id("transaction id", id)?;
        self.post::<Transaction, ()>(
            "TransactionDTO",
            &format!("/transactions/{id}/cancel"),
            None,
        )
        .map(|t| self.rec(t))
    }

    fn execute(&self, auth: PaymentAuthorization) -> Result<SimulatedPaymentRecord, PaymentError> {
        execute_with(
            Simulation::NessieSandbox,
            &self.ledger,
            auth,
            |call| match call {
                PreparedCall::Purchase { account_id, body } => {
                    path_id("account id", account_id)?;
                    self.post(
                        "TransactionDTO",
                        &format!("/accounts/{account_id}/purchases"),
                        Some(body),
                    )
                    .map(CallAnswer::Transaction)
                }
                PreparedCall::CreateBill { account_id, body } => {
                    path_id("account id", account_id)?;
                    self.post(
                        "BillDTO",
                        &format!("/accounts/{account_id}/bills"),
                        Some(body),
                    )
                    .map(CallAnswer::Bill)
                }
                PreparedCall::PayBill { bill_id, body } => {
                    path_id("bill id", bill_id)?;
                    self.post(
                        "TransactionDTO",
                        &format!("/bills/{bill_id}/payments"),
                        Some(body),
                    )
                    .map(CallAnswer::Transaction)
                }
            },
        )
    }
}
