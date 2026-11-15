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
use std::sync::{Arc, LazyLock, Mutex, RwLock, Weak};

use chrono::{Duration, Utc};
use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_core::{DataOrigin, EvidenceClassification, UtcDateTime};
use ss_domain::communications::call_purposes::{
    self as cp, Approved, CallPurposes, PurposeOutcome, PurposeState,
};
use ss_domain::communications::call_script::{AVAILABILITY_QUESTION, QUESTION_BANK};
use ss_domain::communications::call_summary::{self as csum, CallEnd, InputFinding};
use ss_domain::communications::response_agent::{
    self as agent, CallContext, ConversationState, EscalationRule, Say, Utterance,
};
use ss_domain::communications::{
    availability, build_script, call_payload, describe, detect_escalation, extract,
};
use ss_domain::contracts::{
    CallQuestion, CallSession, CallState, CommunicationState, HandoffReason, HandoffStatus,
    OwnerHandoffRequest, ParticipantState, SourceEvidence, StructuredFinding, TranscriptTurn,
};
use ss_domain::integrations::telnyx::{
    client_state, decode_client_state_json, StreamOptions, TelephonyError, TelephonyProvider,
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
use crate::services::voice_bridge::{
    BridgeEnd, BridgeEvents, BridgeHandle, BridgeOptions, BridgeRegistry, CallBridgeSpec,
    Speaker as BridgeSpeaker, ToolCall, TranscriptLine,
};
use crate::services::voice_session_config as vsc;
use crate::store::comms::{
    call_by_provider_id, channel_for_owner, CALLS, COMMUNICATIONS, HANDOFFS,
};
use crate::store::repo::{users, ASSESSMENTS, PROFILES, PROJECTS};
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
/// What everything shown about a live test call starts with: the call was dialed to the owner's
/// own test number (`LIVE_CALLS_ONLY_TO`) instead of the listing's contact.
pub const TEST_CALL_LABEL: &str = "TEST CALL: ";

/// The first transcript line of a live test call (a system line). `masked` is the test number
/// as [`crate::config::mask_number`] shows it; the number itself is never written.
pub fn test_call_note(masked: &str) -> String {
    format!(
        "{TEST_CALL_LABEL}this call was dialed to the owner's test number ({masked}) instead of \
         the listing's contact. Nothing was sent to the contact. The person answering is the \
         owner, so nothing said on this call is stored as evidence about the property."
    )
}

/// What an event or an owner text about a live test call starts with.
pub const TEST_CALL_PREFIX: &str = "TEST CALL: dialed to the owner's test number instead of the \
     listing's contact; nothing was sent to the contact. ";

/// The system line when the owner accepts a handoff on a live test call.
pub const TEST_CALL_OWNER_LINE: &str = "[test mode: the owner is already the person on the \
     line (the call was dialed to the owner's test number), so no second call was placed]";

/// Was this call dialed to the owner's test number instead of its recipient? Read from the
/// stored record (its first transcript line, which only the server writes), so it holds after a
/// restart and whatever the settings are by then.
pub fn is_test_call(call: &CallSession) -> bool {
    call.transcript
        .first()
        .is_some_and(|t| t.speaker == "system" && t.text.starts_with(TEST_CALL_LABEL))
}

/// The label every owner text about a simulated call starts with (Python's summary prefix).
pub const SIMULATED_LABEL: &str = "[Simulated call] ";

/// The claim type of a request's own question that is not a bank question starts with this
/// (`plan_questions` gives a bridged call's the same form). No scripted answer exists for one.
pub const REQUEST_CLAIM_PREFIX: &str = "outreach_question_";

/// Whether a script question is the request's own and not a bank question.
pub fn is_request_question(q: &CallQuestion) -> bool {
    q.claim_type.starts_with(REQUEST_CLAIM_PREFIX)
}

fn same_question(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// The script of a simulated call the server leads itself (no voice bridge): the approved
/// request's own questions first, in their order (blank ones and repeats dropped), then the
/// bank's (`build_script`) as before, without the ones the request already asks. A request
/// question that is a bank question (the same text, case and outer spaces aside) keeps the
/// bank's claim type, so its scripted answer and its extraction apply; any other one gets
/// `outreach_question_<i>`, for which the simulated contact has no answer.
pub fn simulated_script(
    request_questions: &[String],
    bank: Vec<CallQuestion>,
) -> Vec<CallQuestion> {
    let mut out: Vec<CallQuestion> = Vec::new();
    for (i, q) in request_questions.iter().enumerate() {
        let text = q.trim();
        if text.is_empty() || out.iter().any(|o| same_question(&o.question, text)) {
            continue;
        }
        let known = bank
            .iter()
            .find(|b| same_question(&b.question, text))
            .cloned()
            .or_else(|| {
                QUESTION_BANK
                    .iter()
                    .find(|(_, question, _)| same_question(question, text))
                    .map(|(_, question, claim)| CallQuestion {
                        question: (*question).to_owned(),
                        claim_type: (*claim).to_owned(),
                        requirement_id: None,
                    })
            })
            .or_else(|| same_question(AVAILABILITY_QUESTION, text).then(availability));
        out.push(known.unwrap_or_else(|| CallQuestion {
            question: text.to_owned(),
            claim_type: format!("{REQUEST_CLAIM_PREFIX}{i}"),
            requirement_id: None,
        }));
    }
    for b in bank {
        if !out.iter().any(|o| same_question(&o.question, &b.question)) {
            out.push(b);
        }
    }
    out
}

/// At most this many questions are named in one sentence of the summary; the rest are counted.
const NOTE_MAX_QUESTIONS: usize = 5;
const NOTE_MAX_CHARS: usize = 160;

fn quoted_list(questions: &[&CallQuestion]) -> String {
    let mut parts: Vec<String> = questions
        .iter()
        .take(NOTE_MAX_QUESTIONS)
        .map(|q| {
            let mut text: String = q.question.chars().take(NOTE_MAX_CHARS).collect();
            if q.question.chars().count() > NOTE_MAX_CHARS {
                text.push_str("...");
            }
            format!("\"{text}\"")
        })
        .collect();
    if questions.len() > NOTE_MAX_QUESTIONS {
        parts.push(format!("and {} more", questions.len() - NOTE_MAX_QUESTIONS));
    }
    parts.join("; ")
}

/// What became of the approved request's own questions on a simulated call the server led:
/// which got the simulated contact's "cannot answer" line, which were asked without an answer
/// before the end, and which were never asked. `None` when there is nothing to say (always, for
/// a request without questions). `approved` is the request's stored list when the process
/// still has it; without it only the questions that are not bank questions are known.
fn request_questions_note(call: &CallSession, approved: &[String]) -> Option<String> {
    let done = usize::try_from(call.question_index).unwrap_or(0);
    let spoken = |q: &CallQuestion| {
        call.transcript
            .iter()
            .any(|t| t.speaker == "assistant" && t.text.starts_with(&q.question))
    };
    let (mut unanswered, mut cut, mut unasked) = (Vec::new(), Vec::new(), Vec::new());
    for (i, q) in call.script.iter().enumerate() {
        let own = is_request_question(q);
        if !own && !approved.iter().any(|a| same_question(a, &q.question)) {
            continue;
        }
        if i < done {
            if own {
                unanswered.push(q);
            }
        } else if spoken(q) {
            cut.push(q);
        } else {
            unasked.push(q);
        }
    }
    let mut out: Vec<String> = Vec::new();
    if !unanswered.is_empty() {
        out.push(format!(
            "Of your own questions, the simulated contact has no scripted answer for: {}.",
            quoted_list(&unanswered)
        ));
    }
    if !cut.is_empty() {
        out.push(format!(
            "Asked, with no answer before the call ended: {}.",
            quoted_list(&cut)
        ));
    }
    if !unasked.is_empty() {
        out.push(format!(
            "Not asked before the call ended: {}.",
            quoted_list(&unasked)
        ));
    }
    (!out.is_empty()).then(|| out.join(" "))
}

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

/// What an outreach request carried besides the fields of its `CommunicationSession`: its own
/// questions and what the owner approved the call to do (a tour, rent limits). Kept on the
/// server only, never in a response, an event or a log (the limits hold the maximum rent), so
/// it has no `Debug`.
#[derive(Clone, Default, PartialEq)]
pub struct RequestExtras {
    pub questions: Vec<String>,
    pub purposes: Option<CallPurposes>,
}

impl RequestExtras {
    fn has_purposes(&self) -> bool {
        self.purposes
            .as_ref()
            .is_some_and(|p| p.tour.is_some() || p.negotiation.is_some())
    }
}

/// The key of the approval payload that binds an approval to the request's purposes and
/// questions.
pub const REQUEST_DIGEST_KEY: &str = "request_digest";

/// A keyed digest (HMAC-SHA256 with the service secret) of the request's purposes and
/// questions, present only when the request has purposes. It goes into the approval payload,
/// so `require_approved` refuses a start whose stored purposes or questions are not the ones
/// the approval was given for; being keyed, it cannot be searched for the maximum.
pub fn request_digest(secret: &str, extras: &RequestExtras) -> Option<String> {
    if !extras.has_purposes() {
        return None;
    }
    let body = json!({"purposes": extras.purposes, "questions": extras.questions}).to_string();
    Some(crate::security::sign(secret, &body))
}

/// What a call may do besides its questions, with where it stands (process-local, by call id,
/// dropped at any terminal state: a restart during a call loses it, and the call then has no
/// tour or rent tool answers but refusals).
#[derive(Clone, Default)]
struct CallExtras {
    questions: Vec<String>,
    approved: Approved,
    state: PurposeState,
    /// A "would the owner go to ..." line whose `evaluate_rent` call has not arrived yet.
    pending_concession: Option<String>,
    /// `evaluate_rent` answered since the last counterparty line.
    rent_tool_since_line: bool,
    /// REVIEW FIX (voice-glue-1-review R1): the last purpose tool call (the bridge's turn, the
    /// name, the arguments) with its answer, so a repeat in the same turn is answered once.
    last_tool: Option<(u64, String, Value, String)>,
}

/// What a closed call leaves for its summary.
#[derive(Default)]
struct ClosedVoice {
    agent: Option<(ConversationState, Option<CallContext>)>,
    extras: Option<CallExtras>,
}

/// `CallOrchestrator`. Owner texts it asks for are collected in `notices`; the caller sends them
/// after its transaction commits.
pub struct CallOrchestrator<'a> {
    app: &'a App,
    telephony: Arc<dyn TelephonyProvider>,
    pub notices: Vec<NotifyRequest>,
    /// The process-local state of calls this orchestrator closed, kept for `finalize`.
    closed: HashMap<Uuid, ClosedVoice>,
}

impl<'a> CallOrchestrator<'a> {
    pub fn new(app: &'a App) -> Self {
        let telephony = app.voice().telephony.clone();
        Self {
            app,
            telephony,
            notices: Vec::new(),
            closed: HashMap::new(),
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
        let test_detail = is_test_call(call).then(|| {
            format!("{TEST_CALL_PREFIX}{}", detail.unwrap_or(""))
                .trim_end()
                .to_owned()
        });
        let detail = test_detail.as_deref().or(detail);
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
        if let Some(control) = self.app.voice_calls().speaking_control(&call) {
            if !control.say(text.clone()).await {
                text = format!("{text} [speak failed: bridge gone]");
            }
            let mut call = call;
            call.transcript.push(turn("assistant", text));
            return self.save(tx, call).await;
        }
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
        // Every owner text about a live test call says so first.
        let text = if is_test_call(call) {
            format!("{TEST_CALL_PREFIX}{text}")
        } else {
            text
        };
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
        // The request's own questions and purposes are the ones stored at outreach under the
        // communication id; nothing supplied at start can replace them. Their digest is part of
        // the approved payload, so an approval covers exactly the request it was given for.
        let extras = s
            .communication_id
            .and_then(|id| self.app.voice_calls().request_extras(id))
            .unwrap_or_default();
        let mut payload = call_payload(s.project_id, &s.recipient, s.property_id);
        if let Some(d) = request_digest(self.app.settings.auth_secret.expose(), &extras) {
            payload.insert(REQUEST_DIGEST_KEY.into(), Value::String(d));
        }
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
        // With the voice bridge on, the introduction is the owner's ACTIVE recording (read under
        // the owner's recording lock), or none: the recording is optional (owner, 07:16Z), and a
        // call without one opens with the server's identification line. With the bridge off, the
        // request's reference, as before.
        let use_active = self.app.voice_calls().enabled();
        let intro = if use_active {
            tx.advisory_xact_lock(crate::services::design::lock_key(
                "intro-recording",
                s.owner_id,
            ))
            .await?;
            crate::services::intro_recording::active_for_call(tx, s.owner_id).await?
        } else {
            None
        };
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
        call.intro_recording_reference = if use_active {
            intro.as_ref().map(|i| i.object_key.clone())
        } else {
            s.intro_recording_reference
        };
        call.communication_id = s.communication_id;
        call.script = build_script(&assessments);
        call.simulated = s.simulated;
        // A simulated call the server leads itself asks the approved request's own questions
        // first: `extras` is the request stored at outreach (read above, before the approval
        // check), never anything supplied at start. A bridged call plans them at the bridge.
        if s.simulated && !use_active && !extras.questions.is_empty() {
            call.script = simulated_script(&extras.questions, std::mem::take(&mut call.script));
        }
        let Some(phone) = get(&s.recipient, "phone").map(str::to_owned) else {
            return Err(AppError::conflict("recipient has no phone number"));
        };
        // Live calls restricted to the owner's test number (`LIVE_CALLS_ONLY_TO`): that number
        // is dialed instead of the recipient's, which never leaves this function (the carrier
        // client substitutes it again), and the record says so in its first line.
        let test_number = (!s.simulated && self.telephony.mode() == "live")
            .then(|| self.app.settings.live_test_number())
            .flatten();
        let phone = match test_number {
            Some(n) => {
                call.transcript.push(turn(
                    "system",
                    test_call_note(&crate::config::mask_number(&n)),
                ));
                n
            }
            None => phone,
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
        self.begin_purposes(tx, &call, extras).await?;
        if let Some(id) = s.communication_id {
            self.app.voice_calls().forget_request(id);
        }
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
        if is_terminal(target) {
            let left = self.app.voice_calls().close(call.call_id).await;
            self.closed.insert(call.call_id, left);
        } else if target == CallState::AiResumed {
            self.app.voice_calls().unpause(call.call_id).await;
        }
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
        if self.app.voice_calls().is_bridged(call.call_id) {
            if let Some(call) = self.bridged_event(tx, &call, et, p).await? {
                return Ok(Some(call));
            }
        }
        if et == Some("call.answered") && call.state == CallState::Dialing {
            return self.on_answered(tx, call).await.map(Some);
        }
        if et == Some("call.machine.detection.ended")
            && p.get("result").and_then(Value::as_str) == Some("machine")
        {
            // With the voice bridge on, nothing is spoken on the carrier to a machine: not over
            // the owner's recording, not over the assistant. The call is hung up without a
            // message and the reason is stored (a voicemail policy is the owner's to decide).
            let vcs = self.app.voice_calls();
            if vcs.enabled() || vcs.is_bridged(call.call_id) {
                vcs.pause(call.call_id).await;
                let mut call = call;
                call.transcript.push(turn("system", VOICEMAIL_NOTE));
                let call = self.save(tx, call).await?;
                if let Some(leg) = call.provider_call_id.clone() {
                    let _ = self.tel(move |t| t.hangup(&leg)).await?;
                }
                return Ok(Some(call));
            }
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
        if self.app.voice_calls().enabled() {
            return self.answered_bridged(tx, call).await;
        }
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
        if self.app.voice_calls().is_bridged(call.call_id) {
            return self.agent_resume(tx, call).await;
        }
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
            let mut finding = extract(&q, text);
            if call.simulated && is_request_question(&q) {
                // The simulation has no scripted answer to a request's own question: whatever
                // was said, the question stays unanswered and nothing becomes evidence.
                finding.value = Value::Null;
                finding.unit = None;
            }
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
        // A live test call is answered by the owner, not the contact: nothing said on it becomes
        // evidence about the property. The finding stays on the call's own event, labelled.
        let test_call = is_test_call(call);
        if let Some(property_id) = call.property_id.filter(|_| !test_call) {
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
                        "summary": if test_call {
                            format!(
                                "{TEST_CALL_LABEL}said by the owner on the test number, not by \
                                 the listing's contact; not stored as evidence. {}",
                                describe(finding)
                            )
                        } else {
                            describe(finding)
                        },
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
        let prior = call.state;
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
        self.finalize(tx, call, prior).await
    }

    /// The owner's monthly rent limit of the project's business profile, in cents.
    async fn rent_limit_cents(&self, tx: &mut dyn Tx, project_id: Uuid) -> AppResult<Option<i64>> {
        Ok(PROFILES
            .list(tx, Query::new().eq("project_id", project_id))
            .await?
            .into_iter()
            .next()
            .and_then(|p| p.monthly_rent_limit))
    }

    fn label(call: &CallSession) -> &'static str {
        if call.simulated {
            SIMULATED_LABEL
        } else {
            ""
        }
    }

    fn contact(call: &CallSession) -> String {
        let name = vsc::field(get_default(&call.recipient, "name", ""));
        let role = vsc::field(get_default(&call.recipient, "role", ""));
        match (name.is_empty(), role.is_empty()) {
            (true, true) => "the listing contact".to_owned(),
            (true, false) => format!("the {role}"),
            (false, true) => name,
            (false, false) => format!("{name} ({role})"),
        }
    }

    /// At call start: validate what the owner approved against the profile's rent limit and
    /// keep it, with the request's questions, for the call. A refused purpose does not stop
    /// the call; the owner is told why.
    async fn begin_purposes(
        &mut self,
        tx: &mut dyn Tx,
        call: &CallSession,
        extras: RequestExtras,
    ) -> AppResult<()> {
        let has = extras.has_purposes();
        if !has && extras.questions.is_empty() {
            return Ok(());
        }
        let owner = self.owner_name(tx, call.owner_id).await?;
        let limit = self.rent_limit_cents(tx, call.project_id).await?;
        let mut approved = cp::approve(&extras.purposes.unwrap_or_default(), &owner, limit);
        // The refusals name the profile's limit at most, never the request's maximum.
        let mut notes = std::mem::take(&mut approved.refusals);
        if has && !self.app.voice_calls().enabled() {
            approved.tour = None;
            approved.negotiation = None;
            notes = vec![
                "The tour request and the rent limits are not used: the voice assistant is \
                 switched off, so this call only asks its questions."
                    .to_owned(),
            ];
        }
        if !notes.is_empty() {
            let text = format!(
                "{}About the call to {}: {} The call goes ahead without it.",
                Self::label(call),
                Self::contact(call),
                notes.join(" ")
            );
            self.notify_owner(
                call,
                text,
                "call_purposes",
                format!("call-purposes:{}", call.call_id),
            );
        }
        self.app.voice_calls().set_extras(
            call.call_id,
            CallExtras {
                questions: extras.questions,
                approved,
                ..CallExtras::default()
            },
        );
        Ok(())
    }

    /// The owner notices for what a tool call changed (never sent to the counterparty).
    fn notify_outcomes(&mut self, call: &CallSession, before: &PurposeState, after: &PurposeState) {
        let old = cp::outcomes(before);
        let (label, who) = (Self::label(call), Self::contact(call));
        for o in cp::outcomes(after) {
            if old.contains(&o) {
                continue;
            }
            match o {
                PurposeOutcome::RentWithinLimits {
                    amount_cents,
                    at_or_under_target,
                    ..
                } => {
                    let text = format!(
                        "{label}Needs your decision: on the call, {who} said {} a month. That \
                         is {}. Nothing is agreed: the assistant only said it would bring the \
                         number to you.",
                        agent::format_money(amount_cents, "USD"),
                        if at_or_under_target {
                            "at or under your target"
                        } else {
                            "inside the limit you set"
                        }
                    );
                    self.notify_owner(
                        call,
                        text,
                        "rent_decision",
                        format!("call-rent:{}:{amount_cents}", call.call_id),
                    );
                }
                PurposeOutcome::TourBooked { start, end, .. } => {
                    let text = format!(
                        "{label}Tour set on the call with {who}: {} (the property's local \
                         time), {} minutes, inside your times. This is the assistant's record \
                         only: nothing was sent to them in writing.",
                        cp::spoken_time(&start),
                        (end - start).num_minutes()
                    );
                    self.notify_owner(
                        call,
                        text,
                        "tour_booked",
                        format!("call-tour:{}:{}", call.call_id, start.to_rfc3339()),
                    );
                }
                PurposeOutcome::TourMoved {
                    from_start,
                    start,
                    end,
                    ..
                } => {
                    let text = format!(
                        "{label}Tour moved on the call with {who}, at their request: from {} \
                         to {} (the property's local time), {} minutes, inside your times. \
                         This is the assistant's record only: nothing was sent to them in \
                         writing.",
                        cp::spoken_time(&from_start),
                        cp::spoken_time(&start),
                        (end - start).num_minutes()
                    );
                    self.notify_owner(
                        call,
                        text,
                        "tour_booked",
                        format!("call-tour:{}:{}", call.call_id, start.to_rfc3339()),
                    );
                }
                PurposeOutcome::TourNeedsOwner { .. } => {
                    let text = format!(
                        "{label}Follow-up from the call with {who}: they could not make any of \
                         your tour times and are waiting for other times from you."
                    );
                    self.notify_owner(
                        call,
                        text,
                        "tour_followup",
                        format!("call-tour-followup:{}", call.call_id),
                    );
                }
                PurposeOutcome::RentAboveLimits { .. } => {}
            }
        }
    }

    /// How the call ended, for the summary: every state named.
    fn call_end(prior: CallState, call: &CallSession) -> CallEnd {
        if machine_detected(call) {
            return CallEnd::Voicemail;
        }
        match (call.state, prior) {
            (CallState::Declined, _) => CallEnd::NoAnswer,
            // No "cancelled" end exists in the summary; a cancelled call did not complete.
            (CallState::Failed | CallState::Cancelled, _) => CallEnd::Failed,
            (_, CallState::HumanJoined) => CallEnd::HandedToOwner,
            (_, CallState::Intro | CallState::Dialing | CallState::Created) => CallEnd::HungUp,
            (
                _,
                CallState::Active
                | CallState::AiResumed
                | CallState::HandoffPending
                | CallState::OwnerJoining
                | CallState::Completed
                | CallState::Failed
                | CallState::Declined
                | CallState::Cancelled,
            ) => CallEnd::Completed,
        }
    }

    /// The structured summary's owner text for a bridged call or a call with purposes: the
    /// Response Agent's kept findings with their cents, the planned questions, the handoffs,
    /// then the purposes' owner lines inside the text's bound.
    async fn voice_summary(
        &mut self,
        tx: &mut dyn Tx,
        call: &CallSession,
        prior: CallState,
        left: &ClosedVoice,
    ) -> AppResult<String> {
        let handoffs: Vec<OwnerHandoffRequest> = HANDOFFS
            .list(tx, Query::new().eq("project_id", call.project_id))
            .await?
            .into_iter()
            .filter(|h| h.call_id == call.call_id)
            .collect();
        let limit = self.rent_limit_cents(tx, call.project_id).await?;
        let mut input =
            csum::SummaryInput::from_call(call, Self::call_end(prior, call), handoffs, limit);
        if let Some((state, ctx)) = &left.agent {
            if !state.findings.is_empty() {
                input.findings = state
                    .findings
                    .iter()
                    .map(|f| {
                        let mut i = InputFinding::from(f);
                        // A range finding has a string value and no cents; a long bare number
                        // (an account-like value) is not carried into the summary.
                        if i.value.as_i64().is_some_and(|n| n.abs() >= 100_000_000)
                            && i.amount_cents.is_none()
                        {
                            i.value = Value::String("[REDACTED]".into());
                        }
                        i
                    })
                    .collect();
            }
            if let Some(ctx) = ctx.as_ref().filter(|c| !c.questions.is_empty()) {
                input.questions = ctx
                    .questions
                    .iter()
                    .map(|q| csum::OutreachQuestion {
                        id: q.id.clone(),
                        text: q.text.clone(),
                        topic: q.topic.clone(),
                        claim_type: q.claim_type.clone(),
                    })
                    .collect();
            }
        }
        let mut text = csum::owner_text(&csum::summarize(&input));
        if let Some(extras) = &left.extras {
            for line in cp::owner_lines(&extras.state) {
                if text.chars().count() + 1 + line.chars().count() <= csum::MAX_OWNER_TEXT_CHARS {
                    text.push(' ');
                    text.push_str(&line);
                }
            }
        }
        Ok(text)
    }

    async fn finalize(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        prior: CallState,
    ) -> AppResult<CallSession> {
        let left = self.closed.remove(&call.call_id).unwrap_or_default();
        let mut call = call;
        if let Some(q) = left
            .extras
            .as_ref()
            .and_then(|e| e.pending_concession.clone())
        {
            // A proposal the rent tool never answered still reaches the owner.
            call.deferred_questions.push(q);
        }
        let voice = left.agent.is_some()
            || left
                .extras
                .as_ref()
                .is_some_and(|e| e.approved.tour.is_some() || e.approved.negotiation.is_some());
        let voice_text = if voice {
            Some(self.voice_summary(tx, &call, prior, &left).await?)
        } else {
            None
        };
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
        if call.simulated && !voice {
            let approved = left.extras.as_ref().map_or(&[][..], |e| &e.questions[..]);
            if let Some(note) = request_questions_note(&call, approved) {
                if !summary.ends_with(' ') {
                    summary.push(' ');
                }
                summary.push_str(&note);
            }
        }
        // A bridged call, or one with purposes, gets the structured summary's owner text (it
        // carries the simulated label itself); every other call keeps Python's text.
        let (summary, text) = match voice_text {
            Some(t) => (t.clone(), t),
            None => {
                let summary = ss_domain::py::strip(&summary).to_owned();
                let text = format!("{}{summary}", Self::label(&call));
                (summary, text)
            }
        };
        // REVIEW FIX (voice-only-to-1-review F1): the stored summary of a live test call says,
        // like the owner text (`notify_owner`), that the owner's test number was dialed and not
        // the contact; it named the contact as the person called.
        call.outcome_summary = Some(if is_test_call(&call) {
            format!("{TEST_CALL_PREFIX}{summary}")
        } else {
            summary
        });
        let call = self.save(tx, call).await?;
        if let Some(comm_id) = call.communication_id {
            if let Some(mut comm) = COMMUNICATIONS
                .get_in_project(tx, comm_id, call.project_id)
                .await?
            {
                comm.state = CommunicationState::Completed;
                // A live test call was answered by the owner: what was said stays on the call
                // record and is not copied to the contact's communication.
                if !is_test_call(&call) {
                    comm.structured_findings = call.findings.clone();
                }
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
        // In the three handoff states nothing flows through the bridge either way.
        self.app.voice_calls().pause(call.call_id).await;
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
        if response == HandoffStatus::Accepted && is_test_call(&call) {
            // The owner's own leg would ring the phone that is already on this call: it is not
            // dialed. The accepted handoff is stored above; the record says why nobody joined.
            let mut call = call;
            call.transcript.push(turn("system", TEST_CALL_OWNER_LINE));
            let call = self.save(tx, call).await?;
            return self
                .resume(
                    tx,
                    call,
                    &h.question,
                    "test mode: the owner is already the person on the line, no second call \
                     was placed",
                )
                .await;
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
            // On a bridged call nothing says the owner is joining: only HUMAN_JOINED says joined.
            let line = if self.app.voice_calls().is_bridged(call.call_id) {
                HOLD_FOR_OWNER_LINE
            } else {
                "Great news, the owner is joining the call now. One moment please."
            };
            let call = self.say(tx, call, line).await?;
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
            let call = self
                .advance(
                    tx,
                    call.call_id,
                    CallState::HumanJoined,
                    Some("owner joined"),
                )
                .await?;
            if self.app.voice_calls().is_bridged(call.call_id) {
                // The bridge is paused, so this goes out on the carrier (`speak`).
                let client = self.owner_name(tx, call.owner_id).await?;
                return self.say(tx, call, &owner_joined_line(&client)).await;
            }
            return Ok(call);
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

// ---- Grok Voice: the recording, then the bridge ---------------------------------------------
//
// With the bridge on (`Settings::voice_bridge_active`, or a launcher a test installs), a call
// runs: answered -> INTRO, play the owner's ACTIVE recording and nothing else -> the carrier's
// `call.playback.ended` for exactly that playback -> ACTIVE: the per-call stream secret, the
// server-built session, `streaming_start`; the bridge speaks our opening line verbatim before the
// model may speak -> every finished counterparty line goes through the Response Agent and the
// functions above (`record_finding`, `request_handoff`) -> the three handoff states pause the
// bridge -> any terminal state closes it. With the bridge off nothing below runs.

/// The polite fixed line when the recording, the stream or the bridge fails.
pub const APOLOGY_LINE: &str = "I'm sorry, we're having a technical problem on our side. The \
                                owner will follow up with you directly. Goodbye.";
/// Said (on the carrier) when the owner accepted a handoff and is being dialed.
pub const HOLD_FOR_OWNER_LINE: &str = "Please hold for a moment.";
/// Said by the server only on the HUMAN_JOINED transition. To a landlord "the owner" is the
/// property's owner, so the client is named.
pub fn owner_joined_line(client: &str) -> String {
    format!("{} has joined the call.", client_name(client))
}
/// The client's name as one bounded line (`voice_session_config::field`), or a neutral phrase.
fn client_name(client: &str) -> String {
    let c = vsc::field(client);
    if c.is_empty() {
        "The person I am calling for".to_owned()
    } else {
        c
    }
}
/// A commitment or a sensitive disclosure the model asked about: never approved by a tool
/// call. The client is named.
pub fn not_approved_output(client: &str) -> String {
    let c = client_name(client);
    json!({
        "status": "not_approved",
        "detail": format!(
            "Not approved; {c} has been asked. Say that you have asked {c}, and do not agree \
             to or disclose anything."
        ),
    })
    .to_string()
}
/// An escalation the model asked for.
pub fn owner_asked_output(client: &str) -> String {
    let c = client_name(client);
    json!({
        "status": "owner_asked",
        "detail": format!("{c} has been asked. Say so and wait; do not answer for them."),
    })
    .to_string()
}
/// The answer when no call or client can be named (an unknown call, an error).
pub const NOT_APPROVED_OUTPUT: &str = r#"{"status":"not_approved","detail":"Not approved; the person you are calling for has been asked. Say that you have asked them, and do not agree to or disclose anything."}"#;
/// The stored reason when an answering machine was detected on a call with the voice bridge.
pub const VOICEMAIL_NOTE: &str = "[answering machine detected; hung up without leaving a message]";
pub const UNKNOWN_TOOL_OUTPUT: &str = r#"{"status":"unknown_tool"}"#;
/// How long after the Response Agent's closing line the call is hung up.
pub const CLOSING_HANGUP_DELAY_S: u64 = 8;

/// The transcript note when the bridge refused a direction (it had ended).
pub const DIRECTION_REFUSED_NOTE: &str =
    "[a direction to the voice assistant was not sent: bridge gone]";
/// The transcript note when a direction was withheld because it held a rent figure of the
/// owner's profile.
pub const DIRECTION_WITHHELD_NOTE: &str =
    "[a direction to the voice assistant was withheld: it held the profile's rent figure]";

/// What becomes of one `Say::Instruct` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Direction {
    Send(String),
    /// Nothing is left after cleaning.
    Nothing,
    /// It holds the profile's rent statement: never sent.
    Withheld,
}

/// A `Say::Instruct` text as it is sent to the bridge. The Response Agent builds it from its
/// own fixed words plus three kinds of stored text, each of which is cleaned here with
/// [`vsc::field`] (one bounded line): the question to ask next (the owner's request or the
/// question bank), a requirement statement of the owner's profile, and a recorded fact about
/// the property (evidence). Nothing the counterparty said is in it. The whole is then made one
/// line and cut to the bridge's bound. A direction that holds the profile's rent statement is
/// withheld: the owner's rent figure is never sent as a direction.
pub fn direction_text(text: &str, d: &agent::Decision, ctx: Option<&CallContext>) -> Direction {
    let mut pieces: Vec<&str> = Vec::new();
    if let Some(q) = &d.next_question {
        pieces.push(&q.text);
    }
    if let Some(c) = ctx {
        if c.owner_requirements
            .iter()
            .any(|r| r.topic == "rent" && !r.statement.is_empty() && text.contains(&r.statement))
        {
            return Direction::Withheld;
        }
        pieces.extend(c.owner_requirements.iter().map(|r| r.statement.as_str()));
        pieces.extend(c.known_facts.iter().map(|f| f.statement.as_str()));
    }
    // Longest first, so a piece that holds another is cleaned as one.
    pieces.sort_by_key(|p| std::cmp::Reverse(p.len()));
    pieces.dedup();
    let mut out = text.to_owned();
    for p in pieces {
        let clean = vsc::field(p);
        if !p.is_empty() && clean != p && out.contains(p) {
            out = out.replace(p, &clean);
        }
    }
    let line: String = out
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{85}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let line: String = line
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(crate::services::voice_bridge::MAX_INSTRUCTION_CHARS)
        .collect();
    if line.is_empty() {
        Direction::Nothing
    } else {
        Direction::Send(line)
    }
}

/// The orchestrator's control of one bridge (the live [`BridgeHandle`], or a test's fixture).
/// Each call answers whether the bridge took it.
#[async_trait::async_trait]
pub trait BridgeControl: Send + Sync {
    async fn say(&self, text: String) -> bool;
    /// A direction for the model's next turns, never spoken as such and starting no response.
    async fn instruct(&self, text: String) -> bool;
    async fn pause(&self) -> bool;
    async fn resume(&self) -> bool;
    async fn stop(&self) -> bool;
}

#[async_trait::async_trait]
impl BridgeControl for BridgeHandle {
    async fn say(&self, text: String) -> bool {
        BridgeHandle::say(self, text).await.is_ok()
    }
    async fn instruct(&self, text: String) -> bool {
        BridgeHandle::instruct(self, text).await.is_ok()
    }
    async fn pause(&self) -> bool {
        BridgeHandle::pause(self).await.is_ok()
    }
    async fn resume(&self) -> bool {
        BridgeHandle::resume(self).await.is_ok()
    }
    async fn stop(&self) -> bool {
        BridgeHandle::stop(self).await.is_ok()
    }
}

/// Registers a call's bridge before the carrier's stream connects.
pub trait BridgeLauncher: Send + Sync {
    /// The bridge id (the stream URL's path segment) and its control.
    fn expect(&self, spec: CallBridgeSpec) -> (String, Arc<dyn BridgeControl>);
    fn remove(&self, bridge_id: &str);
}

/// The live launcher: the registry the carrier's WebSocket route admits from.
pub struct LiveBridgeLauncher {
    pub registry: BridgeRegistry,
}

impl BridgeLauncher for LiveBridgeLauncher {
    fn expect(&self, spec: CallBridgeSpec) -> (String, Arc<dyn BridgeControl>) {
        let (id, handle) = self.registry.expect(spec);
        (id, Arc::new(handle))
    }
    fn remove(&self, bridge_id: &str) {
        self.registry.remove(bridge_id);
    }
}

/// One bridged call's process-local state (lost on restart: a call in progress then gets no
/// more decisions, and its stream is refused).
struct VoiceCall {
    bridge_id: Option<String>,
    control: Option<Arc<dyn BridgeControl>>,
    /// SHA-256 of the playback URL of the recording; the ended event must carry that URL.
    intro_url_sha: Option<String>,
    paused: bool,
    context: Option<CallContext>,
    agent: ConversationState,
    next_turn: u64,
}

impl VoiceCall {
    fn new() -> Self {
        Self {
            bridge_id: None,
            control: None,
            intro_url_sha: None,
            paused: false,
            context: None,
            agent: ConversationState::default(),
            next_turn: 1,
        }
    }
}

/// The voice bridges of this process: the carrier route's registry, the launcher (None: the
/// bridge is off), and one entry per bridged call.
pub struct VoiceCalls {
    pub registry: BridgeRegistry,
    launcher: RwLock<Option<Arc<dyn BridgeLauncher>>>,
    app: RwLock<Weak<App>>,
    calls: Mutex<HashMap<Uuid, VoiceCall>>,
    /// By communication id: what the outreach request carried, until its call starts.
    requests: Mutex<IndexMap<Uuid, RequestExtras>>,
    /// By call id: the approved purposes and their state, until the call ends.
    extras: Mutex<HashMap<Uuid, CallExtras>>,
}

/// At most this many requests wait for their start; the oldest is dropped first.
const MAX_PENDING_REQUESTS: usize = 2048;

fn sha_hex(s: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

impl VoiceCalls {
    pub fn for_settings(settings: &crate::config::Settings) -> Self {
        let registry = BridgeRegistry::new();
        let launcher: Option<Arc<dyn BridgeLauncher>> = if settings.voice_bridge_active() {
            Some(Arc::new(LiveBridgeLauncher {
                registry: registry.clone(),
            }))
        } else {
            None
        };
        Self {
            registry,
            launcher: RwLock::new(launcher),
            app: RwLock::new(Weak::new()),
            calls: Mutex::new(HashMap::new()),
            requests: Mutex::new(IndexMap::new()),
            extras: Mutex::new(HashMap::new()),
        }
    }

    /// Keep what an outreach request carried besides its communication's fields (server side
    /// only). A request with neither questions nor purposes keeps nothing.
    pub fn stash_request(&self, communication_id: Uuid, extras: RequestExtras) {
        if extras.questions.is_empty() && !extras.has_purposes() {
            return;
        }
        let mut m = self.requests.lock().unwrap_or_else(|e| e.into_inner());
        while m.len() >= MAX_PENDING_REQUESTS {
            m.shift_remove_index(0);
        }
        m.insert(communication_id, extras);
    }

    fn request_extras(&self, communication_id: Uuid) -> Option<RequestExtras> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&communication_id)
            .cloned()
    }

    fn forget_request(&self, communication_id: Uuid) {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shift_remove(&communication_id);
    }

    /// Whether an approved tour or rent negotiation of the call is not settled yet.
    fn purposes_open(&self, call_id: Uuid) -> bool {
        self.extras_map().get(&call_id).is_some_and(|e| {
            let tour = e.approved.tour.is_some()
                && e.state.tour.booked.is_none()
                && !e.state.tour.needs_owner;
            let rent = e.approved.negotiation.is_some()
                && e.state.negotiation.with_owner.is_none()
                && e.state.negotiation.declined.is_none();
            tour || rent
        })
    }

    /// Undo the Response Agent's "ended" after a withheld closing, so later lines are decided.
    fn reopen_agent(&self, call_id: Uuid) {
        if let Some(v) = self.map().get_mut(&call_id) {
            v.agent.ended = false;
        }
    }

    fn extras_map(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, CallExtras>> {
        self.extras.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_extras(&self, call_id: Uuid, extras: CallExtras) {
        self.extras_map().insert(call_id, extras);
    }

    fn call_extras(&self, call_id: Uuid) -> Option<CallExtras> {
        self.extras_map().get(&call_id).cloned()
    }

    /// Whether a call still holds purposes state (tests: dropped at any terminal state).
    pub fn has_purposes_state(&self, call_id: Uuid) -> bool {
        self.extras_map().contains_key(&call_id)
    }

    /// How many outreach requests wait for their start (tests).
    pub fn pending_requests(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, VoiceCall>> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Install (or remove) the launcher; tests install a fixture bridge here.
    pub fn set_launcher(&self, launcher: Option<Arc<dyn BridgeLauncher>>) {
        *self.launcher.write().unwrap_or_else(|e| e.into_inner()) = launcher;
    }

    fn launcher(&self) -> Option<Arc<dyn BridgeLauncher>> {
        self.launcher
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether new calls take the bridge path.
    pub fn enabled(&self) -> bool {
        self.launcher().is_some()
    }

    /// Remember the shared app, so bridge events (which arrive outside any request) can open
    /// their own transactions.
    pub fn bind(&self, app: &Arc<App>) {
        let mut w = self.app.write().unwrap_or_else(|e| e.into_inner());
        if w.upgrade().is_none() {
            *w = Arc::downgrade(app);
        }
    }

    fn app_weak(&self) -> Weak<App> {
        self.app.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_bridged(&self, call_id: Uuid) -> bool {
        self.map().contains_key(&call_id)
    }

    /// Whether the call has a registered bridge (tests).
    pub fn has_bridge(&self, call_id: Uuid) -> bool {
        self.map()
            .get(&call_id)
            .is_some_and(|v| v.bridge_id.is_some())
    }

    /// Whether the call's bridge is paused (tests).
    pub fn is_paused(&self, call_id: Uuid) -> bool {
        self.map().get(&call_id).is_some_and(|v| v.paused)
    }

    /// The control that speaks for the call now: a bridge exists, is not paused, and the call
    /// is ACTIVE or AI_RESUMED. Otherwise the server speaks on the carrier.
    fn speaking_control(&self, call: &CallSession) -> Option<Arc<dyn BridgeControl>> {
        if !matches!(call.state, CallState::Active | CallState::AiResumed) {
            return None;
        }
        self.map()
            .get(&call.call_id)
            .filter(|v| !v.paused)
            .and_then(|v| v.control.clone())
    }

    async fn pause(&self, call_id: Uuid) {
        let control = {
            let mut m = self.map();
            m.get_mut(&call_id).and_then(|v| {
                v.paused = true;
                v.control.clone()
            })
        };
        if let Some(c) = control {
            c.pause().await;
        }
    }

    async fn unpause(&self, call_id: Uuid) {
        let control = {
            let mut m = self.map();
            m.get_mut(&call_id).and_then(|v| {
                v.paused = false;
                v.control.clone()
            })
        };
        if let Some(c) = control {
            c.resume().await;
        }
    }

    /// Stop and forget the call's bridge; the stream secret dies with it, and so does the
    /// purposes state (the limits). What the summary needs is handed back.
    async fn close(&self, call_id: Uuid) -> ClosedVoice {
        let entry = self.map().remove(&call_id);
        let extras = self.extras_map().remove(&call_id);
        let mut left = ClosedVoice {
            agent: None,
            extras,
        };
        if let Some(v) = entry {
            if let (Some(id), Some(l)) = (v.bridge_id.as_deref(), self.launcher()) {
                l.remove(id);
            }
            if let Some(c) = v.control {
                c.stop().await;
            }
            left.agent = Some((v.agent, v.context));
        }
        left
    }

    /// Drop the bridge without a stop (it ended by itself), keep the call bridged-and-paused so
    /// the server speaks on the carrier.
    fn detach(&self, call_id: Uuid) {
        let id = {
            let mut m = self.map();
            m.get_mut(&call_id).and_then(|v| {
                v.paused = true;
                v.control = None;
                v.bridge_id.take()
            })
        };
        if let (Some(id), Some(l)) = (id, self.launcher()) {
            l.remove(&id);
        }
    }
}

impl App {
    /// The process's voice bridges (built from the settings on first use).
    pub fn voice_calls(&self) -> Arc<VoiceCalls> {
        if let Some(v) = self.extensions.get::<VoiceCalls>() {
            return v;
        }
        let v = Arc::new(VoiceCalls::for_settings(&self.settings));
        self.extensions.insert(v.clone());
        self.extensions.get::<VoiceCalls>().unwrap_or(v)
    }
}

/// `wss://<PUBLIC_BASE_URL host>/api/v1/voice/stream/{bridge_id}?token=<secret>`.
pub fn stream_url(public_base_url: &str, bridge_id: &str, token: &str) -> String {
    let base = public_base_url.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    };
    format!("{base}/api/v1/voice/stream/{bridge_id}?token={token}")
}

/// What a bridge (or the intro watchdog) reports about a call.
#[derive(Debug, Clone)]
pub enum BridgeInput {
    Counterparty(String),
    Assistant(String),
    Tool {
        name: String,
        arguments: Value,
        /// The bridge's turn counter when the model made the call.
        turn_id: u64,
    },
    /// The bridge ended by itself (`EndReason::as_str`).
    Ended(String),
    /// The recording's playback did not end within its bound.
    IntroTimeout,
}

/// Run one bridge input in its own transaction, then send the owner texts it caused.
pub async fn bridge_input(app: &Arc<App>, call_id: Uuid, input: BridgeInput) -> AppResult<String> {
    app.voice_calls().bind(app);
    let mut tx = app.store.begin().await?;
    let mut orch = CallOrchestrator::new(app);
    let out = orch.on_bridge_input(&mut *tx, call_id, input).await?;
    tx.commit().await?;
    app.owner_notify()
        .dispatch(app.clone(), orch.take_notices());
    Ok(out)
}

/// The orchestrator's side of one call's bridge.
pub struct CallBridgeEvents {
    app: Weak<App>,
    call_id: Uuid,
}

impl CallBridgeEvents {
    async fn run(&self, input: BridgeInput) -> Option<String> {
        let app = self.app.upgrade()?;
        match bridge_input(&app, self.call_id, input).await {
            Ok(s) => Some(s),
            Err(_) => {
                // Ids only: an error message is not logged (it could quote call content).
                logging::warning(
                    "app.services.call_orchestration",
                    "bridge event failed",
                    &[("call_id", Value::String(self.call_id.to_string()))],
                );
                None
            }
        }
    }
}

#[async_trait::async_trait]
impl BridgeEvents for CallBridgeEvents {
    async fn transcript(&self, line: TranscriptLine) {
        let input = match line.speaker {
            BridgeSpeaker::Counterparty => BridgeInput::Counterparty(line.text),
            BridgeSpeaker::Assistant => BridgeInput::Assistant(line.text),
        };
        self.run(input).await;
    }
    async fn tool_call(&self, call: ToolCall) -> String {
        self.run(BridgeInput::Tool {
            name: call.name,
            arguments: call.arguments,
            turn_id: call.turn_id,
        })
        .await
        .unwrap_or_else(|| NOT_APPROVED_OUTPUT.to_owned())
    }
    async fn ended(&self, end: BridgeEnd) {
        self.run(BridgeInput::Ended(end.reason.as_str())).await;
    }
}

fn arg_str(v: &Value, k: &str) -> String {
    let raw = v.get(k).and_then(Value::as_str).unwrap_or("");
    let (cut, _) = agent::cut(raw);
    let (red, _, _) = agent::redact(&cut);
    red.chars().take(300).collect()
}

impl CallOrchestrator<'_> {
    /// Carrier events of a bridged call that differ from the fixed-line flow; `None` lets the
    /// normal handling run.
    async fn bridged_event(
        &mut self,
        tx: &mut dyn Tx,
        call: &CallSession,
        et: Option<&str>,
        p: &serde_json::Map<String, Value>,
    ) -> AppResult<Option<CallSession>> {
        match et {
            // Only the ended event of our own recording's playback moves the call on.
            Some("call.playback.ended") if call.state == CallState::Intro => {
                if machine_detected(call) {
                    return Ok(Some(call.clone())); // being hung up; nothing starts
                }
                let expected = self
                    .app
                    .voice_calls()
                    .map()
                    .get(&call.call_id)
                    .and_then(|v| v.intro_url_sha.clone());
                let got = p.get("media_url").and_then(Value::as_str).map(sha_hex);
                if expected.is_none() || got != expected {
                    return Ok(Some(call.clone()));
                }
                // An ended event without a status does not start the assistant: it is treated
                // as a failed playback.
                let status = p
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("no status");
                if status != "completed" {
                    let why =
                        format!("the recorded introduction did not play to its end ({status})");
                    return self.fail_politely(tx, call.clone(), &why).await.map(Some);
                }
                self.begin_bridge(tx, call.clone()).await.map(Some)
            }
            // Nothing else speaks or moves the call during the recording.
            Some("call.speak.ended" | "call.playback.ended") => Ok(Some(call.clone())),
            // The bridge carries the speech: the carrier's transcription is not used.
            Some("call.transcription") => Ok(Some(call.clone())),
            Some("streaming.failed") if !is_terminal(call.state) => {
                self.app.voice_calls().detach(call.call_id);
                self.fail_politely(tx, call.clone(), "the media stream failed")
                    .await
                    .map(Some)
            }
            _ => Ok(None),
        }
    }

    /// `call.answered` with the bridge on: the recording and nothing else.
    async fn answered_bridged(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
    ) -> AppResult<CallSession> {
        let vcs = self.app.voice_calls();
        vcs.map().insert(call.call_id, VoiceCall::new());
        let Some(reference) = call
            .intro_recording_reference
            .clone()
            .filter(|r| !r.is_empty())
        else {
            // No recording (optional): the bridge's opening line is the first thing heard.
            return self.begin_bridge(tx, call).await;
        };
        // A recording revoked or replaced between the approval and the answer is not played;
        // the call goes on without it and the reason is stored on the call.
        tx.advisory_xact_lock(crate::services::design::lock_key(
            "intro-recording",
            call.owner_id,
        ))
        .await?;
        let active = crate::services::intro_recording::active_for_call(tx, call.owner_id).await?;
        let Some(intro) = active.filter(|i| i.object_key == reference) else {
            let mut call = call;
            call.transcript.push(turn(
                "system",
                "[the owner's recorded introduction was revoked or replaced after the approval; \
                 not played]",
            ));
            call.intro_recording_reference = None;
            let call = self.save(tx, call).await?;
            return self.begin_bridge(tx, call).await;
        };
        let Some(leg) = call.provider_call_id.clone() else {
            return self
                .fail_politely(tx, call, "the call has no carrier leg")
                .await;
        };
        let token = crate::security::sign_media_key(
            self.app.settings.auth_secret.expose(),
            &reference,
            900,
        );
        let url = format!(
            "{}/api/v1/media/public/{token}",
            self.app.settings.public_base_url
        );
        let sha = sha_hex(&url);
        match self.tel(move |t| t.play(&leg, &url)).await? {
            Ok(()) => {}
            Err(TelephonyError::Provider(e)) => {
                let why = format!("the recorded introduction could not start ({})", e.kind);
                return self.fail_politely(tx, call, &why).await;
            }
            Err(TelephonyError::Forbidden(m)) => return Err(AppError::forbidden(m)),
        }
        if let Some(v) = vcs.map().get_mut(&call.call_id) {
            v.intro_url_sha = Some(sha);
        }
        let owner = self.owner_name(tx, call.owner_id).await?;
        let mut call = call;
        call.transcript.push(turn(
            "system",
            format!("[playing {owner}'s recorded introduction; waiting for it to end]"),
        ));
        let call = self.save(tx, call).await?;
        // The watchdog: no matching ended event within the recording's length plus the grace.
        let bound = std::time::Duration::from_millis(
            u64::try_from(intro.duration_ms).unwrap_or(60_000)
                + self.app.settings.voice_intro_grace_s * 1000,
        );
        if let Some(app) = vcs.app_weak().upgrade() {
            let call_id = call.call_id;
            tokio::spawn(async move {
                tokio::time::sleep(bound).await;
                let _ = bridge_input(&app, call_id, BridgeInput::IntroTimeout).await;
            });
        }
        Ok(call)
    }

    /// After the recording: ACTIVE, the server's session, the stream secret, `streaming_start`.
    async fn begin_bridge(&mut self, tx: &mut dyn Tx, call: CallSession) -> AppResult<CallSession> {
        let vcs = self.app.voice_calls();
        if vcs.has_bridge(call.call_id) {
            return Ok(call); // one bridge per call
        }
        let (Some(launcher), Some(leg)) = (vcs.launcher(), call.provider_call_id.clone()) else {
            return self
                .fail_politely(tx, call, "the voice bridge is unavailable")
                .await;
        };
        let call = call.transition(CallState::Active)?;
        let mut call = self.save(tx, call).await?;
        let owner = self.owner_name(tx, call.owner_id).await?;
        let extras = vcs.call_extras(call.call_id).unwrap_or_default();
        let built = vsc::build(
            tx,
            &call,
            &owner,
            &self.app.settings.xai_voice,
            &extras.questions,
            &extras.approved,
        )
        .await?;
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let key = self
            .app
            .settings
            .xai_api_key
            .as_ref()
            .map(|k| k.expose().to_owned())
            .unwrap_or_default();
        let spec = CallBridgeSpec {
            call_control_id: leg.clone(),
            token: token.clone(),
            session: built.session.clone(),
            opening_line: Some(built.opening_line.clone()),
            xai: crate::integrations::xai_realtime::XaiConfig::new(
                self.app.settings.xai_realtime_url.clone(),
                key,
            ),
            options: BridgeOptions::default(),
            events: Arc::new(CallBridgeEvents {
                app: vcs.app_weak(),
                call_id: call.call_id,
            }),
        };
        let (bridge_id, control) = launcher.expect(spec);
        let url = stream_url(&self.app.settings.public_base_url, &bridge_id, &token);
        let (first, state) = agent::start(&built.context);
        {
            let mut m = vcs.map();
            let v = m.entry(call.call_id).or_insert_with(VoiceCall::new);
            v.bridge_id = Some(bridge_id);
            v.control = Some(control);
            v.paused = false;
            v.context = Some(built.context.clone());
            v.agent = state;
        }
        let opts = StreamOptions::voice_agent(&url);
        match self.tel(move |t| t.start_streaming(&leg, &opts)).await? {
            Ok(()) => {}
            Err(TelephonyError::Provider(e)) => {
                vcs.detach(call.call_id);
                let why = format!("the media stream could not start ({})", e.kind);
                return self.fail_politely(tx, call, &why).await;
            }
            Err(TelephonyError::Forbidden(m)) => return Err(AppError::forbidden(m)),
        }
        // The bridge speaks this verbatim before the model may say anything.
        call.transcript.push(turn("assistant", built.opening_line));
        let call = self.save(tx, call).await?;
        self.apply_say(tx, call, &first).await
    }

    /// The Response Agent's `say` and `end_call`.
    async fn apply_say(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        d: &agent::Decision,
    ) -> AppResult<CallSession> {
        // The Response Agent closes when its questions are done. While an approved tour or
        // rent negotiation is still open, that closing is withheld (nothing said, no hangup)
        // and the agent keeps deciding; it closes on a later line, once they are settled.
        if d.end_call && self.app.voice_calls().purposes_open(call.call_id) {
            self.app.voice_calls().reopen_agent(call.call_id);
            return Ok(call);
        }
        let call = match &d.say {
            Say::Exact(t) => self.say(tx, call, t).await?,
            Say::Instruct(t) => self.instruct(tx, call, t, d).await?,
            Say::Silent => call,
        };
        if d.end_call {
            if let Some(leg) = call.provider_call_id.clone() {
                let t = self.telephony.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(CLOSING_HANGUP_DELAY_S))
                        .await;
                    let _ = blocking(move || t.hangup(&leg)).await;
                });
            }
        }
        Ok(call)
    }

    /// The Response Agent's silent direction for the model's next turns (`Say::Instruct`): sent
    /// to the call's bridge, never spoken by the server and never stored as a spoken line. A
    /// call without a bridge sends nothing and changes nothing. A paused bridge keeps the latest
    /// direction until it resumes. A refusal (the bridge is gone) is noted on the transcript and
    /// the call goes on as it was.
    async fn instruct(
        &self,
        tx: &mut dyn Tx,
        call: CallSession,
        text: &str,
        d: &agent::Decision,
    ) -> AppResult<CallSession> {
        let (control, ctx) = {
            let vcs = self.app.voice_calls();
            let m = vcs.map();
            match m.get(&call.call_id) {
                Some(v) => (v.control.clone(), v.context.clone()),
                None => (None, None),
            }
        };
        let Some(control) = control else {
            return Ok(call);
        };
        let note = match direction_text(text, d, ctx.as_ref()) {
            Direction::Send(text) => {
                if control.instruct(text).await {
                    return Ok(call);
                }
                DIRECTION_REFUSED_NOTE
            }
            Direction::Nothing => return Ok(call),
            Direction::Withheld => DIRECTION_WITHHELD_NOTE,
        };
        let mut call = call;
        call.transcript.push(turn("system", note));
        self.save(tx, call).await
    }

    /// The fixed-line end: the bridge (if any) is let go, the reason stored, the apology spoken
    /// on the carrier, the call hung up.
    async fn fail_politely(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        why: &str,
    ) -> AppResult<CallSession> {
        let vcs = self.app.voice_calls();
        vcs.detach(call.call_id);
        let mut call = call;
        call.transcript
            .push(turn("system", format!("[{why}; ending the call]")));
        let call = self.save(tx, call).await?;
        let call = self.say(tx, call, APOLOGY_LINE).await?;
        if let Some(leg) = call.provider_call_id.clone() {
            let _ = self.tel(move |t| t.hangup(&leg)).await?;
        }
        Ok(call)
    }

    /// After a handoff: the Response Agent re-asks the open question, the next one, or closes.
    async fn agent_resume(&mut self, tx: &mut dyn Tx, call: CallSession) -> AppResult<CallSession> {
        let decision = {
            let vcs = self.app.voice_calls();
            let mut m = vcs.map();
            m.get_mut(&call.call_id).and_then(|v| {
                let ctx = v.context.clone()?;
                let turn_id = v.next_turn;
                v.next_turn += 1;
                let (d, st) = agent::resume(&ctx, &v.agent, turn_id);
                v.agent = st;
                Some(d)
            })
        };
        match decision {
            Some(d) => self.apply_say(tx, call, &d).await,
            None => Ok(call),
        }
    }

    /// One bridge input for the call, inside `tx`.
    pub async fn on_bridge_input(
        &mut self,
        tx: &mut dyn Tx,
        call_id: Uuid,
        input: BridgeInput,
    ) -> AppResult<String> {
        let Some(call) = CALLS.get(tx, call_id).await? else {
            return Ok(NOT_APPROVED_OUTPUT.to_owned());
        };
        lock_project(tx, call.project_id).await?;
        let mut call = CALLS.require(tx, call_id).await?;
        let vcs = self.app.voice_calls();
        if is_terminal(call.state) || !vcs.is_bridged(call_id) {
            return Ok(match input {
                // After the call ended the purposes are gone: the module's own refusal.
                BridgeInput::Tool {
                    name, arguments, ..
                } if is_purpose_tool(&name) => purpose_refusal(&name, &arguments),
                BridgeInput::Tool { .. } => NOT_APPROVED_OUTPUT.to_owned(),
                _ => String::new(),
            });
        }
        match input {
            BridgeInput::Assistant(text) => {
                let (cut, _) = agent::cut(&text);
                let (red, _, _) = agent::redact(&cut);
                // The opening line and our `say` lines are already in the transcript.
                let already = call
                    .transcript
                    .iter()
                    .rev()
                    .take(6)
                    .any(|t| t.speaker == "assistant" && t.text == red);
                if !already && !red.trim().is_empty() {
                    call.transcript.push(turn("assistant", red));
                    self.save(tx, call).await?;
                }
                Ok(String::new())
            }
            BridgeInput::Counterparty(text) => {
                self.on_bridge_utterance(tx, call, &text).await?;
                Ok(String::new())
            }
            BridgeInput::Tool {
                name,
                arguments,
                turn_id,
            } => {
                self.on_bridge_tool(tx, call, &name, &arguments, turn_id)
                    .await
            }
            BridgeInput::Ended(reason) => {
                vcs.detach(call_id);
                let why = format!("the voice assistant ended ({reason})");
                self.fail_politely(tx, call, &why).await?;
                Ok(String::new())
            }
            BridgeInput::IntroTimeout => {
                let pending = vcs
                    .map()
                    .get(&call_id)
                    .is_some_and(|v| v.intro_url_sha.is_some() && v.bridge_id.is_none());
                if call.state == CallState::Intro && pending && !machine_detected(&call) {
                    self.fail_politely(
                        tx,
                        call,
                        "the recorded introduction did not end within its bound",
                    )
                    .await?;
                }
                Ok(String::new())
            }
        }
    }

    /// A finished counterparty line: the Response Agent decides; the functions above act.
    async fn on_bridge_utterance(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        text: &str,
    ) -> AppResult<CallSession> {
        let step = {
            let vcs = self.app.voice_calls();
            let mut m = vcs.map();
            m.get_mut(&call.call_id).and_then(|v| {
                let ctx = v.context.clone()?;
                let turn_id = v.next_turn;
                v.next_turn += 1;
                let utt = Utterance {
                    turn_id,
                    speaker: agent::Speaker::Counterparty,
                    text: text.to_owned(),
                };
                let (d, st) = agent::step(&ctx, &v.agent, &utt);
                v.agent = st;
                Some((d, ctx))
            })
        };
        let mut call = call;
        let Some((d, ctx)) = step else {
            // No decision state (the bridge never started): keep only the redacted line.
            let (cut, _) = agent::cut(text);
            call.transcript
                .push(turn("counterparty", agent::redact(&cut).0));
            return self.save(tx, call).await;
        };
        // Only the cut and redacted text is stored.
        call.transcript
            .push(turn("counterparty", d.stored_text.clone()));
        let mut call = self.save(tx, call).await?;
        if !matches!(call.state, CallState::Active | CallState::AiResumed) {
            return Ok(call); // the owner is (being) connected; nothing is decided or said
        }
        for f in &d.findings {
            let question = f
                .question_id
                .as_ref()
                .and_then(|id| ctx.questions.iter().find(|q| q.id == *id))
                .map_or_else(|| f.topic.clone(), |q| q.text.clone());
            let sf = f.to_structured_finding(&question);
            call.findings.push(sf.clone());
            call.question_index += 1;
            call = self.save(tx, call).await?;
            self.record_finding(tx, &call, f.requirement_id.as_deref(), &sf)
                .await?;
        }
        if let Some(fu) = &d.owner_followup {
            call.deferred_questions.push(fu.clone());
        }
        // While a rent negotiation is approved, `evaluate_rent` answers a counter-offer or a
        // concession asked ("would the owner go to ..."): the Response Agent's "I can't
        // negotiate terms myself" line is not spoken over the tool's answer, whichever of the
        // two arrives first. A proposal the tool has not answered is kept and reaches the owner
        // as a follow-up if the tool call never comes (by the next counterparty line, or at the
        // end). A commitment ("do we have a deal at ...?") is a hold and is not touched here.
        let mut d = d;
        let negotiating = {
            let vcs = self.app.voice_calls();
            let mut m = vcs.extras_map();
            match m
                .get_mut(&call.call_id)
                .filter(|e| e.approved.negotiation.is_some())
            {
                Some(e) => {
                    let answered = std::mem::take(&mut e.rent_tool_since_line);
                    if let Some(q) = e.pending_concession.take() {
                        call.deferred_questions.push(q);
                    }
                    let concession = d
                        .escalation
                        .as_ref()
                        .filter(|x| x.rule == EscalationRule::Concession && !x.hold)
                        .map(|x| x.question_for_owner.clone());
                    if let Some(q) = &concession {
                        if !answered {
                            e.pending_concession = Some(q.clone());
                        }
                    }
                    concession.is_some()
                }
                None => false,
            }
        };
        if negotiating {
            d.say = Say::Silent;
            if std::mem::take(&mut d.end_call) {
                self.app.voice_calls().reopen_agent(call.call_id);
            }
        } else if let Some(esc) = d.escalation.as_ref().filter(|e| !e.hold) {
            call.deferred_questions.push(esc.question_for_owner.clone());
        }
        let call = self.save(tx, call).await?;
        if let Some(esc) = d.escalation.as_ref().filter(|e| e.hold) {
            // Pauses the bridge; the hold line then goes out on the carrier.
            self.request_handoff(tx, call.call_id, esc.reason, &esc.question_for_owner)
                .await?;
            let call = CALLS.require(tx, call.call_id).await?;
            return self.apply_say(tx, call, &d).await;
        }
        self.apply_say(tx, call, &d).await
    }

    /// A tool call. The tour and rent tools are answered by the server's rules from what the
    /// owner approved for this call; commitments and sensitive disclosures are never approved
    /// by the model.
    async fn on_bridge_tool(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        name: &str,
        args: &Value,
        turn_id: u64,
    ) -> AppResult<String> {
        if is_purpose_tool(name) {
            return self.on_purpose_tool(tx, call, name, args, turn_id).await;
        }
        let client = self.owner_name(tx, call.owner_id).await?;
        // The owner is asked in the server's words: a fixed question per kind, with the
        // Response Agent's own question for the latest counterparty line when it raised one.
        // The model's text is kept only as a redacted transcript note.
        let (reason, fixed, note, output) = match name {
            vsc::TOOL_APPROVAL => {
                let kind = arg_str(args, "kind");
                let (reason, fixed) = match kind.as_str() {
                    "commitment" => (
                        HandoffReason::BindingCommitment,
                        "The listing contact is asking for a commitment on the call. Nothing \
                         was agreed. Can you join?",
                    ),
                    "sensitive_disclosure" => (
                        HandoffReason::SensitivePersonalInfo,
                        "The listing contact is asking for personal or financial information \
                         (not recorded). Can you speak with them directly?",
                    ),
                    _ => (
                        HandoffReason::ExceedsAuthority,
                        "The listing contact is asking for something the assistant may not \
                         decide. Can you join?",
                    ),
                };
                (
                    reason,
                    fixed,
                    arg_str(args, "detail"),
                    not_approved_output(&client),
                )
            }
            vsc::TOOL_ESCALATE => (
                HandoffReason::CounterpartyRequestedOwner,
                "The listing contact asked something only you can answer. Can you join?",
                arg_str(args, "question"),
                owner_asked_output(&client),
            ),
            _ => return Ok(UNKNOWN_TOOL_OUTPUT.to_owned()),
        };
        let latest = self
            .app
            .voice_calls()
            .map()
            .get(&call.call_id)
            .and_then(|v| v.agent.decisions.last())
            .and_then(|d| d.escalation.as_ref())
            .map(|e| e.question_for_owner.clone())
            .filter(|q| !q.trim().is_empty());
        let question = match latest {
            Some(q) => format!("{fixed} Their last line: {}", vsc::field(&q)),
            None => fixed.to_owned(),
        };
        // No server record approves a commitment or a disclosure on a call (no approval payload
        // names one), so the answer is always "not approved"; the owner is asked.
        let mut call = call;
        call.transcript.push(turn(
            "system",
            format!(
                "[assistant tool {name}: {}; the model's note: {note}]",
                reason.as_str()
            ),
        ));
        let call = self.save(tx, call).await?;
        if matches!(call.state, CallState::Active | CallState::AiResumed) {
            self.request_handoff(tx, call.call_id, reason, &question)
                .await?;
        }
        Ok(output)
    }

    /// `check_tour_time` / `evaluate_rent`: the module decides from the stored approval and
    /// state; the new state is stored; the model gets the module's JSON and nothing else. Not
    /// approved, unreadable arguments, a call that is not the assistant's to lead right now:
    /// the module's refusal.
    async fn on_purpose_tool(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        name: &str,
        args: &Value,
        turn_id: u64,
    ) -> AppResult<String> {
        if !matches!(call.state, CallState::Active | CallState::AiResumed) {
            return Ok(purpose_refusal(name, args));
        }
        let vcs = self.app.voice_calls();
        let now = Utc::now().fixed_offset();
        let step = {
            let mut m = vcs.extras_map();
            m.get_mut(&call.call_id).map(|e| {
                // REVIEW FIX (voice-glue-1-review R1): the same call again in the same turn (a
                // duplicated call id, the model's two identical calls in one response) gets the
                // first answer; the state, the transcript and the owner's notices do not move.
                if let Some((_, _, _, out)) = e
                    .last_tool
                    .as_ref()
                    .filter(|(t, n, a, _)| *t == turn_id && n == name && a == args)
                {
                    return Err(out.clone());
                }
                let before = e.state.clone();
                let (out, after) = cp::dispatch(&e.approved, &before, name, args, now);
                e.state = after.clone();
                e.last_tool = Some((turn_id, name.to_owned(), args.clone(), out.to_string()));
                if name == cp::TOOL_EVALUATE_RENT && e.approved.negotiation.is_some() {
                    e.pending_concession = None;
                    e.rent_tool_since_line = true;
                }
                Ok((out, before, after))
            })
        };
        let (out, before, after) = match step {
            None => return Ok(purpose_refusal(name, args)),
            Some(Err(repeated)) => return Ok(repeated),
            Some(Ok(step)) => step,
        };
        let decision = out
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let mut call = call;
        // The decision's name only: the arguments and the line are not stored here (the line
        // the assistant speaks arrives as its own transcript turn).
        call.transcript.push(turn(
            "system",
            format!("[assistant tool {name}: {decision}]"),
        ));
        let needs_owner = !before.tour.needs_owner && after.tour.needs_owner;
        if needs_owner {
            call.deferred_questions.push(
                "Tour: they could not make any of your times and are waiting for other times \
                 from you."
                    .to_owned(),
            );
        }
        let call = self.save(tx, call).await?;
        self.notify_outcomes(&call, &before, &after);
        Ok(out.to_string())
    }
}

/// Whether an answering machine was detected on this call (the stored reason).
fn machine_detected(call: &CallSession) -> bool {
    call.transcript
        .iter()
        .any(|t| t.speaker == "system" && t.text == VOICEMAIL_NOTE)
}

fn is_purpose_tool(name: &str) -> bool {
    name == cp::TOOL_CHECK_TOUR || name == cp::TOOL_EVALUATE_RENT
}

/// The module's own answer when nothing is approved for the call (or the call is over).
fn purpose_refusal(name: &str, args: &Value) -> String {
    cp::dispatch(
        &Approved::default(),
        &PurposeState::default(),
        name,
        args,
        Utc::now().fixed_offset(),
    )
    .0
    .to_string()
}
