//! Port of `apps/api/app/contracts/communications.py`: channels, communication sessions, calls and
//! their state machine, owner handoffs and their state machine, outreach requests, and the
//! owner's Photon channel (the Python Photon code owns that table; Rust only reads it).
//!
//! Field order, defaults and enum strings are Python's. Invalid transitions return the shared
//! [`InvalidStateTransition`] error with Python's message (`call: transition A -> B is not
//! allowed`). As in Python, `transition` returns a copy with the new state and does not
//! re-validate the rest of the model.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ss_core::{
    check_transition, new_id, string_enum, utcnow, Checker, Contract, InvalidStateTransition,
    TransitionTable, UtcDateTime, Validate,
};
use uuid::Uuid;

string_enum! {
    pub enum Channel {
        PhotonImessage = "PHOTON_IMESSAGE",
        PhotonSms = "PHOTON_SMS",
        Email = "EMAIL",
        Voice = "VOICE",
        Web = "WEB",
    }
}

string_enum! {
    #[derive(Default)]
    pub enum CommunicationState {
        #[default]
        Draft = "DRAFT",
        AwaitingApproval = "AWAITING_APPROVAL",
        Approved = "APPROVED",
        InProgress = "IN_PROGRESS",
        Completed = "COMPLETED",
        Failed = "FAILED",
        Cancelled = "CANCELLED",
    }
}

fn owner_reported() -> String {
    "OWNER_REPORTED".to_owned()
}

/// One answer from a call or message, as the counterparty stated it. `classification` defaults
/// to `OWNER_REPORTED`: what a broker says is owner- or broker-reported evidence, never verified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredFinding {
    pub question: String,
    #[serde(default)]
    pub answer: Option<String>,
    /// e.g. `'electrical_service_amps'`.
    #[serde(default)]
    pub claim_type: Option<String>,
    /// Python `Any`: an int, a bool, a string, or null (unclear).
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub speaker: Option<String>,
    #[serde(default)]
    pub transcript_offset_s: Option<f64>,
    #[serde(default = "owner_reported")]
    pub classification: String,
}

impl Validate for StructuredFinding {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for StructuredFinding {
    const NAME: &'static str = "StructuredFinding";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunicationSession {
    #[serde(default = "new_id")]
    pub communication_id: Uuid,
    pub project_id: Uuid,
    #[serde(default)]
    pub property_id: Option<Uuid>,
    pub channel: Channel,
    /// `[{'role':'broker','name':'...','handle':'+1...'}]`.
    #[serde(default)]
    pub external_participants: Vec<IndexMap<String, String>>,
    pub authorized_owner_id: Uuid,
    pub purpose: String,
    #[serde(default)]
    pub state: CommunicationState,
    #[serde(default)]
    pub recording_reference: Option<String>,
    #[serde(default)]
    pub transcript_reference: Option<String>,
    #[serde(default)]
    pub structured_findings: Vec<StructuredFinding>,
    #[serde(default)]
    pub approval_id: Option<Uuid>,
    #[serde(default)]
    pub provider_message_ids: Vec<String>,
    #[serde(default = "utcnow")]
    pub created_at: UtcDateTime,
}

impl Validate for CommunicationSession {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for CommunicationSession {
    const NAME: &'static str = "CommunicationSession";
}

string_enum! {
    #[derive(Default)]
    pub enum CallState {
        #[default]
        Created = "CREATED",
        Dialing = "DIALING",
        Intro = "INTRO",
        Active = "ACTIVE",
        HandoffPending = "HANDOFF_PENDING",
        OwnerJoining = "OWNER_JOINING",
        HumanJoined = "HUMAN_JOINED",
        AiResumed = "AI_RESUMED",
        Completed = "COMPLETED",
        Failed = "FAILED",
        Declined = "DECLINED",
        Cancelled = "CANCELLED",
    }
}

/// `CALL_TRANSITIONS`. `COMPLETED`, `FAILED`, `DECLINED` and `CANCELLED` are terminal.
pub const CALL_TRANSITIONS: &TransitionTable<CallState> = &[
    (
        CallState::Created,
        &[CallState::Dialing, CallState::Cancelled, CallState::Failed],
    ),
    (
        CallState::Dialing,
        &[
            CallState::Intro,
            CallState::Declined,
            CallState::Failed,
            CallState::Cancelled,
        ],
    ),
    (
        CallState::Intro,
        &[CallState::Active, CallState::Failed, CallState::Completed],
    ),
    (
        CallState::Active,
        &[
            CallState::HandoffPending,
            CallState::Completed,
            CallState::Failed,
        ],
    ),
    (
        CallState::HandoffPending,
        &[
            CallState::OwnerJoining,
            CallState::AiResumed,
            CallState::Completed,
            CallState::Failed,
        ],
    ),
    (
        CallState::OwnerJoining,
        &[
            CallState::HumanJoined,
            CallState::AiResumed,
            CallState::Failed,
        ],
    ),
    (
        CallState::HumanJoined,
        &[
            CallState::AiResumed,
            CallState::Completed,
            CallState::Failed,
        ],
    ),
    (
        CallState::AiResumed,
        &[
            CallState::HandoffPending,
            CallState::Completed,
            CallState::Failed,
        ],
    ),
    (CallState::Completed, &[]),
    (CallState::Failed, &[]),
    (CallState::Declined, &[]),
    (CallState::Cancelled, &[]),
];

fn default_may_ask_about() -> Vec<String> {
    [
        "availability",
        "infrastructure",
        "permitted_use",
        "showing_times",
    ]
    .map(String::from)
    .to_vec()
}

fn default_may_not() -> Vec<String> {
    [
        "agree_to_terms",
        "discuss_deposits_or_payment",
        "offer_guarantor",
        "share_sensitive_personal_info",
    ]
    .map(String::from)
    .to_vec()
}

/// What the assistant may discuss without the owner. Everything else escalates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityScope {
    #[serde(default = "default_may_ask_about")]
    pub may_ask_about: Vec<String>,
    #[serde(default = "default_may_not")]
    pub may_not: Vec<String>,
}

impl Default for AuthorityScope {
    fn default() -> Self {
        Self {
            may_ask_about: default_may_ask_about(),
            may_not: default_may_not(),
        }
    }
}

impl Validate for AuthorityScope {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for AuthorityScope {
    const NAME: &'static str = "AuthorityScope";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantState {
    /// `'assistant' | 'counterparty' | 'owner'`.
    pub role: String,
    #[serde(default)]
    pub leg_id: Option<String>,
    /// `'dialing' | 'joined' | 'left' | 'failed'`.
    pub state: String,
    #[serde(default = "utcnow")]
    pub updated_at: UtcDateTime,
}

impl Validate for ParticipantState {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for ParticipantState {
    const NAME: &'static str = "ParticipantState";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptTurn {
    /// `'assistant' | 'counterparty' | 'owner' | 'system'`.
    pub speaker: String,
    pub text: String,
    #[serde(default = "utcnow")]
    pub at: UtcDateTime,
}

impl Validate for TranscriptTurn {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for TranscriptTurn {
    const NAME: &'static str = "TranscriptTurn";
}

/// One question the assistant may ask, grounded in an UNKNOWN / MAY_REQUIRE_WORK assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallQuestion {
    pub question: String,
    /// Evidence observation_type an answer would produce, e.g. `'tenant_subpanel_amps'`.
    pub claim_type: String,
    #[serde(default)]
    pub requirement_id: Option<String>,
}

impl Validate for CallQuestion {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for CallQuestion {
    const NAME: &'static str = "CallQuestion";
}

string_enum! {
    #[derive(Default)]
    pub enum OwnerChannelStatus {
        /// The owner registered a handle but has not texted the agent yet.
        #[default]
        PendingFirstMessage = "PENDING_FIRST_MESSAGE",
        Linked = "LINKED",
    }
}

fn imessage() -> String {
    "imessage".to_owned()
}

/// How the assistant reaches the owner on Photon. A shared Photon line cannot start a
/// conversation, so the channel only becomes LINKED after the owner texts the agent; the reply ids
/// come from that inbound message. The Python Photon code owns this table; Rust only reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerChannel {
    #[serde(default = "new_id")]
    pub owner_channel_id: Uuid,
    pub owner_id: Uuid,
    /// Photon platform, e.g. `'imessage'`, `'sms'`.
    #[serde(default = "imessage")]
    pub platform: String,
    /// Owner's phone (E.164) or iMessage email.
    pub handle: String,
    #[serde(default)]
    pub status: OwnerChannelStatus,
    /// Photon Space id of the owner<->agent conversation.
    #[serde(default)]
    pub space_id: Option<String>,
    /// Photon User id of the agent (`receivedBy.id`).
    #[serde(default)]
    pub agent_user_id: Option<String>,
    #[serde(default)]
    pub last_inbound_message_id: Option<String>,
    #[serde(default)]
    pub linked_at: Option<UtcDateTime>,
    #[serde(default = "utcnow")]
    pub updated_at: UtcDateTime,
}

impl Validate for OwnerChannel {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for OwnerChannel {
    const NAME: &'static str = "OwnerChannel";
}

/// `CallSession.automation_disclosure`'s default.
pub const AUTOMATION_DISCLOSURE: &str =
    "This call is being handled by an automated assistant on the owner's behalf.";

fn automation_disclosure() -> String {
    AUTOMATION_DISCLOSURE.to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallSession {
    #[serde(default = "new_id")]
    pub call_id: Uuid,
    pub project_id: Uuid,
    #[serde(default)]
    pub property_id: Option<Uuid>,
    #[serde(default)]
    pub provider_call_id: Option<String>,
    #[serde(default)]
    pub conference_id: Option<String>,
    pub owner_id: Uuid,
    /// `{'role':'broker','name':'...','phone':'+1...'}`.
    pub recipient: IndexMap<String, String>,
    /// Owner's genuine recorded intro (object storage key). Never synthesized.
    #[serde(default)]
    pub intro_recording_reference: Option<String>,
    /// Disclosure played/stated per applicable law and provider rules.
    #[serde(default = "automation_disclosure")]
    pub automation_disclosure: String,
    #[serde(default)]
    pub authority_scope: AuthorityScope,
    #[serde(default)]
    pub state: CallState,
    pub approval_id: Uuid,
    #[serde(default)]
    pub handoff_request_id: Option<Uuid>,
    #[serde(default)]
    pub participant_states: Vec<ParticipantState>,
    #[serde(default)]
    pub started_at: Option<UtcDateTime>,
    #[serde(default)]
    pub ended_at: Option<UtcDateTime>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub communication_id: Option<Uuid>,
    #[serde(default)]
    pub script: Vec<CallQuestion>,
    #[serde(default)]
    pub question_index: i64,
    #[serde(default)]
    pub transcript: Vec<TranscriptTurn>,
    #[serde(default)]
    pub findings: Vec<StructuredFinding>,
    /// Sensitive questions the assistant declined to answer for the owner.
    #[serde(default)]
    pub deferred_questions: Vec<String>,
    /// True when driven by the call simulator, not a carrier.
    #[serde(default)]
    pub simulated: bool,
    #[serde(default)]
    pub outcome_summary: Option<String>,
}

impl CallSession {
    /// A new call with Python's defaults for every optional field.
    pub fn new(
        project_id: Uuid,
        owner_id: Uuid,
        recipient: IndexMap<String, String>,
        approval_id: Uuid,
    ) -> Self {
        Self {
            call_id: new_id(),
            project_id,
            property_id: None,
            provider_call_id: None,
            conference_id: None,
            owner_id,
            recipient,
            intro_recording_reference: None,
            automation_disclosure: automation_disclosure(),
            authority_scope: AuthorityScope::default(),
            state: CallState::Created,
            approval_id,
            handoff_request_id: None,
            participant_states: Vec::new(),
            started_at: None,
            ended_at: None,
            failure_reason: None,
            communication_id: None,
            script: Vec::new(),
            question_index: 0,
            transcript: Vec::new(),
            findings: Vec::new(),
            deferred_questions: Vec::new(),
            simulated: false,
            outcome_summary: None,
        }
    }

    /// `CallSession.transition(target)`: a copy in state `target`, or
    /// [`InvalidStateTransition`] when [`CALL_TRANSITIONS`] does not allow it.
    pub fn transition(&self, target: CallState) -> Result<CallSession, InvalidStateTransition> {
        check_transition("call", CALL_TRANSITIONS, self.state, target)?;
        let mut next = self.clone();
        next.state = target;
        Ok(next)
    }
}

impl Validate for CallSession {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for CallSession {
    const NAME: &'static str = "CallSession";
}

string_enum! {
    pub enum HandoffReason {
        BindingCommitment = "BINDING_COMMITMENT",
        FinancialDetails = "FINANCIAL_DETAILS",
        GuarantorRequested = "GUARANTOR_REQUESTED",
        SensitivePersonalInfo = "SENSITIVE_PERSONAL_INFO",
        CounterpartyRequestedOwner = "COUNTERPARTY_REQUESTED_OWNER",
        InsufficientEvidence = "INSUFFICIENT_EVIDENCE",
        ExceedsAuthority = "EXCEEDS_AUTHORITY",
    }
}

string_enum! {
    #[derive(Default)]
    pub enum HandoffStatus {
        #[default]
        Pending = "PENDING",
        Notified = "NOTIFIED",
        Accepted = "ACCEPTED",
        Declined = "DECLINED",
        Deferred = "DEFERRED",
        Expired = "EXPIRED",
    }
}

/// `HANDOFF_TRANSITIONS`. `ACCEPTED`, `DECLINED`, `DEFERRED` and `EXPIRED` are terminal.
pub const HANDOFF_TRANSITIONS: &TransitionTable<HandoffStatus> = &[
    (
        HandoffStatus::Pending,
        &[HandoffStatus::Notified, HandoffStatus::Expired],
    ),
    (
        HandoffStatus::Notified,
        &[
            HandoffStatus::Accepted,
            HandoffStatus::Declined,
            HandoffStatus::Deferred,
            HandoffStatus::Expired,
        ],
    ),
    (HandoffStatus::Accepted, &[]),
    (HandoffStatus::Declined, &[]),
    (HandoffStatus::Deferred, &[]),
    (HandoffStatus::Expired, &[]),
];

fn high() -> String {
    "HIGH".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerHandoffRequest {
    #[serde(default = "new_id")]
    pub handoff_request_id: Uuid,
    pub call_id: Uuid,
    pub project_id: Uuid,
    pub reason: HandoffReason,
    pub question: String,
    /// `'HIGH' | 'NORMAL'`.
    #[serde(default = "high")]
    pub urgency: String,
    pub owner_id: Uuid,
    /// Opaque reference to a short-lived join token held server-side. Never the raw token.
    #[serde(default)]
    pub join_token_reference: Option<String>,
    pub expires_at: UtcDateTime,
    #[serde(default)]
    pub status: HandoffStatus,
}

impl OwnerHandoffRequest {
    /// `OwnerHandoffRequest.transition(target)`: a copy with status `target`, or
    /// [`InvalidStateTransition`] when [`HANDOFF_TRANSITIONS`] does not allow it.
    pub fn transition(
        &self,
        target: HandoffStatus,
    ) -> Result<OwnerHandoffRequest, InvalidStateTransition> {
        check_transition("handoff", HANDOFF_TRANSITIONS, self.status, target)?;
        let mut next = self.clone();
        next.status = target;
        Ok(next)
    }
}

impl Validate for OwnerHandoffRequest {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for OwnerHandoffRequest {
    const NAME: &'static str = "OwnerHandoffRequest";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachRequest {
    pub property_id: Uuid,
    pub channel: Channel,
    pub recipient: IndexMap<String, String>,
    pub purpose: String,
    #[serde(default)]
    pub questions: Vec<String>,
    #[serde(default)]
    pub intro_recording_reference: Option<String>,
}

impl Validate for OutreachRequest {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for OutreachRequest {
    const NAME: &'static str = "OutreachRequest";
}
