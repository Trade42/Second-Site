//! The post-call summary (owner's specification 2026-11-15, section 6): what a finished call
//! established, who said it, and what is still open.
//!
//! Pure and deterministic: no database, no HTTP, no model call, no clock. [`summarize`] takes a
//! [`SummaryInput`] (built from a `CallSession` with [`SummaryInput::from_call`]) and returns a
//! [`CallSummary`]; [`owner_text`] renders the short text the owner receives.
//!
//! Truth rules:
//! - Every item carries `said_by` and `kind`. What the counterparty said is
//!   `reported_by_counterparty`; what we derived is `inferred_by_us` with `derived_from`;
//!   `verified` holds only what an independent evidence class backs, and a call alone backs
//!   nothing, so it is always empty here and the summary says so.
//! - A question with no clear answer is `not_answered`, never a guess; an empty call says nothing
//!   was learned.
//! - The assistant cannot commit the owner: `commitments_made` is always empty. A transcript line
//!   that looks like a commitment is listed under `needs_owner_attention`.
//! - Every free text from the call passes through [`redact`] (SSN, card and account shapes and
//!   anything after a sensitive word).

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ss_core::UtcDateTime;
use uuid::Uuid;

use crate::communications::response_agent::{
    format_money, redact, topic_of_claim, topic_of_question, AgentFinding, MoneyPeriod,
    COMMITMENT_PATTERN,
};
use crate::contracts::communications::{
    CallSession, HandoffStatus, OwnerHandoffRequest, StructuredFinding, TranscriptTurn,
};
use crate::py::PyRegex;

/// The longest owner text, in characters (the messaging channel carries text only).
pub const MAX_OWNER_TEXT_CHARS: usize = 640;
/// The same text as `ss_server::services::call_orchestration::SIMULATED_LABEL`.
pub const SIMULATED_LABEL: &str = "[Simulated call] ";
/// What the summary says about verification after a call.
pub const NOTHING_VERIFIED: &str =
    "Nothing from this call is independently verified; every fact below is as reported.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallEnd {
    Completed,
    NoAnswer,
    Voicemail,
    HungUp,
    Failed,
    HandedToOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaidBy {
    Counterparty,
    Assistant,
    Owner,
    /// Second Site's own records or derivations.
    SecondSite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    ReportedByCounterparty,
    Verified,
    InferredByUs,
    /// A record Second Site holds (a handoff request and its status).
    SystemRecord,
    /// A transcript line quoted as said, not taken as fact.
    TranscriptQuote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostKind {
    Rent,
    Utilities,
    Deposit,
    LeaseTerm,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Monthly,
    Annual,
    PerSqft,
    OneTime,
    /// Not stated.
    Unknown,
}

/// One finding, in the shape both today's `StructuredFinding` and the Response Agent's
/// `AgentFinding` convert to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputFinding {
    pub question: Option<String>,
    pub claim_type: String,
    pub topic: String,
    /// The answer in the counterparty's words.
    pub reported: Option<String>,
    pub value: Value,
    pub unit: Option<String>,
    pub amount_cents: Option<i64>,
    pub period: Option<MoneyPeriod>,
    pub purpose: Option<String>,
}

fn purpose_of(f: &InputFinding) -> String {
    f.purpose
        .as_deref()
        .map(clean)
        .unwrap_or_else(|| "work".into())
}

impl From<&StructuredFinding> for InputFinding {
    fn from(f: &StructuredFinding) -> Self {
        let claim = f.claim_type.clone().unwrap_or_default();
        let topic = if claim.is_empty() {
            topic_of_question(&f.question)
        } else {
            topic_of_claim(&claim)
        };
        InputFinding {
            question: Some(f.question.clone()),
            claim_type: claim,
            topic: topic.to_owned(),
            reported: f.answer.clone(),
            value: f.value.clone(),
            unit: f.unit.clone(),
            amount_cents: None,
            period: None,
            purpose: None,
        }
    }
}

impl From<&AgentFinding> for InputFinding {
    fn from(f: &AgentFinding) -> Self {
        InputFinding {
            question: None,
            claim_type: f.claim_type.clone(),
            topic: f.topic.clone(),
            reported: Some(f.reported.clone()),
            value: f.value.clone(),
            unit: f.unit.clone(),
            amount_cents: f.amount_cents,
            period: f.period,
            purpose: f.purpose.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutreachQuestion {
    pub id: String,
    pub text: String,
    pub topic: String,
    pub claim_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryInput {
    pub call_id: Uuid,
    pub property_id: Option<Uuid>,
    pub property_label: Option<String>,
    pub contact_name: Option<String>,
    pub contact_role: Option<String>,
    pub started_at: Option<UtcDateTime>,
    pub ended_at: Option<UtcDateTime>,
    pub ended: CallEnd,
    pub questions: Vec<OutreachQuestion>,
    pub transcript: Vec<TranscriptTurn>,
    pub findings: Vec<InputFinding>,
    /// Questions the counterparty asked that only the owner can answer.
    pub deferred_questions: Vec<String>,
    pub handoffs: Vec<OwnerHandoffRequest>,
    pub simulated: bool,
    /// The owner's monthly rent limit in cents, when the profile has one.
    pub monthly_rent_limit_cents: Option<i64>,
}

impl SummaryInput {
    /// From today's stored call: the script as the questions, its findings, transcript and
    /// deferred questions. The caller supplies how it ended, the handoffs and the rent limit.
    pub fn from_call(
        call: &CallSession,
        ended: CallEnd,
        handoffs: Vec<OwnerHandoffRequest>,
        monthly_rent_limit_cents: Option<i64>,
    ) -> Self {
        let nonblank = |k: &str| {
            call.recipient
                .get(k)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        SummaryInput {
            call_id: call.call_id,
            property_id: call.property_id,
            property_label: None,
            contact_name: nonblank("name"),
            contact_role: nonblank("role"),
            started_at: call.started_at,
            ended_at: call.ended_at,
            ended,
            questions: call
                .script
                .iter()
                .map(|q| OutreachQuestion {
                    id: format!("bank:{}", q.claim_type),
                    text: q.question.clone(),
                    topic: topic_of_claim(&q.claim_type).to_owned(),
                    claim_type: q.claim_type.clone(),
                })
                .collect(),
            transcript: call.transcript.clone(),
            findings: call.findings.iter().map(InputFinding::from).collect(),
            deferred_questions: call.deferred_questions.clone(),
            handoffs,
            simulated: call.simulated,
            monthly_rent_limit_cents,
        }
    }
}

/// One statement in the summary, always with who said it and what kind of statement it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub said_by: SaidBy,
    pub kind: StatementKind,
    /// The claim in the speaker's words (redacted), or our derived statement.
    pub statement: String,
    pub topic: String,
    pub claim_type: Option<String>,
    pub value: Value,
    pub unit: Option<String>,
    pub amount_cents: Option<i64>,
    pub cost_kind: Option<CostKind>,
    pub period: Option<Period>,
    pub needs_verification: bool,
    /// For a derived item: the claim types or records it was derived from.
    pub derived_from: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub reported_by_counterparty: Vec<Item>,
    /// Only what an independent evidence class backs; a call alone backs nothing.
    pub verified: Vec<Item>,
    pub inferred_by_us: Vec<Item>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerState {
    Answered,
    /// Asked, with no clear answer.
    NotAnswered,
    Declined,
    NotReached,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub question_id: String,
    pub question: String,
    pub topic: String,
    pub state: AnswerState,
    pub said_by: SaidBy,
    pub kind: StatementKind,
    /// The answer in the counterparty's words, when there was one (redacted).
    pub answer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequested {
    pub said_by: SaidBy,
    pub kind: StatementKind,
    pub handoff_request_id: Uuid,
    pub reason: String,
    pub question: String,
    pub status: HandoffStatus,
    /// What became of it, in words.
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallSummary {
    pub call_id: Uuid,
    pub property_id: Option<Uuid>,
    pub property_label: Option<String>,
    pub contact_name: Option<String>,
    pub contact_role: Option<String>,
    pub started_at: Option<UtcDateTime>,
    pub ended_at: Option<UtcDateTime>,
    pub ended: CallEnd,
    pub simulated: bool,
    pub nothing_learned: bool,
    pub verification_note: String,
    pub listing: Section,
    pub costs: Section,
    pub infrastructure: Section,
    pub concerns: Section,
    pub answers: Vec<QuestionAnswer>,
    pub remaining_questions: Vec<Item>,
    pub requested_documents: Vec<Item>,
    pub next_steps: Vec<Item>,
    /// Always empty: the assistant cannot commit the owner.
    pub commitments_made: Vec<Item>,
    pub needs_owner_attention: Vec<Item>,
    pub approvals_requested: Vec<ApprovalRequested>,
}

// ---- Patterns ---------------------------------------------------------------------------------

struct Patterns {
    decline: PyRegex,
    documents: PyRegex,
    commitment: PyRegex,
    assistant_agrees: PyRegex,
    negation: PyRegex,
    assumed: PyRegex,
}

const DECLINE_PATTERN: &str = r"\b(can'?t (say|share|disclose|tell)|won'?t (say|share|disclose)|rather not|not at liberty|no comment|not (able|allowed) to (say|share|disclose)|prefer not)\b";
const DOCUMENTS_PATTERN: &str = r"\b(send|email|provide|need|bring|submit|forward)\b.{0,60}\b(financials|financial statements?|tax returns?|bank statements?|business plan|application|credit (report|check)|references?|proof of (income|funds|insurance)|insurance certificate|id|identification|documents?|paperwork|w-?9|p&l|profit and loss)\b";
const ASSISTANT_AGREES_PATTERN: &str = r"\b((we|i|the owner|my client|they)('ll| will| would| can| accept| agree| are ready)|(we|i) (accept|agree)|deal|sounds good,? (we|they))\b";
const NEGATION_PATTERN: &str = r"\b(can'?t|cannot|not|won'?t|unable)\b";
/// A line that states or assumes an agreement or a promise ("you'll send the deposit", "the owner
/// agreed to 4,200"). Over-matching only adds a line to what needs the owner's attention.
const ASSUMED_COMMITMENT_PATTERN: &str = r"\b(you('ll| will| are going to)|(the owner|my client|your client|you|we|i|they) (agreed|accepted|promised|committed)|(the owner|your client|my client) (will|'ll|is going to)|(we|i)('ll| will) (hold|reserve|keep|send))\b";

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let re = |p: &str| PyRegex::new(p, true);
        Patterns {
            decline: re(DECLINE_PATTERN),
            documents: re(DOCUMENTS_PATTERN),
            commitment: re(COMMITMENT_PATTERN),
            assistant_agrees: re(ASSISTANT_AGREES_PATTERN),
            negation: re(NEGATION_PATTERN),
            assumed: re(ASSUMED_COMMITMENT_PATTERN),
        }
    })
}

fn clean(text: &str) -> String {
    let (t, _, _) = redact(text.trim());
    t
}

fn value_clean(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(clean(s)),
        other => other.clone(),
    }
}

fn period_of(p: Option<MoneyPeriod>, amount: Option<i64>) -> Option<Period> {
    match (p, amount) {
        (Some(MoneyPeriod::Monthly), _) => Some(Period::Monthly),
        (Some(MoneyPeriod::Annual), _) => Some(Period::Annual),
        (Some(MoneyPeriod::PerSqft), _) => Some(Period::PerSqft),
        (None, Some(_)) => Some(Period::Unknown),
        (None, None) => None,
    }
}

fn cost_kind(topic: &str) -> Option<CostKind> {
    match topic {
        "rent" | "operating_expenses" => Some(CostKind::Rent),
        "utilities" => Some(CostKind::Utilities),
        "deposit" => Some(CostKind::Deposit),
        "lease_terms" => Some(CostKind::LeaseTerm),
        "cost" | "allowance" => Some(CostKind::Other),
        _ => None,
    }
}

fn is_infrastructure(f: &InputFinding) -> bool {
    matches!(f.topic.as_str(), "infrastructure" | "improvement")
        && f.claim_type != "usable_area_sqft"
}

fn has_value(f: &InputFinding) -> bool {
    !f.value.is_null() || f.amount_cents.is_some()
}

fn reported_item(f: &InputFinding) -> Item {
    let statement = f
        .reported
        .as_deref()
        .map(clean)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            clean(&format!(
                "{}: {}",
                f.claim_type,
                crate::py::value_str(&f.value)
            ))
        });
    let ck = cost_kind(&f.topic);
    let mut period = period_of(f.period, f.amount_cents);
    if f.topic == "improvement" && period == Some(Period::Unknown) {
        period = Some(Period::OneTime);
    }
    Item {
        said_by: SaidBy::Counterparty,
        kind: StatementKind::ReportedByCounterparty,
        statement,
        topic: f.topic.clone(),
        claim_type: Some(f.claim_type.clone()),
        value: value_clean(&f.value),
        unit: f.unit.clone(),
        amount_cents: f.amount_cents,
        cost_kind: ck,
        period,
        needs_verification: true,
        derived_from: Vec::new(),
    }
}

fn inferred(topic: &str, statement: String, derived_from: Vec<String>) -> Item {
    Item {
        said_by: SaidBy::SecondSite,
        kind: StatementKind::InferredByUs,
        statement,
        topic: topic.to_owned(),
        claim_type: None,
        value: Value::Null,
        unit: None,
        amount_cents: None,
        cost_kind: None,
        period: None,
        needs_verification: false,
        derived_from,
    }
}

fn quote(speaker: &str, topic: &str, text: &str) -> Item {
    Item {
        said_by: match speaker {
            "assistant" => SaidBy::Assistant,
            "owner" => SaidBy::Owner,
            "counterparty" => SaidBy::Counterparty,
            _ => SaidBy::SecondSite,
        },
        kind: StatementKind::TranscriptQuote,
        statement: clean(text),
        topic: topic.to_owned(),
        claim_type: None,
        value: Value::Null,
        unit: None,
        amount_cents: None,
        cost_kind: None,
        period: None,
        needs_verification: false,
        derived_from: Vec::new(),
    }
}

fn matches_question(q: &OutreachQuestion, f: &InputFinding) -> bool {
    (!q.claim_type.is_empty() && q.claim_type == f.claim_type)
        || f.question.as_deref().map(str::trim) == Some(q.text.trim())
}

fn was_asked(q: &OutreachQuestion, transcript: &[TranscriptTurn]) -> bool {
    transcript
        .iter()
        .any(|t| t.speaker == "assistant" && t.text.contains(q.text.trim()))
}

fn handoff_outcome(s: HandoffStatus) -> &'static str {
    match s {
        HandoffStatus::Pending => "requested; the owner has not been reached yet",
        HandoffStatus::Notified => "the owner was notified and has not answered",
        HandoffStatus::Accepted => "the owner accepted and joined or was dialled",
        HandoffStatus::Declined => "the owner declined",
        HandoffStatus::Deferred => "the owner chose to follow up later",
        HandoffStatus::Expired => "expired without an answer from the owner",
    }
}

/// The structured summary of one finished call.
pub fn summarize(input: &SummaryInput) -> CallSummary {
    let p = patterns();
    let mut s = CallSummary {
        call_id: input.call_id,
        property_id: input.property_id,
        property_label: input.property_label.as_deref().map(clean),
        contact_name: input.contact_name.as_deref().map(clean),
        contact_role: input.contact_role.as_deref().map(clean),
        started_at: input.started_at,
        ended_at: input.ended_at,
        ended: input.ended,
        simulated: input.simulated,
        nothing_learned: false,
        verification_note: NOTHING_VERIFIED.to_owned(),
        listing: Section::default(),
        costs: Section::default(),
        infrastructure: Section::default(),
        concerns: Section::default(),
        answers: Vec::new(),
        remaining_questions: Vec::new(),
        requested_documents: Vec::new(),
        next_steps: Vec::new(),
        commitments_made: Vec::new(),
        needs_owner_attention: Vec::new(),
        approvals_requested: Vec::new(),
    };

    // Reported facts, each in exactly one section.
    let known: Vec<&InputFinding> = input.findings.iter().filter(|f| has_value(f)).collect();
    for f in &known {
        let item = reported_item(f);
        if is_infrastructure(f) {
            s.infrastructure.reported_by_counterparty.push(item);
        } else if item.cost_kind.is_some() || f.amount_cents.is_some() {
            s.costs.reported_by_counterparty.push(item);
        } else {
            s.listing.reported_by_counterparty.push(item);
        }
    }

    // Derived costs and concerns.
    let one_time: Vec<&&InputFinding> = known
        .iter()
        .filter(|f| f.amount_cents.is_some() && f.topic == "improvement")
        .collect();
    if !one_time.is_empty() {
        let total: i64 = one_time.iter().filter_map(|f| f.amount_cents).sum();
        let parts: Vec<String> = one_time
            .iter()
            .map(|f| {
                format!(
                    "{} for {}",
                    format_money(f.amount_cents.unwrap_or(0), "USD"),
                    purpose_of(f)
                )
            })
            .collect();
        let from: Vec<String> = one_time.iter().map(|f| f.claim_type.clone()).collect();
        s.costs.inferred_by_us.push(Item {
            amount_cents: Some(total),
            period: Some(Period::OneTime),
            cost_kind: Some(CostKind::Other),
            ..inferred(
                "cost",
                format!(
                    "Derived: reported one-time costs of {} ({}) would add to the occupancy cost, if \
                     confirmed.",
                    format_money(total, "USD"),
                    parts.join("; ")
                ),
                from.clone(),
            )
        });
        for f in one_time {
            s.concerns.inferred_by_us.push(inferred(
                "improvement",
                format!(
                    "Derived concern: the reported {} of {} is a cost the tenant may carry; it is \
                     unverified and may change the property's ranking.",
                    purpose_of(f),
                    format_money(f.amount_cents.unwrap_or(0), "USD")
                ),
                vec![f.claim_type.clone()],
            ));
        }
    }
    if let Some(limit) = input.monthly_rent_limit_cents {
        for f in known.iter().filter(|f| {
            f.topic == "rent" && f.period == Some(MoneyPeriod::Monthly) && f.amount_cents.is_some()
        }) {
            let amount = f.amount_cents.unwrap_or(0);
            if amount > limit {
                s.concerns.inferred_by_us.push(inferred(
                    "rent",
                    format!(
                        "Derived concern: the reported rent of {} a month is over the owner's limit \
                         of {} a month.",
                        format_money(amount, "USD"),
                        format_money(limit, "USD")
                    ),
                    vec![f.claim_type.clone(), "profile.monthly_rent_limit".into()],
                ));
            }
        }
    }
    for f in known.iter().filter(|f| {
        matches!(f.claim_type.as_str(), "in_unit_plumbing" | "hvac_zonable")
            && f.value == Value::Bool(false)
    }) {
        let what = if f.claim_type == "in_unit_plumbing" {
            "no in-suite plumbing"
        } else {
            "HVAC that cannot be zoned"
        };
        s.concerns.inferred_by_us.push(inferred(
            "infrastructure",
            format!(
                "Derived concern: the contact reported {what}; the build-out may need that work."
            ),
            vec![f.claim_type.clone()],
        ));
    }

    // Every outreach question, with its state.
    for q in &input.questions {
        let found: Vec<&InputFinding> = input
            .findings
            .iter()
            .filter(|f| matches_question(q, f))
            .collect();
        let answer = found.iter().find_map(|f| f.reported.as_deref()).map(clean);
        let state = if found.iter().any(|f| has_value(f)) {
            AnswerState::Answered
        } else if answer
            .as_deref()
            .is_some_and(|a| p.decline.search(a).is_some())
        {
            AnswerState::Declined
        } else if !found.is_empty() || was_asked(q, &input.transcript) {
            AnswerState::NotAnswered
        } else {
            AnswerState::NotReached
        };
        let (said_by, kind) = if state == AnswerState::NotReached {
            (SaidBy::SecondSite, StatementKind::InferredByUs)
        } else {
            (SaidBy::Counterparty, StatementKind::ReportedByCounterparty)
        };
        if state != AnswerState::Answered {
            let why = match state {
                AnswerState::Declined => "declined",
                AnswerState::NotReached => "not reached",
                _ => "not answered",
            };
            s.remaining_questions.push(inferred(
                &q.topic,
                format!("{} ({why})", q.text.trim()),
                vec![q.id.clone()],
            ));
        }
        s.answers.push(QuestionAnswer {
            question_id: q.id.clone(),
            question: q.text.trim().to_owned(),
            topic: q.topic.clone(),
            state,
            said_by,
            kind,
            answer,
        });
    }
    for d in &input.deferred_questions {
        s.remaining_questions.push(Item {
            said_by: SaidBy::Counterparty,
            kind: StatementKind::TranscriptQuote,
            ..inferred(
                "owner_question",
                format!("They asked, for you: {}", clean(d)),
                Vec::new(),
            )
        });
    }

    // Transcript: requested documents and commitment look-alikes.
    for t in &input.transcript {
        let text = t.text.as_str();
        if t.speaker == "counterparty" {
            if p.documents.search(text).is_some() {
                s.requested_documents
                    .push(quote(&t.speaker, "documents", text));
            }
            if p.commitment.search(text).is_some() || p.assumed.search(text).is_some() {
                let mut q = quote(&t.speaker, "commitment", text);
                q.statement = format!(
                    "They asked for or assumed a commitment; none was made: {}",
                    q.statement
                );
                s.needs_owner_attention.push(q);
            }
        } else if t.speaker == "assistant"
            && (p.assistant_agrees.search(text).is_some()
                || p.commitment.search(text).is_some()
                || p.assumed.search(text).is_some())
            && p.negation.search(text).is_none()
        {
            let mut q = quote(&t.speaker, "commitment", text);
            q.statement = format!(
                "The assistant said something that sounds like a commitment; it is not one and binds \
                 no one: {}",
                q.statement
            );
            s.needs_owner_attention.push(q);
        }
    }

    // Approvals requested: the handoffs and what became of them.
    for h in &input.handoffs {
        s.approvals_requested.push(ApprovalRequested {
            said_by: SaidBy::SecondSite,
            kind: StatementKind::SystemRecord,
            handoff_request_id: h.handoff_request_id,
            reason: h.reason.as_str().to_owned(),
            question: clean(&h.question),
            status: h.status,
            outcome: handoff_outcome(h.status).to_owned(),
        });
    }

    // Next steps, derived.
    let open_handoffs: Vec<&ApprovalRequested> = s
        .approvals_requested
        .iter()
        .filter(|a| !matches!(a.status, HandoffStatus::Accepted))
        .collect();
    for a in &open_handoffs {
        s.next_steps.push(inferred(
            "decision",
            format!("Decide: {}", a.question),
            vec![format!("handoff:{}", a.handoff_request_id)],
        ));
    }
    if !s.requested_documents.is_empty() {
        s.next_steps.push(inferred(
            "documents",
            format!(
                "They asked for {} document request(s); you decide what, if anything, to send.",
                s.requested_documents.len()
            ),
            vec!["requested_documents".into()],
        ));
    }
    if !s.remaining_questions.is_empty() {
        s.next_steps.push(inferred(
            "follow_up",
            format!(
                "Follow up on {} open question(s).",
                s.remaining_questions.len()
            ),
            vec!["remaining_questions".into()],
        ));
    }
    if !known.is_empty() {
        s.next_steps.push(inferred(
            "verification",
            "Verify the reported facts before relying on them.".into(),
            known.iter().map(|f| f.claim_type.clone()).collect(),
        ));
    }

    let spoke = input.transcript.iter().any(|t| t.speaker == "counterparty");
    s.nothing_learned = known.is_empty()
        && (!spoke || matches!(input.ended, CallEnd::NoAnswer | CallEnd::Voicemail));
    s
}

/// Statements joined by "; " without their own end punctuation, ending in one full stop (or the
/// last one's question mark).
fn joined(items: &[String]) -> String {
    let bare: Vec<&str> = items
        .iter()
        .map(|i| i.trim().trim_end_matches(['.', ';', ',']))
        .collect();
    let mut out = bare.join("; ");
    if !out.ends_with('?') {
        out.push('.');
    }
    out
}

fn push_bounded(out: &mut String, part: &str, max: usize) -> bool {
    let have = out.chars().count();
    let add = part.chars().count();
    if have + add <= max {
        out.push_str(part);
        return true;
    }
    // Cut only at a clause boundary ("; "), never inside a figure; drop the part if none fits.
    let room = max.saturating_sub(have + 1);
    let head: String = part.chars().take(room).collect();
    if let Some(at) = head.rfind("; ") {
        out.push_str(&head[..at]);
    }
    if out.chars().count() < max {
        out.push('…');
    }
    false
}

/// The short plain-text update for the owner, at most [`MAX_OWNER_TEXT_CHARS`] characters:
/// "simulated" first when it was, then what needs the owner's decision, then the reported facts,
/// each as said by the contact, never as established.
pub fn owner_text(s: &CallSummary) -> String {
    let max = MAX_OWNER_TEXT_CHARS;
    let who = match (&s.contact_name, &s.contact_role) {
        (Some(n), Some(r)) => format!("{n} ({r})"),
        (Some(n), None) => n.clone(),
        (None, Some(r)) => format!("the {r}"),
        (None, None) => "the property manager".to_owned(),
    };
    let mut parts: Vec<String> = Vec::new();
    let decide: Vec<String> = s
        .approvals_requested
        .iter()
        .filter(|a| !matches!(a.status, HandoffStatus::Accepted))
        .map(|a| a.question.clone())
        .chain(s.needs_owner_attention.iter().map(|a| a.statement.clone()))
        .collect();
    if !decide.is_empty() {
        parts.push(format!("Needs your decision: {} ", joined(&decide)));
    }
    if s.nothing_learned {
        let how = match s.ended {
            CallEnd::NoAnswer => "No answer",
            CallEnd::Voicemail => "Reached voicemail",
            CallEnd::Failed => "The call failed",
            CallEnd::HungUp => "The call ended early",
            _ => "The call ended",
        };
        parts.push(format!("{how}; nothing was learned about the listing. "));
    } else {
        let reported: Vec<String> = [&s.costs, &s.infrastructure, &s.listing]
            .iter()
            .flat_map(|sec| sec.reported_by_counterparty.iter())
            .map(|i| i.statement.clone())
            .fold(Vec::new(), |mut acc: Vec<String>, st| {
                // The same clause can back two findings (an answer and its amount): say it once.
                let bare = st.trim().trim_end_matches(['.', ';', ',']).to_owned();
                if !acc.iter().any(|a| a.contains(&bare)) {
                    acc.push(st);
                }
                acc
            });
        if reported.is_empty() {
            parts.push(format!("{who} gave no clear answers. "));
        } else {
            parts.push(format!("{who} said: {} Not verified. ", joined(&reported)));
        }
        for c in &s.concerns.inferred_by_us {
            parts.push(format!("{} ", c.statement));
        }
    }
    let open = s
        .answers
        .iter()
        .filter(|a| a.state != AnswerState::Answered)
        .count();
    if open > 0 {
        parts.push(format!(
            "{open} of {} questions still open.",
            s.answers.len()
        ));
    }
    let mut out = String::new();
    if s.simulated {
        out.push_str(SIMULATED_LABEL);
    }
    for part in parts {
        if !push_bounded(&mut out, &part, max) {
            break;
        }
    }
    // Every part was redacted when the summary was built. A second pass over the joined text
    // would let one clause's sensitive word ("number", "account") blank every figure after it.
    out.trim_end().chars().take(max).collect()
}
