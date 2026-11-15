//! Texting the owner: the Rust service never calls Photon. It asks the Python service, which keeps
//! Photon, through `POST {PYTHON_SERVICE_URL}/internal/v1/owner/notify` (agreed on the team board
//! with Photon's owner; route reviewed in `orchestrator/reports/python-link-review.md`).
//!
//! What a notify is, here:
//! - The call flow collects [`NotifyRequest`]s while it works inside the request's transaction
//!   (where Python calls `notify_owner` inline) and hands them to [`OwnerNotify::dispatch`] only
//!   **after that transaction has committed**. Nothing here runs while a transaction, a row lock
//!   or an advisory lock is held.
//! - Delivery is best effort and never blocks or fails the request that caused it:
//!   [`OwnerNotify::dispatch`] sends the requests of one request in order, on a background task,
//!   and the HTTP answer does not wait for them ([`OwnerNotify::idle`] lets tests wait).
//! - When Python answers 200, Python's `notify_owner` has written the communication row and the
//!   `communication.owner_notified` event itself (committed before it answered), so Rust writes
//!   neither. Its `delivery` (`accepted` = Photon took the message, not a delivery confirmation;
//!   `recorded` = fixture; `not_linked`; `failed`) is logged, never upgraded.
//! - When Python was not reached or refused ([`NotifyOutcome::NotSent`]: not configured, a body
//!   the route would refuse, connection refused, any 4xx), or the outcome is unknown
//!   ([`NotifyOutcome::Unknown`]: timeout, transport error after sending, 5xx, an unreadable
//!   200), Rust records what Python's `notify_owner` records for a text that did not go out: a
//!   `FAILED` communication (`external_participants = [{role: owner, handle}]`) and a
//!   `communication.owner_notified` event whose `delivery` is `failed` (not sent) or `unknown`
//!   (it may have been sent), under its own key `notify-unreached:<key>` so that it never
//!   collides with Python's `notify:<key>` dedupe record. It also logs a warning. A text is
//!   never reported as sent unless Python said so.
//! - The idempotency key is passed through unchanged, so a repeat never texts twice once Python
//!   has committed it. There are no automatic retries: after a 500 or a timeout a same-key retry
//!   could text twice (python-link-2 report, section 4), and `failed` is final for a key.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use ss_domain::contracts::{Channel, CommunicationSession, CommunicationState};
use uuid::Uuid;

use crate::app::{App, AppState};
use crate::contracts::{ActorType, EventType};
use crate::error::AppResult;
use crate::logging;
use crate::services::events::Emit;
use crate::store::comms::{channel_for_owner, COMMUNICATIONS};

/// The route's bounds (`OwnerNotifyBody`).
pub const MAX_TEXT_CHARS: usize = 1500;
pub const MAX_PURPOSE_CHARS: usize = 100;
pub const MAX_KEY_CHARS: usize = 200;
/// `security.SERVICE_PURPOSE`.
pub const SERVICE_PURPOSE: &str = "service";
/// Lifetime of the service token minted for one notify: it is verified when Python starts the
/// request, so it only has to outlive the connection set-up.
pub const SERVICE_TOKEN_TTL_S: i64 = 120;
/// Whole-request timeout. Python's route can take about 62 s with live Photon (3 attempts x 20 s
/// plus backoff, review R13); waiting that long would keep the delivery task and a connection
/// busy while the call flow moves on. After this many seconds the outcome is recorded as
/// `unknown` (the text may still go out; Python then records it as well).
pub const NOTIFY_TIMEOUT_S: u64 = 15;
pub const NOTIFY_CONNECT_TIMEOUT_S: u64 = 3;
/// Event key prefix of the records Rust writes when Python was not reached.
pub const UNREACHED_KEY_PREFIX: &str = "notify-unreached:";

/// One text for the owner (the route's body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyRequest {
    pub owner_id: Uuid,
    pub project_id: Uuid,
    pub text: String,
    pub purpose: String,
    pub idempotency_key: String,
}

impl NotifyRequest {
    /// The route's JSON body.
    pub fn body(&self) -> Value {
        json!({
            "owner_id": self.owner_id.to_string(),
            "project_id": self.project_id.to_string(),
            "text": self.text,
            "purpose": self.purpose,
            "idempotency_key": self.idempotency_key,
        })
    }
}

/// What came of one notify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// Python answered 200: `notify_owner` ran and committed its own record.
    Answered {
        communication_id: Uuid,
        delivery: String,
    },
    /// Python did not run `notify_owner` (or was never asked): nothing was sent.
    NotSent { reason: String },
    /// It may or may not have been sent (timeout, 5xx, transport failure after sending).
    Unknown { reason: String },
}

#[async_trait]
pub trait OwnerNotifier: Send + Sync {
    async fn notify(&self, request: &NotifyRequest) -> NotifyOutcome;
    /// Short description for logs (`http`, `not_configured`, `recording`).
    fn kind(&self) -> &'static str;
}

fn is_py_space(c: char) -> bool {
    ss_domain::py::is_space(c)
}

fn has_control(s: &str) -> bool {
    s.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}')
}

/// Bring a text within the route's bounds: blank texts are refused; longer texts are cut to
/// 1,499 characters plus `…` (the route counts characters as Python does). Returns the text and
/// whether it was truncated.
pub fn fit_text(text: &str) -> Result<(String, bool), String> {
    if !text.chars().any(|c| !is_py_space(c)) {
        return Err("text is blank".into());
    }
    if text.chars().count() <= MAX_TEXT_CHARS {
        return Ok((text.to_owned(), false));
    }
    let mut cut: String = text.chars().take(MAX_TEXT_CHARS - 1).collect();
    cut.push('…');
    Ok((cut, true))
}

/// The route's checks on `purpose` and `idempotency_key` (`STORED_LABEL`, lengths), plus ASCII
/// keys (review R18: a non-ASCII key fails Photon's header).
pub fn check_labels(purpose: &str, key: &str) -> Result<(), String> {
    let label_ok = |s: &str, max: usize| {
        let n = s.chars().count();
        n >= 1 && n <= max && !has_control(s) && s.chars().any(|c| !is_py_space(c))
    };
    if !label_ok(purpose, MAX_PURPOSE_CHARS) {
        return Err(
            "purpose must be 1 to 100 characters, not blank, without control characters".into(),
        );
    }
    if !label_ok(key, MAX_KEY_CHARS) || !key.is_ascii() {
        return Err(
            "idempotency_key must be 1 to 200 ASCII characters, not blank, without control characters"
                .into(),
        );
    }
    Ok(())
}

/// No `PYTHON_SERVICE_URL`: nothing can be sent.
#[derive(Debug, Default)]
pub struct NotConfiguredNotifier;

#[async_trait]
impl OwnerNotifier for NotConfiguredNotifier {
    async fn notify(&self, _request: &NotifyRequest) -> NotifyOutcome {
        NotifyOutcome::NotSent {
            reason: "PYTHON_SERVICE_URL is not set; the owner was not texted".into(),
        }
    }
    fn kind(&self) -> &'static str {
        "not_configured"
    }
}

/// The HTTP client of the Python route.
#[derive(Clone)]
pub struct HttpOwnerNotifier {
    inner: Arc<HttpInner>,
}

struct HttpInner {
    url: String,
    auth_secret: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for HttpOwnerNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the secret.
        f.debug_struct("HttpOwnerNotifier")
            .field("url", &self.inner.url)
            .finish_non_exhaustive()
    }
}

impl HttpOwnerNotifier {
    /// `base_url` is `PYTHON_SERVICE_URL` (scheme, host, port; a trailing `/` is ignored).
    pub fn new(base_url: &str, auth_secret: &str) -> Self {
        Self::with_timeouts(
            base_url,
            auth_secret,
            Duration::from_secs(NOTIFY_CONNECT_TIMEOUT_S),
            Duration::from_secs(NOTIFY_TIMEOUT_S),
        )
    }

    pub fn with_timeouts(
        base_url: &str,
        auth_secret: &str,
        connect: Duration,
        total: Duration,
    ) -> Self {
        // No proxy: the Python service is reached directly (root decision on review R8); ureq
        // reads no proxy variables unless asked to.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(connect)
            .timeout(total)
            .redirects(0)
            .user_agent(concat!("second-site-backend/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            inner: Arc::new(HttpInner {
                url: format!(
                    "{}/internal/v1/owner/notify",
                    base_url.trim_end_matches('/')
                ),
                auth_secret: auth_secret.to_owned(),
                agent,
            }),
        }
    }

    pub fn url(&self) -> &str {
        &self.inner.url
    }
}

fn json_of(resp: ureq::Response) -> Option<Value> {
    resp.into_string()
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

impl HttpInner {
    fn send_blocking(&self, request: &NotifyRequest) -> NotifyOutcome {
        let token = crate::security::issue_token_with(
            &self.auth_secret,
            request.owner_id,
            SERVICE_TOKEN_TTL_S,
            SERVICE_PURPOSE,
        );
        let body = request.body().to_string();
        let result = self
            .agent
            .post(&self.url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json")
            .send_string(&body);
        match result {
            Ok(resp) => parse_answer(resp),
            Err(ureq::Error::Status(code, resp)) => {
                let error_code = json_of(resp)
                    .and_then(|v| {
                        v.get("error_code")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                let reason = format!("Python service answered HTTP {code} {error_code}")
                    .trim_end()
                    .to_owned();
                if code >= 500 {
                    // The route may have texted before its commit failed (python-link-2, sec. 4).
                    NotifyOutcome::Unknown { reason }
                } else {
                    NotifyOutcome::NotSent { reason }
                }
            }
            Err(ureq::Error::Transport(t)) => match t.kind() {
                ureq::ErrorKind::Dns
                | ureq::ErrorKind::ConnectionFailed
                | ureq::ErrorKind::InvalidUrl
                | ureq::ErrorKind::UnknownScheme
                | ureq::ErrorKind::InvalidProxyUrl
                | ureq::ErrorKind::ProxyConnect => NotifyOutcome::NotSent {
                    reason: format!("Python service not reached: {}", t.kind()),
                },
                kind => NotifyOutcome::Unknown {
                    reason: format!("no answer from the Python service ({kind}); outcome unknown"),
                },
            },
        }
    }
}

fn parse_answer(resp: ureq::Response) -> NotifyOutcome {
    let status = resp.status();
    if !(200..300).contains(&status) {
        return NotifyOutcome::NotSent {
            reason: format!("Python service answered HTTP {status}"),
        };
    }
    let parsed: Option<(Uuid, String)> = json_of(resp).and_then(|v| {
        let id = v.get("communication_id")?.as_str()?.parse().ok()?;
        let delivery = v.get("delivery")?.as_str()?.to_owned();
        Some((id, delivery))
    });
    match parsed {
        Some((communication_id, delivery))
            if matches!(
                delivery.as_str(),
                "accepted" | "recorded" | "not_linked" | "failed"
            ) =>
        {
            NotifyOutcome::Answered {
                communication_id,
                delivery,
            }
        }
        _ => NotifyOutcome::Unknown {
            reason: format!("Python service answered HTTP {status} with an unreadable body"),
        },
    }
}

#[async_trait]
impl OwnerNotifier for HttpOwnerNotifier {
    async fn notify(&self, request: &NotifyRequest) -> NotifyOutcome {
        let me = self.inner.clone();
        let req = request.clone();
        match tokio::task::spawn_blocking(move || me.send_blocking(&req)).await {
            Ok(outcome) => outcome,
            Err(e) => NotifyOutcome::Unknown {
                reason: format!("notify task failed: {e}"),
            },
        }
    }
    fn kind(&self) -> &'static str {
        "http"
    }
}

/// A test double: records every request and answers what it was told to (by default 200
/// `recorded`, as Python's fixture Photon would).
#[derive(Debug)]
pub struct RecordingNotifier {
    requests: Mutex<Vec<NotifyRequest>>,
    answer: Mutex<Option<NotifyOutcome>>,
}

impl Default for RecordingNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingNotifier {
    pub fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            answer: Mutex::new(None),
        }
    }

    /// Answer every later request with `outcome` (`None`: 200 `recorded` with a fresh id).
    pub fn answer_with(&self, outcome: Option<NotifyOutcome>) {
        *self.answer.lock().unwrap_or_else(|e| e.into_inner()) = outcome;
    }

    pub fn requests(&self) -> Vec<NotifyRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl OwnerNotifier for RecordingNotifier {
    async fn notify(&self, request: &NotifyRequest) -> NotifyOutcome {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.clone());
        let answer = self
            .answer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        answer.unwrap_or_else(|| NotifyOutcome::Answered {
            communication_id: Uuid::new_v4(),
            delivery: "recorded".into(),
        })
    }
    fn kind(&self) -> &'static str {
        "recording"
    }
}

/// The notifier on the application state, with the in-flight delivery counter.
pub struct OwnerNotify {
    notifier: Arc<dyn OwnerNotifier>,
    inflight: Arc<AtomicUsize>,
    idle: Arc<tokio::sync::Notify>,
}

impl OwnerNotify {
    pub fn new(notifier: Arc<dyn OwnerNotifier>) -> Self {
        Self {
            notifier,
            inflight: Arc::new(AtomicUsize::new(0)),
            idle: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// `PYTHON_SERVICE_URL` set: the HTTP client; otherwise nothing can be sent.
    pub fn from_settings(s: &crate::config::Settings) -> Self {
        match s
            .python_service_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            Some(url) => Self::new(Arc::new(HttpOwnerNotifier::new(
                url,
                s.auth_secret.expose(),
            ))),
            None => Self::new(Arc::new(NotConfiguredNotifier)),
        }
    }

    pub fn notifier(&self) -> Arc<dyn OwnerNotifier> {
        self.notifier.clone()
    }

    /// Send `requests` in order on a background task. Call only after the transaction that
    /// produced them has committed. Never fails and never waits.
    pub fn dispatch(&self, app: AppState, requests: Vec<NotifyRequest>) {
        if requests.is_empty() {
            return;
        }
        self.inflight.fetch_add(1, Ordering::SeqCst);
        let notifier = self.notifier.clone();
        let inflight = self.inflight.clone();
        let idle = self.idle.clone();
        tokio::spawn(async move {
            for req in requests {
                deliver_one(&app, notifier.as_ref(), req).await;
            }
            if inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
                idle.notify_waiters();
            }
        });
    }

    /// Wait until every dispatched delivery has finished (tests).
    pub async fn idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.inflight.load(Ordering::SeqCst) == 0 {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), notified).await;
        }
    }
}

/// Validate, send, log, and record honestly what could not be confirmed.
async fn deliver_one(app: &App, notifier: &dyn OwnerNotifier, mut req: NotifyRequest) {
    let outcome = match (
        fit_text(&req.text),
        check_labels(&req.purpose, &req.idempotency_key),
    ) {
        (Err(e), _) | (_, Err(e)) => NotifyOutcome::NotSent {
            reason: format!("refused before calling the Python service: {e}"),
        },
        (Ok((text, truncated)), Ok(())) => {
            if truncated {
                logging::warning(
                    "app.services.notifier",
                    "owner text longer than 1500 characters was truncated before sending",
                    &[("purpose", Value::String(req.purpose.clone()))],
                );
            }
            req.text = text;
            notifier.notify(&req).await
        }
    };
    let fields = |extra: Vec<(&'static str, Value)>| {
        let mut f = vec![
            ("purpose", Value::String(req.purpose.clone())),
            (
                "idempotency_key",
                Value::String(req.idempotency_key.clone()),
            ),
            ("project_id", Value::String(req.project_id.to_string())),
            ("notifier", Value::String(notifier.kind().to_owned())),
        ];
        f.extend(extra);
        f
    };
    match &outcome {
        NotifyOutcome::Answered {
            communication_id,
            delivery,
        } => {
            let f = fields(vec![
                ("delivery", Value::String(delivery.clone())),
                (
                    "communication_id",
                    Value::String(communication_id.to_string()),
                ),
            ]);
            if matches!(delivery.as_str(), "accepted" | "recorded") {
                // 'accepted' means Photon took the message, not that it was delivered.
                logging::info("app.services.notifier", "owner notify answered", &f);
            } else {
                logging::warning(
                    "app.services.notifier",
                    "owner notify answered: not sent",
                    &f,
                );
            }
        }
        NotifyOutcome::NotSent { reason } => logging::warning(
            "app.services.notifier",
            "owner was not texted",
            &fields(vec![("reason", Value::String(reason.clone()))]),
        ),
        NotifyOutcome::Unknown { reason } => logging::warning(
            "app.services.notifier",
            "owner notify outcome unknown",
            &fields(vec![("reason", Value::String(reason.clone()))]),
        ),
    }
    let delivery = match &outcome {
        NotifyOutcome::Answered { .. } => return,
        NotifyOutcome::NotSent { .. } => "failed",
        NotifyOutcome::Unknown { .. } => "unknown",
    };
    if let Err(e) = record_unreached(app, &req, delivery).await {
        logging::error(
            "app.services.notifier",
            "could not record the owner notify outcome",
            &fields(vec![("error", Value::String(e.to_string()))]),
        );
    }
}

/// What Python's `notify_owner` stores for a text that did not go out, written by Rust because
/// Python was not reached: a FAILED communication and a `communication.owner_notified` event with
/// `delivery` `failed` or `unknown`, under `notify-unreached:<key>`.
async fn record_unreached(app: &App, req: &NotifyRequest, delivery: &str) -> AppResult<()> {
    let mut tx = app.store.begin().await?;
    let key = format!("{UNREACHED_KEY_PREFIX}{}", req.idempotency_key);
    if app.events().exists(&mut *tx, &key).await? {
        return Ok(()); // already recorded (a repeated request)
    }
    let ch = channel_for_owner(&mut *tx, req.owner_id).await?;
    let channel = match &ch {
        Some(c) if c.platform != "imessage" => Channel::PhotonSms,
        _ => Channel::PhotonImessage,
    };
    let mut participant = indexmap::IndexMap::new();
    participant.insert("role".to_owned(), "owner".to_owned());
    participant.insert(
        "handle".to_owned(),
        ch.as_ref().map(|c| c.handle.clone()).unwrap_or_default(),
    );
    let comm = CommunicationSession {
        communication_id: Uuid::new_v4(),
        project_id: req.project_id,
        property_id: None,
        channel,
        external_participants: vec![participant],
        authorized_owner_id: req.owner_id,
        purpose: req.purpose.clone(),
        state: CommunicationState::Failed,
        recording_reference: None,
        transcript_reference: None,
        structured_findings: Vec::new(),
        approval_id: None,
        provider_message_ids: Vec::new(),
        created_at: ss_core::UtcDateTime::now(),
    };
    COMMUNICATIONS.add(&mut *tx, &comm).await?;
    app.events()
        .emit(
            &mut *tx,
            Emit::new(
                EventType::CommunicationOwnerNotified,
                req.project_id,
                ActorType::Agent,
                "owner-messaging",
                key,
                json!({
                    "communication_id": comm.communication_id.to_string(),
                    "delivery": delivery,
                    "purpose": req.purpose,
                }),
            ),
        )
        .await?;
    tx.commit().await?;
    Ok(())
}

impl App {
    /// The owner notifier (`PYTHON_SERVICE_URL`). Tests replace it with
    /// `app.extensions.insert(Arc::new(OwnerNotify::new(...)))`.
    pub fn owner_notify(&self) -> Arc<OwnerNotify> {
        match self.extensions.get::<OwnerNotify>() {
            Some(n) => n,
            None => {
                let n = Arc::new(OwnerNotify::from_settings(&self.settings));
                self.extensions.insert(n.clone());
                n
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_fitted_to_the_route() {
        assert!(fit_text("   \t\n").is_err());
        assert!(fit_text("").is_err());
        assert_eq!(fit_text("hi").unwrap(), ("hi".into(), false));
        let long: String = "é".repeat(1600);
        let (t, cut) = fit_text(&long).unwrap();
        assert!(cut);
        assert_eq!(t.chars().count(), 1500);
        assert!(t.ends_with('…'));
        let exact: String = "a".repeat(1500);
        assert_eq!(fit_text(&exact).unwrap(), (exact.clone(), false));
    }

    #[test]
    fn labels_follow_the_route() {
        assert!(check_labels("approval_request", "approval-notify:x").is_ok());
        assert!(check_labels("", "k").is_err());
        assert!(check_labels("a\u{0}b", "k").is_err());
        assert!(check_labels("a\nb", "k").is_err());
        assert!(check_labels("p", "clé").is_err());
        assert!(check_labels("p", &"k".repeat(201)).is_err());
        assert!(check_labels(&"p".repeat(101), "k").is_err());
        assert!(check_labels("p", " ").is_err());
    }
}
