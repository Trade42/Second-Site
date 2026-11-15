//! Port of `apps/api/app/services/call_orchestration/__init__.py`: approval-gated outbound broker
//! calls, the carrier-event state machine and the owner handoff.
//!
//! ```text
//! start()            APPROVED PLACE_CALL -> dial (client_state carries call_id + role)       -> DIALING
//! call.answered      play the owner's recorded intro, else speak the intro + disclosure      -> INTRO
//! machine detected   short voicemail message, hang up
//! speak/playback end start transcription, ask the first grounded question                   -> ACTIVE
//! call.transcription escalation phrase -> OwnerHandoffRequest + text to the owner            -> HANDOFF_PENDING
//!                    otherwise -> StructuredFinding -> SourceEvidence(OWNER_REPORTED) -> evidence_updated
//! owner JOIN         dial the owner, conference the broker leg                               -> OWNER_JOINING
//! owner leg joined                                                                            -> HUMAN_JOINED
//! owner LATER/none   the assistant defers the sensitive question and continues               -> AI_RESUMED
//! call.hangup        finalize: findings on the CommunicationSession, outreach.completed, owner summary
//! ```
//!
//! Every state transition, event (type, actor `agent`/`voice-handoff`, idempotency key, payload),
//! transcript turn, finding and evidence record is Python's. Rules held in code:
//! - **Approval at the action boundary.** [`CallOrchestrator::start`] refuses unless the approval
//!   is APPROVED, unexpired, for `PLACE_CALL`, for exactly this call's payload
//!   (`call_payload(project_id, recipient, property_id)`), and belongs to this project
//!   (`ApprovalService::require_approved`, which locks the approval row). It is spent with
//!   `mark_executed` (a version-checked update) in the same transaction, right after the carrier
//!   accepted the dial. A dial the carrier refuses leaves the approval APPROVED, as in Python.
//! - **Accepted is not answered.** A dial answer only moves the call to DIALING; INTRO, ACTIVE,
//!   COMPLETED, FAILED and DECLINED are reached only on the carrier's own events (or the
//!   simulator's events of the same shape, labelled `simulated`).
//! - **What the broker says is reported evidence**: `OWNER_REPORTED`, with the limitation
//!   "not independently verified" (and "SIMULATED CALL" for simulations, `FIXTURE_SYNTHETIC`),
//!   never verified capacity.
//! - **Escalation**: `detect_escalation` runs on every counterparty utterance while the assistant
//!   leads the call; during a handoff the assistant stays quiet; it never agrees to anything, and
//!   an expired or declined handoff tells the counterparty the owner will follow up.
//! - **Owner texts** are collected in [`CallOrchestrator::notices`] and sent by the caller after
//!   the transaction commits ([`crate::services::notifier`]); Python calls `notify_owner` inline.
//!
//! Deliberate differences from Python, each fixing a defect:
//! - Every record loaded from the request goes through the project (the routes do that); the
//!   communication a call updates is loaded through the call's project.
//! - `owner_response` refuses (409 `INVALID_STATE_TRANSITION`) when the call is no longer
//!   `HANDOFF_PENDING`, before any carrier command. Python reaches the same 409 later, after it
//!   may already have dialed the owner's phone, and then rolls back the records of that dial.
//! - `find_call`: when an event's `call_control_id` is not a leg of the call its `client_state`
//!   names, the event is matched by its leg instead (Python trusts `client_state` alone); a
//!   `client_state` or `call_id` of the wrong JSON type is "unknown call" (Python: 500).
//! - A recipient without a `phone` is refused with 409 before anything is dialed (Python:
//!   `KeyError`, 500).
//! - Carrier commands run on the blocking pool (`integrations::blocking`).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use chrono::{Duration, Utc};
use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_core::{DataOrigin, EvidenceClassification, UtcDateTime};
use ss_domain::communications::{build_script, call_payload, describe, detect_escalation, extract};
use ss_domain::contracts::{
    CallSession, CallState, CommunicationState, HandoffReason, HandoffStatus, OwnerHandoffRequest,
    ParticipantState, SourceEvidence, StructuredFinding, TranscriptTurn,
};
use ss_domain::integrations::telnyx::{
    client_state, decode_client_state_json, TelephonyError, TelephonyProvider,
};
use uuid::Uuid;

use crate::app::App;
use crate::contracts::{ActionType, ActorType, EventType};
use crate::error::{AppError, AppResult};
use crate::integrations::blocking;
use crate::logging;
use crate::services::events::Emit;
use crate::services::evidence::EvidenceService;
use crate::services::notifier::NotifyRequest;
use crate::store::comms::{
    call_by_provider_id, channel_for_owner, CALLS, COMMUNICATIONS, HANDOFFS,
};
use crate::store::repo::{users, ASSESSMENTS, PROJECTS};
use crate::store::{Query, Tx};

/// `HANDOFF_TTL` (3 minutes).
pub const HANDOFF_TTL_S: i64 = 180;
/// `TERMINAL`.
pub const TERMINAL: [CallState; 4] = [
    CallState::Completed,
    CallState::Failed,
    CallState::Declined,
    CallState::Cancelled,
];
/// Python's dial fallback when `TELNYX_FROM_NUMBER` is unset.
pub const DEFAULT_FROM: &str = "+10000000000";
pub const ACTOR: &str = "voice-handoff";
/// The label every owner text about a simulated call starts with (Python's summary prefix).
pub const SIMULATED_LABEL: &str = "[Simulated call] ";

/// `_JOIN_TOKENS`: reference -> join token, process-local and never stored or put in an event
/// (Python keeps the same dict; nothing reads the token back yet). Bounded here.
static JOIN_TOKENS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const MAX_JOIN_TOKENS: usize = 4096;

fn join_tokens() -> std::sync::MutexGuard<'static, HashMap<String, String>> {
    JOIN_TOKENS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lock the project row (`SELECT ... FOR UPDATE`) before the transaction's first write. Every
/// call-flow transaction takes it first, in the order the search and the business-profile update
/// take it (project row, then the project's event lock): without it, a long call-flow
/// transaction that already holds the event lock and then inserts a row referencing the project
/// (the foreign-key check takes `FOR KEY SHARE`) deadlocks against a search that holds the row and
/// waits for the event lock. It also serializes concurrent carrier events of one call, so neither
/// overwrites the other's update of the call document (Python has that lost-update race).
/// Re-entrant within a transaction.
pub async fn lock_project(tx: &mut dyn Tx, project_id: Uuid) -> AppResult<()> {
    PROJECTS.get_for_update(tx, project_id).await?;
    Ok(())
}

pub fn is_terminal(s: CallState) -> bool {
    TERMINAL.contains(&s)
}

/// Whether a failed dial leaves open that the carrier placed the call: a timeout, a 5xx, a
/// transport failure after the request went out, or a 2xx the client could not read. A refusal
/// the carrier stated (4xx, 401/403, 404, 429), a connection that was never made, and the fixture
/// provider's simulated failure are not; for those the approval stays APPROVED, as in Python.
pub fn dial_may_have_been_placed(e: &ss_domain::integrations::base::ProviderError) -> bool {
    use ss_domain::integrations::base::ProviderErrorKind as K;
    const NOT_SENT: [&str; 10] = [
        "Connection Failed",
        "Dns Failed",
        "Bad URL",
        "Unknown Scheme",
        "Malformed proxy",
        "Proxy Connect",
        "Provided proxy",
        "Insecure request",
        "simulated dial failure",
        "retry policy allows no attempts",
    ];
    match e.kind {
        K::Timeout => true,
        K::BadResponse => !e.message.starts_with("HTTP "),
        K::Unavailable => !NOT_SENT.iter().any(|p| e.message.starts_with(p)),
        _ => false,
    }
}

fn now() -> UtcDateTime {
    UtcDateTime::now()
}

fn turn(speaker: &str, text: impl Into<String>) -> TranscriptTurn {
    TranscriptTurn {
        speaker: speaker.to_owned(),
        text: text.into(),
        at: now(),
    }
}

fn participant(role: &str, leg: Option<String>, state: &str) -> ParticipantState {
    ParticipantState {
        role: role.to_owned(),
        leg_id: leg,
        state: state.to_owned(),
        updated_at: now(),
    }
}

fn get<'m>(m: &'m IndexMap<String, String>, k: &str) -> Option<&'m str> {
    m.get(k).map(String::as_str)
}

/// `d.get(k) or default` for a string map.
fn get_or<'m>(m: &'m IndexMap<String, String>, k: &str, default: &'m str) -> &'m str {
    match m.get(k) {
        Some(v) if !v.is_empty() => v,
        Some(_) => "",
        None => default,
    }
}

/// `d.get(k, default)` (an empty value is kept, as Python's `dict.get`).
fn get_default<'m>(m: &'m IndexMap<String, String>, k: &str, default: &'m str) -> &'m str {
    m.get(k).map_or(default, String::as_str)
}

/// The question at `call.question_index` when `question_index < len(script)` (Python indexing,
/// negative counts from the end).
fn current_question(call: &CallSession) -> Option<ss_domain::contracts::CallQuestion> {
    let len = call.script.len() as i64;
    if call.question_index >= len {
        return None;
    }
    let idx = if call.question_index < 0 {
        len + call.question_index
    } else {
        call.question_index
    };
    usize::try_from(idx)
        .ok()
        .and_then(|i| call.script.get(i))
        .cloned()
}

/// Python truthiness of an optional JSON value, and `str(v)` when truthy.
fn truthy_str(v: Option<&Value>) -> Option<String> {
    v.filter(|v| ss_domain::py::truthy(v))
        .map(ss_domain::py::value_str)
}

/// The arguments of [`CallOrchestrator::start`].
#[derive(Debug, Clone)]
pub struct StartCall {
    pub project_id: Uuid,
    pub owner_id: Uuid,
    pub recipient: IndexMap<String, String>,
    pub property_id: Option<Uuid>,
    pub approval_id: Option<Uuid>,
    pub intro_recording_reference: Option<String>,
    pub communication_id: Option<Uuid>,
    pub simulated: bool,
}

/// `CallOrchestrator`. Owner texts it asks for are collected in `notices`; the caller sends them
/// after its transaction commits.
pub struct CallOrchestrator<'a> {
    app: &'a App,
    telephony: Arc<dyn TelephonyProvider>,
    pub notices: Vec<NotifyRequest>,
}

impl<'a> CallOrchestrator<'a> {
    pub fn new(app: &'a App) -> Self {
        let telephony = app.voice().telephony.clone();
        Self {
            app,
            telephony,
            notices: Vec::new(),
        }
    }

    pub fn app(&self) -> &'a App {
        self.app
    }

    /// The owner texts collected so far (send them after the commit).
    pub fn take_notices(&mut self) -> Vec<NotifyRequest> {
        std::mem::take(&mut self.notices)
    }

    /// Run one carrier command on the blocking pool.
    async fn tel<T, F>(&self, f: F) -> AppResult<Result<T, TelephonyError>>
    where
        T: Send + 'static,
        F: FnOnce(&dyn TelephonyProvider) -> Result<T, TelephonyError> + Send + 'static,
    {
        let t = self.telephony.clone();
        blocking(move || f(t.as_ref())).await
    }

    /// A command whose provider failure Python does not catch: `ProviderError` becomes the
    /// request's error (503 here), `ForbiddenError` a 403.
    async fn tel_strict<T, F>(&self, f: F) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&dyn TelephonyProvider) -> Result<T, TelephonyError> + Send + 'static,
    {
        match self.tel(f).await? {
            Ok(v) => Ok(v),
            Err(TelephonyError::Provider(e)) => Err(AppError::provider(&e)),
            Err(TelephonyError::Forbidden(m)) => Err(AppError::forbidden(m)),
        }
    }

    fn webhook_url(&self) -> String {
        format!("{}/webhooks/telnyx", self.app.settings.public_base_url)
    }

    fn caller_number(&self) -> String {
        self.app
            .settings
            .telnyx_from_number
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| DEFAULT_FROM.to_owned())
    }

    // ---- persistence / events ---------------------------------------------------------------

    async fn save(&self, tx: &mut dyn Tx, call: CallSession) -> AppResult<CallSession> {
        CALLS.upsert(tx, &call).await?;
        Ok(call)
    }

    async fn emit(
        &self,
        tx: &mut dyn Tx,
        call: &CallSession,
        et: EventType,
        detail: Option<&str>,
        key: Option<String>,
    ) -> AppResult<()> {
        let key = key.unwrap_or_else(|| {
            format!(
                "call:{}:{}:{}",
                call.call_id,
                call.state.as_str(),
                call.transcript.len()
            )
        });
        self.app
            .events()
            .emit(
                tx,
                Emit::new(
                    et,
                    call.project_id,
                    ActorType::Agent,
                    ACTOR,
                    key,
                    json!({
                        "call_id": call.call_id.to_string(),
                        "state": call.state.as_str(),
                        "detail": detail,
                    }),
                ),
            )
            .await?;
        Ok(())
    }

    /// `_say`: speak on the broker leg and keep the transcript.
    async fn say(&self, tx: &mut dyn Tx, call: CallSession, text: &str) -> AppResult<CallSession> {
        let mut text = text.to_owned();
        if let Some(leg) = call.provider_call_id.clone() {
            let spoken = text.clone();
            match self.tel(move |t| t.speak(&leg, &spoken)).await? {
                Ok(()) => {}
                Err(TelephonyError::Provider(e)) => {
                    text = format!("{text} [speak failed: {}]", e.kind);
                }
                Err(TelephonyError::Forbidden(m)) => return Err(AppError::forbidden(m)),
            }
        }
        let mut call = call;
        call.transcript.push(turn("assistant", text));
        self.save(tx, call).await
    }

    /// `_notify_owner`: queued, sent after the commit.
    fn notify_owner(&mut self, call: &CallSession, text: String, purpose: &str, key: String) {
        self.notices.push(NotifyRequest {
            owner_id: call.owner_id,
            project_id: call.project_id,
            text,
            purpose: purpose.to_owned(),
            idempotency_key: key,
        });
    }

    async fn owner_name(&self, tx: &mut dyn Tx, owner_id: Uuid) -> AppResult<String> {
        Ok(users::get(tx, owner_id)
            .await?
            .map(|u| u.display_name)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "the business owner".to_owned()))
    }

    // ---- start --------------------------------------------------------------------------------

    /// `start`: the approval gate, the call record and the dial.
    pub async fn start(&mut self, tx: &mut dyn Tx, s: StartCall) -> AppResult<CallSession> {
        lock_project(tx, s.project_id).await?;
        let payload = call_payload(s.project_id, &s.recipient, s.property_id);
        let approved = self
            .app
            .approvals()
            .require_approved(
                tx,
                s.project_id,
                s.approval_id,
                ActionType::PlaceCall,
                &payload,
            )
            .await?;
        let assessments = match s.property_id {
            Some(pid) => {
                ASSESSMENTS
                    .list(tx, Query::new().eq("property_id", pid))
                    .await?
            }
            None => Vec::new(),
        };
        let mut call = CallSession::new(
            s.project_id,
            s.owner_id,
            s.recipient.clone(),
            approved.approval.approval_id,
        );
        call.property_id = s.property_id;
        call.intro_recording_reference = s.intro_recording_reference;
        call.communication_id = s.communication_id;
        call.script = build_script(&assessments);
        call.simulated = s.simulated;
        let Some(phone) = get(&s.recipient, "phone").map(str::to_owned) else {
            return Err(AppError::conflict("recipient has no phone number"));
        };
        CALLS.add(tx, &call).await?;
        let mut call = call.transition(CallState::Dialing)?;
        let (from, url, key) = (
            self.caller_number(),
            self.webhook_url(),
            format!("call:{}", call.call_id),
        );
        let cs = client_state(&call.call_id.to_string(), "counterparty");
        let dialed = self
            .tel(move |t| t.dial(&phone, &from, &url, &key, Some(&cs)))
            .await?;
        let res = match dialed {
            Ok(r) => r,
            Err(TelephonyError::Provider(err)) => {
                // REVIEW FIX (rust-server-4-review F1): when the carrier may have placed the call
                // (no answer, a 5xx, an unreadable 2xx), the approval is spent and the record says
                // the outcome is unknown, so a repeated start cannot dial the broker a second time.
                let unknown = dial_may_have_been_placed(&err);
                let reason = if unknown {
                    format!(
                        "dial outcome unknown, the call may have been placed: {}",
                        err.message
                    )
                } else {
                    err.message.clone()
                };
                let mut failed = call.transition(CallState::Failed)?;
                failed.failure_reason = Some(reason.clone());
                failed.ended_at = Some(now());
                let failed = self.save(tx, failed).await?;
                if unknown {
                    self.app.approvals().mark_executed(tx, approved).await?;
                }
                self.emit(tx, &failed, EventType::CallFailed, Some(&reason), None)
                    .await?;
                return Ok(failed);
            }
            Err(TelephonyError::Forbidden(m)) => return Err(AppError::forbidden(m)),
        };
        call.provider_call_id = Some(res.call_control_id.clone());
        call.started_at = Some(now());
        call.participant_states = vec![participant(
            "counterparty",
            Some(res.call_control_id),
            "dialing",
        )];
        let call = self.save(tx, call).await?;
        self.app.approvals().mark_executed(tx, approved).await?;
        self.set_comm_state(tx, &call, CommunicationState::InProgress)
            .await?;
        self.emit(
            tx,
            &call,
            EventType::CallStarted,
            Some("dial initiated (not yet answered)"),
            None,
        )
        .await?;
        Ok(call)
    }

    /// `advance`.
    pub async fn advance(
        &mut self,
        tx: &mut dyn Tx,
        call_id: Uuid,
        target: CallState,
        detail: Option<&str>,
    ) -> AppResult<CallSession> {
        let mut call = CALLS.require(tx, call_id).await?.transition(target)?;
        if is_terminal(target) {
            call.ended_at = Some(now());
            call.failure_reason = if target == CallState::Failed {
                detail.map(str::to_owned)
            } else {
                None
            };
        }
        let call = self.save(tx, call).await?;
        let et = match target {
            CallState::Completed => Some(EventType::CallCompleted),
            CallState::Failed => Some(EventType::CallFailed),
            CallState::HumanJoined => Some(EventType::CallOwnerJoined),
            _ => None,
        };
        if let Some(et) = et {
            self.emit(tx, &call, et, detail, None).await?;
        }
        Ok(call)
    }

    // ---- carrier events -----------------------------------------------------------------------

    /// `find_call`: the call and the leg's role (`counterparty` / `owner`), from `client_state`
    /// first, then from the leg id. See the module docs for the leg consistency check.
    pub async fn find_call(
        &self,
        tx: &mut dyn Tx,
        payload: &serde_json::Map<String, Value>,
    ) -> AppResult<(Option<CallSession>, String)> {
        let leg = payload
            .get("call_control_id")
            .filter(|v| ss_domain::py::truthy(v))
            .map(ss_domain::py::value_str);
        let state = match decode_client_state_json(payload.get("client_state")) {
            Ok(s) => s,
            Err(e) => {
                logging::warning(
                    "app.services.call_orchestration",
                    "carrier event with a client_state that is not a string; ignored",
                    &[("error", Value::String(e))],
                );
                IndexMap::new()
            }
        };
        if let Some(Value::String(cid)) = state.get("call_id").filter(|v| ss_domain::py::truthy(v))
        {
            if let Ok(id) = cid.parse::<Uuid>() {
                if let Some(call) = CALLS.get(tx, id).await? {
                    let role = match state.get("role") {
                        Some(Value::String(r)) => r.clone(),
                        None => "counterparty".to_owned(),
                        Some(other) => ss_domain::py::value_str(other),
                    };
                    let consistent = match &leg {
                        None => true,
                        Some(l) => {
                            call.provider_call_id.as_deref() == Some(l.as_str())
                                || call
                                    .participant_states
                                    .iter()
                                    .any(|p| p.leg_id.as_deref() == Some(l.as_str()))
                        }
                    };
                    if consistent {
                        return Ok((Some(call), role));
                    }
                    logging::warning(
                        "app.services.call_orchestration",
                        "carrier event leg is not a leg of the call named in client_state; \
                         matching by leg instead",
                        &[("call_id", Value::String(id.to_string()))],
                    );
                }
            }
        }
        if let Some(l) = leg {
            if let Some(call) = call_by_provider_id(tx, &l).await? {
                return Ok((Some(call), "counterparty".to_owned()));
            }
        }
        Ok((None, "unknown".to_owned()))
    }

    /// `handle_event`: one Telnyx-shaped event (`{data: {event_type, payload}}` or the inner
    /// object itself).
    pub async fn handle_event(
        &mut self,
        tx: &mut dyn Tx,
        event: &Value,
    ) -> AppResult<Option<CallSession>> {
        let data = match event.get("data") {
            Some(d) if ss_domain::py::truthy(d) => d,
            _ => event,
        };
        let Some(data) = data.as_object() else {
            return Ok(None);
        };
        let et: Option<String> = data
            .get("event_type")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let empty = serde_json::Map::new();
        let p = match data.get("payload") {
            Some(Value::Object(m)) => m,
            _ => &empty,
        };
        let (call, role) = self.find_call(tx, p).await?;
        let Some(call) = call else {
            return Ok(None);
        };
        // Lock, then read the call again: a concurrent event of the same call has committed.
        lock_project(tx, call.project_id).await?;
        let call = CALLS.require(tx, call.call_id).await?;
        if is_terminal(call.state) {
            return Ok(Some(call));
        }
        if role == "owner" {
            return self
                .owner_leg_event(tx, call, et.as_deref(), p)
                .await
                .map(Some);
        }
        let et = et.as_deref();
        if et == Some("call.answered") && call.state == CallState::Dialing {
            return self.on_answered(tx, call).await.map(Some);
        }
        if et == Some("call.machine.detection.ended")
            && p.get("result").and_then(Value::as_str) == Some("machine")
        {
            let owner = self.owner_name(tx, call.owner_id).await?;
            let call = self
                .say(
                    tx,
                    call,
                    &format!(
                        "Hi, this is an automated assistant calling for {owner} about your \
                         commercial listing. We'll follow up by email. Thank you."
                    ),
                )
                .await?;
            if let Some(leg) = call.provider_call_id.clone() {
                self.tel_strict(move |t| t.hangup(&leg)).await?;
            }
            return Ok(Some(call));
        }
        if matches!(et, Some("call.speak.ended" | "call.playback.ended"))
            && call.state == CallState::Intro
        {
            return self.begin_questions(tx, call).await.map(Some);
        }
        if et == Some("call.transcription") {
            let td = match p.get("transcription_data") {
                Some(Value::Object(m)) => m.clone(),
                _ => serde_json::Map::new(),
            };
            let text = match td.get("transcript") {
                Some(Value::String(s)) => ss_domain::py::strip(s).to_owned(),
                _ => String::new(),
            };
            let is_final = td.get("is_final").is_none_or(ss_domain::py::truthy);
            if is_final && !text.is_empty() {
                return self.on_utterance(tx, call, &text).await.map(Some);
            }
            return Ok(Some(call));
        }
        if et == Some("call.hangup") {
            let cause = truthy_str(p.get("hangup_cause"));
            return self.on_hangup(tx, call, cause).await.map(Some);
        }
        Ok(Some(call))
    }

    async fn on_answered(&mut self, tx: &mut dyn Tx, call: CallSession) -> AppResult<CallSession> {
        let mut call = call.transition(CallState::Intro)?;
        call.participant_states = vec![participant(
            "counterparty",
            call.provider_call_id.clone(),
            "joined",
        )];
        let mut call = self.save(tx, call).await?;
        let owner = self.owner_name(tx, call.owner_id).await?;
        if let (Some(intro), Some(leg)) = (
            call.intro_recording_reference
                .clone()
                .filter(|r| !r.is_empty()),
            call.provider_call_id.clone(),
        ) {
            let token = crate::security::sign_media_key(
                self.app.settings.auth_secret.expose(),
                &intro,
                900,
            );
            let url = format!(
                "{}/api/v1/media/public/{token}",
                self.app.settings.public_base_url
            );
            match self.tel(move |t| t.play(&leg, &url)).await? {
                Ok(()) => {
                    call.transcript.push(turn(
                        "system",
                        format!("[played {owner}'s recorded introduction]"),
                    ));
                    call = self.save(tx, call).await?;
                }
                Err(TelephonyError::Provider(_)) => {}
                Err(TelephonyError::Forbidden(m)) => return Err(AppError::forbidden(m)),
            }
            let disclosure = call.automation_disclosure.clone();
            return self.say(tx, call, &disclosure).await;
        }
        let text = format!(
            "Hi {}, I'm an automated assistant calling on behalf of {owner}, who is looking at your \
             listing for a second location. {} I just have a few quick questions about the space. \
             Is now an okay time?",
            get_default(&call.recipient, "name", "there"),
            call.automation_disclosure
        );
        self.say(tx, call, &text).await
    }

    async fn begin_questions(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
    ) -> AppResult<CallSession> {
        let call = call.transition(CallState::Active)?;
        let call = self.save(tx, call).await?;
        if let Some(leg) = call.provider_call_id.clone() {
            match self.tel(move |t| t.start_transcription(&leg)).await? {
                Ok(()) | Err(TelephonyError::Provider(_)) => {}
                Err(TelephonyError::Forbidden(m)) => return Err(AppError::forbidden(m)),
            }
        }
        self.ask_next(tx, call).await
    }

    async fn ask_next(&mut self, tx: &mut dyn Tx, call: CallSession) -> AppResult<CallSession> {
        if let Some(q) = current_question(&call) {
            return self.say(tx, call, &q.question).await;
        }
        let text = format!(
            "That's everything I needed. Thank you so much. {}Have a great day.",
            if call.deferred_questions.is_empty() {
                ""
            } else {
                "The owner will follow up directly on the other items. "
            }
        );
        let call = self.say(tx, call, &text).await?;
        if let Some(leg) = call.provider_call_id.clone() {
            self.tel_strict(move |t| t.hangup(&leg)).await?;
        }
        Ok(call)
    }

    /// `on_utterance`: one final transcription turn of the counterparty.
    pub async fn on_utterance(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        text: &str,
    ) -> AppResult<CallSession> {
        let mut call = call;
        call.transcript.push(turn("counterparty", text));
        let call = self.save(tx, call).await?;
        if matches!(
            call.state,
            CallState::HumanJoined | CallState::OwnerJoining | CallState::HandoffPending
        ) {
            return Ok(call); // the owner is (being) connected; the assistant stays quiet
        }
        if call.state == CallState::Intro {
            return self.begin_questions(tx, call).await;
        }
        if let Some(reason) = detect_escalation(text) {
            let call = self
                .say(
                    tx,
                    call,
                    "That's a decision for the owner, and I can't agree to anything on their \
                     behalf. Let me check if they can join right now.",
                )
                .await?;
            self.request_handoff(tx, call.call_id, reason, text).await?;
            return CALLS.require(tx, call.call_id).await;
        }
        let mut call = call;
        if let Some(q) = current_question(&call) {
            let finding = extract(&q, text);
            call.findings.push(finding.clone());
            call.question_index += 1;
            call = self.save(tx, call).await?;
            self.record_finding(tx, &call, q.requirement_id.as_deref(), &finding)
                .await?;
        }
        self.ask_next(tx, call).await
    }

    async fn record_finding(
        &mut self,
        tx: &mut dyn Tx,
        call: &CallSession,
        _requirement_id: Option<&str>,
        finding: &StructuredFinding,
    ) -> AppResult<()> {
        let mut evidence_id: Option<Uuid> = None;
        let claim = finding.claim_type.clone().unwrap_or_default();
        if let Some(property_id) = call.property_id {
            if !finding.value.is_null() && finding.claim_type.as_deref() != Some("available_from") {
                let who = get_or(&call.recipient, "name", "the listing contact");
                let who = if who.is_empty() {
                    "the listing contact"
                } else {
                    who
                };
                let mut limits = vec![format!(
                    "Stated by {who} ({}) on a call on {}; not independently verified.",
                    get_default(&call.recipient, "role", "broker"),
                    Utc::now().format("%Y-%m-%d")
                )];
                if call.simulated {
                    limits.insert(
                        0,
                        "SIMULATED CALL: scripted demo answer, not a real conversation.".into(),
                    );
                }
                let ev = SourceEvidence {
                    evidence_id: Uuid::new_v4(),
                    project_id: Some(call.project_id),
                    property_id: Some(property_id),
                    provider: "broker_call".into(),
                    dataset: None,
                    source_record_id: Some(call.call_id.to_string()),
                    source_url: None,
                    source_asset_reference: None,
                    data_origin: if call.simulated {
                        DataOrigin::FixtureSynthetic
                    } else {
                        DataOrigin::Live
                    },
                    retrieved_at: now(),
                    source_effective_at: None,
                    observation_type: claim.clone(),
                    extracted_value: finding.value.clone(),
                    unit: finding.unit.clone(),
                    evidence_classification: EvidenceClassification::OwnerReported,
                    match_confidence: None,
                    verification_status: ss_core::VerificationStatus::Unverified,
                    limitations: limits,
                };
                EvidenceService.add(tx, &ev).await?;
                evidence_id = Some(ev.evidence_id);
                self.app
                    .events()
                    .emit(
                        tx,
                        Emit::new(
                            EventType::InfrastructureEvidenceUpdated,
                            call.project_id,
                            ActorType::Agent,
                            ACTOR,
                            format!("call-evidence:{}", ev.evidence_id),
                            json!({
                                "property_id": property_id.to_string(),
                                "evidence_ids": [ev.evidence_id.to_string()],
                                "reason": format!("broker answer on call ({claim})"),
                            }),
                        ),
                    )
                    .await?;
            }
        }
        self.app
            .events()
            .emit(
                tx,
                Emit::new(
                    EventType::CallFindingRecorded,
                    call.project_id,
                    ActorType::Agent,
                    ACTOR,
                    format!("finding:{}:{}", call.call_id, call.question_index),
                    json!({
                        "call_id": call.call_id.to_string(),
                        "claim_type": finding.claim_type,
                        "evidence_id": evidence_id.map(|e| e.to_string()),
                        "summary": describe(finding),
                    }),
                ),
            )
            .await?;
        Ok(())
    }

    async fn on_hangup(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        cause: Option<String>,
    ) -> AppResult<CallSession> {
        if call.state == CallState::Dialing {
            let target = if cause.as_deref() == Some("call_rejected") {
                CallState::Declined
            } else {
                CallState::Failed
            };
            let detail = format!("not answered ({})", cause.as_deref().unwrap_or("unknown"));
            let call = self
                .advance(tx, call.call_id, target, Some(&detail))
                .await?;
            self.set_comm_state(tx, &call, CommunicationState::Failed)
                .await?;
            return Ok(call);
        }
        let call = if call.state == CallState::Intro {
            let detail = format!(
                "ended during intro ({})",
                cause.as_deref().unwrap_or("hangup")
            );
            self.advance(tx, call.call_id, CallState::Completed, Some(&detail))
                .await?
        } else if matches!(
            call.state,
            CallState::HandoffPending | CallState::OwnerJoining
        ) {
            self.advance(
                tx,
                call.call_id,
                CallState::Failed,
                Some("counterparty hung up during handoff"),
            )
            .await?
        } else {
            let detail = cause.unwrap_or_else(|| "normal_clearing".to_owned());
            self.advance(tx, call.call_id, CallState::Completed, Some(&detail))
                .await?
        };
        self.finalize(tx, call).await
    }

    async fn finalize(&mut self, tx: &mut dyn Tx, call: CallSession) -> AppResult<CallSession> {
        let known: Vec<&StructuredFinding> = call
            .findings
            .iter()
            .filter(|f| !f.value.is_null())
            .collect();
        let mut summary = format!(
            "Call with {} ended. ",
            get_default(&call.recipient, "name", "the broker")
        );
        if known.is_empty() {
            summary.push_str("No clear answers. ");
        } else {
            summary.push_str("Learned: ");
            summary.push_str(
                &known
                    .iter()
                    .map(|f| describe(f))
                    .collect::<Vec<_>>()
                    .join("; "),
            );
            summary.push_str(". ");
        }
        if !call.deferred_questions.is_empty() {
            summary.push_str(&format!(
                "Deferred to you: {}.",
                call.deferred_questions.join("; ")
            ));
        }
        let summary = ss_domain::py::strip(&summary).to_owned();
        let mut call = call;
        call.outcome_summary = Some(summary.clone());
        let call = self.save(tx, call).await?;
        if let Some(comm_id) = call.communication_id {
            if let Some(mut comm) = COMMUNICATIONS
                .get_in_project(tx, comm_id, call.project_id)
                .await?
            {
                comm.state = CommunicationState::Completed;
                comm.structured_findings = call.findings.clone();
                comm.transcript_reference =
                    Some(format!("call_sessions/{}/transcript", call.call_id));
                COMMUNICATIONS.upsert(tx, &comm).await?;
                self.app
                    .events()
                    .emit(
                        tx,
                        Emit::new(
                            EventType::OutreachCompleted,
                            call.project_id,
                            ActorType::Agent,
                            ACTOR,
                            format!("outreach:{}:completed", comm.communication_id),
                            json!({
                                "communication_id": comm.communication_id.to_string(),
                                "property_id": call.property_id.map(|p| p.to_string()),
                            }),
                        ),
                    )
                    .await?;
            }
        }
        let text = format!(
            "{}{summary}",
            if call.simulated { SIMULATED_LABEL } else { "" }
        );
        self.notify_owner(
            &call,
            text,
            "call_summary",
            format!("call-summary:{}", call.call_id),
        );
        Ok(call)
    }

    async fn set_comm_state(
        &self,
        tx: &mut dyn Tx,
        call: &CallSession,
        state: CommunicationState,
    ) -> AppResult<()> {
        if let Some(comm_id) = call.communication_id {
            if let Some(mut comm) = COMMUNICATIONS
                .get_in_project(tx, comm_id, call.project_id)
                .await?
            {
                if comm.state != state {
                    comm.state = state;
                    COMMUNICATIONS.upsert(tx, &comm).await?;
                }
            }
        }
        Ok(())
    }

    // ---- owner handoff ------------------------------------------------------------------------

    /// `request_handoff`: only from ACTIVE or AI_RESUMED.
    pub async fn request_handoff(
        &mut self,
        tx: &mut dyn Tx,
        call_id: Uuid,
        reason: HandoffReason,
        question: &str,
    ) -> AppResult<OwnerHandoffRequest> {
        let call = CALLS.require(tx, call_id).await?;
        lock_project(tx, call.project_id).await?;
        let call = CALLS.require(tx, call_id).await?;
        if !matches!(call.state, CallState::Active | CallState::AiResumed) {
            return Err(AppError::conflict(format!(
                "cannot hand off from {}",
                call.state.as_str()
            ))
            .code("INVALID_STATE_TRANSITION"));
        }
        let reference = crate::security::opaque_reference();
        let token = crate::security::issue_token_with(
            self.app.settings.auth_secret.expose(),
            call.owner_id,
            HANDOFF_TTL_S,
            &format!("join:{}", call.call_id),
        );
        {
            let mut tokens = join_tokens();
            if tokens.len() >= MAX_JOIN_TOKENS {
                tokens.clear();
            }
            tokens.insert(reference.clone(), token);
        }
        let handoff = OwnerHandoffRequest {
            handoff_request_id: Uuid::new_v4(),
            call_id: call.call_id,
            project_id: call.project_id,
            reason,
            question: question.to_owned(),
            urgency: "HIGH".into(),
            owner_id: call.owner_id,
            join_token_reference: Some(reference),
            expires_at: UtcDateTime::from_datetime(Utc::now() + Duration::seconds(HANDOFF_TTL_S)),
            status: HandoffStatus::Pending,
        };
        HANDOFFS.add(tx, &handoff).await?;
        let mut call = call.transition(CallState::HandoffPending)?;
        call.handoff_request_id = Some(handoff.handoff_request_id);
        let call = self.save(tx, call).await?;
        self.app
            .events()
            .emit(
                tx,
                Emit::new(
                    EventType::CallOwnerHandoffRequested,
                    call.project_id,
                    ActorType::Agent,
                    ACTOR,
                    format!("handoff:{}", handoff.handoff_request_id),
                    json!({
                        "call_id": call.call_id.to_string(),
                        "handoff_request_id": handoff.handoff_request_id.to_string(),
                        "reason": reason.as_str(),
                    }),
                ),
            )
            .await?;
        // The text carries no token: JOIN only makes us call the owner's own registered number.
        // A simulated call (fixture voice) has nobody on the line and JOIN dials nobody, so its
        // text says so, with the summary's label (AGENTS.md section 5). Deliberately differs
        // from Python's `request_handoff`, which sends the real-call wording for both; a real
        // call's text is unchanged byte for byte.
        let text = if call.simulated {
            format!(
                "{SIMULATED_LABEL}In this simulated call with {}, the assistant would hand you \
                 this question: \"{question}\" Nobody is on the line and nobody will call you. \
                 Reply JOIN or LATER to see what the assistant would do, or open {}/activity",
                get_default(&call.recipient, "name", "the broker"),
                self.app.settings.public_web_url
            )
        } else {
            format!(
                "Your assistant needs you on a call with {}: \"{question}\" Reply JOIN and I'll \
                 call you to connect, or LATER and I'll say you'll follow up. Or open {}/activity",
                get_default(&call.recipient, "name", "the broker"),
                self.app.settings.public_web_url
            )
        };
        self.notify_owner(
            &call,
            text,
            "handoff_request",
            format!("handoff-notify:{}", handoff.handoff_request_id),
        );
        let handoff = handoff.transition(HandoffStatus::Notified)?;
        HANDOFFS.upsert(tx, &handoff).await?;
        Ok(handoff)
    }

    /// `_owner_phone`: the linked channel's handle when it is a phone number, else the user's
    /// phone.
    async fn owner_phone(&self, tx: &mut dyn Tx, owner_id: Uuid) -> AppResult<Option<String>> {
        if let Some(ch) = channel_for_owner(tx, owner_id).await? {
            if ch.handle.starts_with('+') {
                return Ok(Some(ch.handle));
            }
        }
        Ok(users::get(tx, owner_id)
            .await?
            .and_then(|u| u.phone)
            .filter(|p| !p.is_empty()))
    }

    /// `owner_response`: JOIN (ACCEPTED), LATER (DEFERRED), DECLINED or EXPIRED.
    pub async fn owner_response(
        &mut self,
        tx: &mut dyn Tx,
        handoff_id: Uuid,
        user_id: Uuid,
        response: HandoffStatus,
    ) -> AppResult<CallSession> {
        let h = HANDOFFS.require(tx, handoff_id).await?;
        self.respond(tx, h, user_id, response).await
    }

    /// `owner_response` for an already loaded (project-checked) handoff.
    pub async fn respond(
        &mut self,
        tx: &mut dyn Tx,
        h: OwnerHandoffRequest,
        user_id: Uuid,
        response: HandoffStatus,
    ) -> AppResult<CallSession> {
        if h.owner_id != user_id {
            return Err(AppError::conflict("not the call owner").code("FORBIDDEN"));
        }
        lock_project(tx, h.project_id).await?;
        let h = HANDOFFS.require(tx, h.handoff_request_id).await?;
        let mut response = response;
        if h.expires_at < now() && h.status == HandoffStatus::Notified {
            response = HandoffStatus::Expired;
        }
        let h = h.transition(response)?;
        let call = CALLS.require(tx, h.call_id).await?;
        if call.state != CallState::HandoffPending {
            // Python reaches the same 409 later, after it may have dialed the owner.
            return Err(AppError::conflict(format!(
                "call is {}; the handoff can no longer be answered",
                call.state.as_str()
            ))
            .code("INVALID_STATE_TRANSITION"));
        }
        HANDOFFS.upsert(tx, &h).await?;
        if let Some(r) = &h.join_token_reference {
            if response != HandoffStatus::Accepted {
                join_tokens().remove(r);
            }
        }
        if response == HandoffStatus::Accepted {
            let Some(phone) = self.owner_phone(tx, call.owner_id).await? else {
                return self
                    .resume(
                        tx,
                        call,
                        &h.question,
                        "owner accepted but has no phone number on file",
                    )
                    .await;
            };
            // REVIEW FIX (rust-server-4-review F2): with live outbound calls disabled the owner
            // leg is refused before any carrier command (Python first creates the conference).
            if self.telephony.mode() == "live" && !self.app.settings.allow_live_outbound_calls {
                return Err(AppError::forbidden(
                    "live outbound calls disabled (ALLOW_LIVE_OUTBOUND_CALLS=false)",
                ));
            }
            let mut call = call;
            let mut failure: Option<String> = None;
            if call.conference_id.is_none() {
                if let Some(leg) = call.provider_call_id.clone() {
                    let name = format!("call-{}", call.call_id);
                    match self.tel(move |t| t.create_conference(&leg, &name)).await? {
                        Ok(conf) => {
                            call.conference_id = Some(conf);
                            call = self.save(tx, call).await?;
                        }
                        Err(TelephonyError::Provider(e)) => failure = Some(e.kind.to_string()),
                        Err(TelephonyError::Forbidden(m)) => return Err(AppError::forbidden(m)),
                    }
                }
            }
            let owner_leg = if failure.is_none() {
                let (from, url, key) = (
                    self.caller_number(),
                    self.webhook_url(),
                    format!("owner-leg:{}", h.handoff_request_id),
                );
                let cs = client_state(&call.call_id.to_string(), "owner");
                match self
                    .tel(move |t| t.dial(&phone, &from, &url, &key, Some(&cs)))
                    .await?
                {
                    Ok(r) => Some(r.call_control_id),
                    Err(TelephonyError::Provider(e)) => {
                        failure = Some(e.kind.to_string());
                        None
                    }
                    Err(TelephonyError::Forbidden(m)) => return Err(AppError::forbidden(m)),
                }
            } else {
                None
            };
            let Some(owner_leg) = owner_leg else {
                let why = format!(
                    "could not reach the owner ({})",
                    failure.unwrap_or_default()
                );
                return self.resume(tx, call, &h.question, &why).await;
            };
            call.participant_states
                .push(participant("owner", Some(owner_leg), "dialing"));
            let call = self.save(tx, call).await?;
            let call = self
                .say(
                    tx,
                    call,
                    "Great news, the owner is joining the call now. One moment please.",
                )
                .await?;
            return self
                .advance(
                    tx,
                    call.call_id,
                    CallState::OwnerJoining,
                    Some("calling the owner"),
                )
                .await;
        }
        let why = format!("owner {}", response.as_str().to_lowercase());
        self.resume(tx, call, &h.question, &why).await
    }

    async fn resume(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        question: &str,
        why: &str,
    ) -> AppResult<CallSession> {
        let mut call = call;
        call.deferred_questions.push(question.to_owned());
        let call = self.save(tx, call).await?;
        let detail = format!("{why}; assistant defers: {question}");
        let call = self
            .advance(tx, call.call_id, CallState::AiResumed, Some(&detail))
            .await?;
        let call = self
            .say(
                tx,
                call,
                "The owner can't join right now, so I'll make sure they follow up with you \
                 directly on that. I can't agree to anything on their behalf.",
            )
            .await?;
        self.ask_next(tx, call).await
    }

    async fn owner_leg_event(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        et: Option<&str>,
        p: &serde_json::Map<String, Value>,
    ) -> AppResult<CallSession> {
        let leg = p
            .get("call_control_id")
            .filter(|v| ss_domain::py::truthy(v))
            .map(ss_domain::py::value_str);
        if et == Some("call.answered") && call.state == CallState::OwnerJoining {
            if let (Some(conf), Some(leg)) = (
                call.conference_id.clone().filter(|c| !c.is_empty()),
                leg.clone(),
            ) {
                match self.tel(move |t| t.join_conference(&conf, &leg)).await? {
                    Ok(()) => return Ok(call),
                    Err(TelephonyError::Provider(e)) => {
                        let q = call.deferred_questions.last().cloned().unwrap_or_default();
                        return self.resume(tx, call, &q, &e.to_string()).await;
                    }
                    Err(TelephonyError::Forbidden(m)) => return Err(AppError::forbidden(m)),
                }
            }
        }
        if matches!(et, Some("conference.participant.joined" | "call.answered"))
            && call.state == CallState::OwnerJoining
        {
            let mut call = call;
            for s in call.participant_states.iter_mut() {
                if s.role == "owner" {
                    s.state = "joined".into();
                    s.updated_at = now();
                }
            }
            call.transcript
                .push(turn("system", "[owner joined the call]"));
            let call = self.save(tx, call).await?;
            return self
                .advance(
                    tx,
                    call.call_id,
                    CallState::HumanJoined,
                    Some("owner joined"),
                )
                .await;
        }
        if et == Some("call.hangup") && call.state == CallState::OwnerJoining {
            let question = match call.handoff_request_id {
                Some(hid) => HANDOFFS.require(tx, hid).await?.question,
                None => String::new(),
            };
            return self
                .resume(tx, call, &question, "owner did not answer")
                .await;
        }
        if et == Some("call.hangup") && call.state == CallState::HumanJoined {
            let mut call = call;
            call.transcript
                .push(turn("system", "[owner left the call]"));
            let call = self.save(tx, call).await?;
            let call = self
                .advance(
                    tx,
                    call.call_id,
                    CallState::AiResumed,
                    Some("owner left; assistant resumes"),
                )
                .await?;
            return self.ask_next(tx, call).await;
        }
        Ok(call)
    }

    /// `expire_stale_handoffs`.
    pub async fn expire_stale_handoffs(
        &mut self,
        tx: &mut dyn Tx,
        call_id: Uuid,
    ) -> AppResult<CallSession> {
        let call = CALLS.require(tx, call_id).await?;
        let Some(hid) = call.handoff_request_id else {
            return Ok(call);
        };
        if call.state != CallState::HandoffPending {
            return Ok(call);
        }
        let h = HANDOFFS.require(tx, hid).await?;
        if h.expires_at < now()
            && matches!(h.status, HandoffStatus::Pending | HandoffStatus::Notified)
        {
            let owner = h.owner_id;
            return self.respond(tx, h, owner, HandoffStatus::Expired).await;
        }
        Ok(call)
    }

    /// `pending_handoff_for_owner`: the owner's NOTIFIED handoff that expires last (the first of
    /// equals, as Python's `max`).
    pub async fn pending_handoff_for_owner(
        &self,
        tx: &mut dyn Tx,
        owner_id: Uuid,
    ) -> AppResult<Option<OwnerHandoffRequest>> {
        let rows = HANDOFFS
            .list(
                tx,
                Query::new().eq("status", HandoffStatus::Notified.as_str()),
            )
            .await?;
        let mut best: Option<OwnerHandoffRequest> = None;
        for h in rows.into_iter().filter(|h| h.owner_id == owner_id) {
            if best.as_ref().is_none_or(|b| h.expires_at > b.expires_at) {
                best = Some(h);
            }
        }
        Ok(best)
    }
}

/// Number of join tokens currently held (tests).
pub fn join_token_count() -> usize {
    join_tokens().len()
}
