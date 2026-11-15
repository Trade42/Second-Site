//! The Response Agent: what the assistant may say next on a call, decided by Second Site, not by
//! the voice model (owner's specification 2026-11-15, sections 3 to 6).
//!
//! Pure and deterministic: no database, no HTTP, no model call, no clock, no randomness. One
//! counterparty turn in, one [`Decision`] out, plus the next [`ConversationState`]. A turn id that
//! was already decided returns the stored decision unchanged.
//!
//! Rules it enforces:
//! - It never commits the owner. The only moves are: ask an authorised question, restate the
//!   owner's stated requirement from the profile, say it will take a proposal back to the owner,
//!   and escalate.
//! - It never invents. A question the context cannot answer gets "I don't have that; I can ask the
//!   owner", and an escalation when the counterparty says it blocks the conversation.
//! - Everything the counterparty says is a claim reported by the counterparty (the existing
//!   `OWNER_REPORTED` class, attribution `counterparty`), marked as needing verification.
//! - Sensitive identifiers (SSN, tax id, bank or card numbers, date of birth, driver's licence,
//!   passwords) escalate to a direct owner conversation and are redacted from every output.
//! - Asked whether it is a person or a recording, it says it is the owner's authorised automated
//!   assistant.
//!
//! Bounds: the utterance is cut to [`MAX_ANALYSIS_CHARS`] characters before any analysis; at most
//! [`MAX_FINDINGS_PER_TURN`] findings per turn. Every pattern runs on the `regex` crate (through
//! [`PyRegex`]), whose search is linear in the input (no backtracking, no back-references); the
//! hand-written lexer and extractors are single passes over the token list (bounded windows).

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ss_core::EvidenceClassification;

use crate::communications::call_script::{
    extract, AVAILABILITY_CLAIM, QUESTION_BANK, UNSURE_PATTERN,
};
use crate::communications::escalation::detect_escalation;
use crate::contracts::communications::{
    AuthorityScope, CallQuestion, HandoffReason, StructuredFinding, AUTOMATION_DISCLOSURE,
};
use crate::py::PyRegex;

/// Characters of an utterance that are analysed; the rest is cut (and the cut is reported).
pub const MAX_ANALYSIS_CHARS: usize = 2_000;
/// Findings returned for one turn at most.
pub const MAX_FINDINGS_PER_TURN: usize = 6;
/// The largest amount read as money (USD 100 billion, in cents); larger is not a reading.
pub const MAX_AMOUNT_CENTS: i128 = 10_000_000_000_000;
/// What replaces a redacted value.
pub const REDACTED: &str = "[REDACTED]";

/// The legacy hold line (`on_utterance`), used for commitments and the legacy rules.
pub const HOLD_LINE: &str = "That's a decision for the owner, and I can't agree to anything on \
                             their behalf. Let me check if they can join right now.";
pub const SENSITIVE_LINE: &str = "I can't take or share personal or financial identifiers on \
                                  this call. The owner can give that to you directly; let me see \
                                  if they can join right now.";
pub const OWNER_REQUESTED_LINE: &str =
    "Of course. Let me check whether the owner can join right now.";
pub const CONCESSION_LINE: &str =
    "I can't negotiate terms myself, but I'll take that proposal back to the owner.";
pub const LEGAL_LINE: &str =
    "That's a question for the owner; I'll make sure they get back to you on it.";
pub const OUT_OF_SCOPE_LINE: &str =
    "That's something the owner handles directly. Let me see if they can join right now.";
pub const BLOCKING_LINE: &str =
    "I don't have that information. Let me see if the owner can join right now.";
pub const DONT_KNOW_LINE: &str = "I don't have that; I can ask the owner and get back to you.";
pub const IDENTITY_LINE: &str =
    "I'm the owner's authorized automated assistant, not a person or a recording.";

// ---- Input ------------------------------------------------------------------------------------

/// Who spoke a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    Counterparty,
    Owner,
    Assistant,
    System,
}

/// A fact about the property already on record, with how it is known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownFact {
    /// A [`topic_of_question`] topic, e.g. `"parking"`, `"infrastructure"`.
    pub topic: String,
    pub statement: String,
    pub classification: EvidenceClassification,
}

/// A requirement from the owner's profile that the assistant may restate as the owner's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRequirement {
    pub topic: String,
    /// Spoken as is: "the owner is looking at up to $4,500 a month".
    pub statement: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionSource {
    /// `OutreachRequest.questions`.
    Request,
    /// The question bank (`build_script`).
    Bank,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedQuestion {
    /// `"request:<n>"` or `"bank:<claim_type>"`.
    pub id: String,
    pub topic: String,
    pub text: String,
    pub claim_type: String,
    pub requirement_id: Option<String>,
    pub source: QuestionSource,
}

impl PlannedQuestion {
    fn as_call_question(&self) -> CallQuestion {
        CallQuestion {
            question: self.text.clone(),
            claim_type: self.claim_type.clone(),
            requirement_id: self.requirement_id.clone(),
        }
    }
}

/// Everything fixed for the call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallContext {
    pub authority: AuthorityScope,
    pub disclosure: String,
    pub property_summary: String,
    pub known_facts: Vec<KnownFact>,
    pub owner_requirements: Vec<OwnerRequirement>,
    /// From [`plan_questions`]: the request's own questions first, then the bank's.
    pub questions: Vec<PlannedQuestion>,
}

impl Default for CallContext {
    fn default() -> Self {
        Self {
            authority: AuthorityScope::default(),
            disclosure: AUTOMATION_DISCLOSURE.to_owned(),
            property_summary: String::new(),
            known_facts: Vec::new(),
            owner_requirements: Vec::new(),
            questions: Vec::new(),
        }
    }
}

/// One new utterance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utterance {
    pub turn_id: u64,
    pub speaker: Speaker,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTurn {
    pub turn_id: u64,
    pub speaker: Speaker,
    /// Cut and redacted.
    pub text: String,
}

/// What the agent knows so far. Only [`start`], [`step`] and [`resume`] change it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationState {
    /// Question ids asked, in order.
    pub asked: Vec<String>,
    pub answered: Vec<String>,
    /// The question asked last and not answered yet.
    pub current: Option<String>,
    /// Question ids never asked, with why (outside `may_ask_about`, or in `may_not`).
    pub skipped: Vec<(String, String)>,
    pub findings: Vec<AgentFinding>,
    pub turns: Vec<StoredTurn>,
    pub decisions: Vec<Decision>,
    /// A live handoff is pending or in progress: the agent stays silent until [`resume`].
    pub awaiting_owner: bool,
    /// Questions for the owner that do not hold the call.
    pub owner_followups: Vec<String>,
    pub ended: bool,
}

// ---- Output -----------------------------------------------------------------------------------

/// What the voice model says next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case")]
pub enum Say {
    /// Speak this wording exactly (refusals, the identity answer, hold lines, the closing).
    Exact(String),
    /// A short instruction in plain words; the voice model phrases it.
    Instruct(String),
    /// Say nothing.
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Specialist {
    Finance,
    Feasibility,
    Property,
    Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoneyPeriod {
    Monthly,
    Annual,
    PerSqft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerKind {
    Yes,
    No,
    DontKnow,
    /// A value was stated.
    Stated,
    /// An answer that states nothing clear.
    Unclear,
}

/// One claim, as the counterparty reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentFinding {
    pub topic: String,
    pub claim_type: String,
    /// The clause as reported (cut, redacted, at most 200 characters).
    pub reported: String,
    /// The value read: an integer, a bool, a string or null (unclear).
    pub value: Value,
    pub unit: Option<String>,
    /// Money, in integer cents.
    pub amount_cents: Option<i64>,
    pub period: Option<MoneyPeriod>,
    /// What the money is for, e.g. "plumbing retrofit".
    pub purpose: Option<String>,
    /// Always `"counterparty"`.
    pub reported_by: String,
    /// Always `OWNER_REPORTED` (the class the evidence store uses for what a caller states).
    pub classification: EvidenceClassification,
    pub needs_verification: bool,
    pub specialists: Vec<Specialist>,
    /// The question this finding answers, when it does.
    pub question_id: Option<String>,
    pub answer_kind: Option<AnswerKind>,
    pub requirement_id: Option<String>,
}

impl AgentFinding {
    /// The contract shape `record_finding` takes.
    pub fn to_structured_finding(&self, question: &str) -> StructuredFinding {
        StructuredFinding {
            question: question.to_owned(),
            answer: Some(self.reported.clone()),
            claim_type: Some(self.claim_type.clone()),
            value: self.value.clone(),
            unit: self.unit.clone(),
            speaker: Some("counterparty".into()),
            transcript_offset_s: None,
            classification: "OWNER_REPORTED".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationRule {
    SensitiveData,
    Commitment,
    /// One of the five `escalation::RULES`.
    Legacy,
    Concession,
    OwnerRequested,
    LegalOrLicensing,
    MayNotTopic,
    BlockingUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Escalation {
    pub reason: HandoffReason,
    pub rule: EscalationRule,
    /// For the owner (redacted; never the sensitive value).
    pub question_for_owner: String,
    /// `"HIGH"` or `"NORMAL"`, as `OwnerHandoffRequest.urgency`.
    pub urgency: String,
    /// True: hold the call and request a live handoff now. False: take it back to the owner and
    /// keep the call going.
    pub hold: bool,
    /// A direct owner-to-counterparty conversation for sensitive data.
    pub sensitive_handoff: bool,
}

/// One decision for one turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub turn_id: u64,
    pub say: Say,
    pub findings: Vec<AgentFinding>,
    /// The union of the findings' specialists, sorted.
    pub specialists: Vec<Specialist>,
    pub escalation: Option<Escalation>,
    pub answered_question: Option<String>,
    pub next_question: Option<PlannedQuestion>,
    pub end_call: bool,
    /// The utterance as it may be stored (cut and redacted).
    pub stored_text: String,
    pub redacted: bool,
    pub truncated: bool,
    /// A question for the owner that does not hold the call.
    pub owner_followup: Option<String>,
    /// A repeated turn id that was not the latest, or a turn after the call ended.
    pub ignored: bool,
}

impl Decision {
    fn silent(turn_id: u64, stored_text: String, redacted: bool, truncated: bool) -> Self {
        Self {
            turn_id,
            say: Say::Silent,
            findings: Vec::new(),
            specialists: Vec::new(),
            escalation: None,
            answered_question: None,
            next_question: None,
            end_call: false,
            stored_text,
            redacted,
            truncated,
            owner_followup: None,
            ignored: false,
        }
    }
}

// ---- Context builders -------------------------------------------------------------------------

/// Topics in order of precedence, each with the phrases that name it (lower case, substring).
pub const TOPICS: &[(&str, &[&str])] = &[
    ("guarantor", &["guarant"]),
    ("deposit", &["deposit"]),
    ("utilities", &["utilit"]),
    (
        "rent",
        &[
            "rent",
            "price",
            "asking",
            "budget",
            "how much",
            "per square foot",
            "nnn",
            "cam ",
        ],
    ),
    (
        "lease_terms",
        &[
            "lease term",
            "how long",
            "renew",
            "term",
            "lease length",
            "years",
        ],
    ),
    ("parking", &["parking", "park "]),
    (
        "showing_times",
        &[
            "tour",
            "showing",
            "visit",
            "see the space",
            "walk through",
            "walkthrough",
        ],
    ),
    (
        "permitted_use",
        &[
            "zoned",
            "zoning",
            "permitted",
            "allowed",
            "use clause",
            "operate",
        ],
    ),
    (
        "business",
        &[
            "what kind of business",
            "what business",
            "what type of business",
            "what do they do",
        ],
    ),
    (
        "timing",
        &[
            "move in",
            "move-in",
            "open",
            "start date",
            "when do",
            "when would they",
            "timeline",
        ],
    ),
    ("availability", &["available", "availability", "vacant"]),
    (
        "infrastructure",
        &[
            "amp", "electric", "panel", "plumbing", "sink", "drain", "water", "hvac", "ventilat",
            "air ", "square f", "sq ft", "ceiling", "power", "gas",
        ],
    ),
];

/// The topic of a question (the request's, the counterparty's): one of `deposit`, `rent`,
/// `lease_terms`, `utilities`, `availability`, `parking`, `infrastructure`, `permitted_use`,
/// `showing_times`, `business`, `timing`, `guarantor`, `general`.
pub fn topic_of_question(text: &str) -> &'static str {
    topics_in(text).first().copied().unwrap_or("general")
}

/// Every [`TOPICS`] topic the text names, in precedence order.
pub fn topics_in(text: &str) -> Vec<&'static str> {
    let t = text.to_lowercase();
    // A phrase counts where it starts a word ("rental" names rent, "currently" does not).
    let at_word_start = |w: &str| {
        t.match_indices(w).any(|(i, _)| {
            t[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
        })
    };
    TOPICS
        .iter()
        .filter(|(_, words)| words.iter().any(|w| at_word_start(w)))
        .map(|(topic, _)| *topic)
        .collect()
}

/// The topic of a bank claim type. All four bank questions come from infrastructure
/// assessments, so they are `infrastructure`; the closing availability question is
/// `availability`.
pub fn topic_of_claim(claim_type: &str) -> &'static str {
    if claim_type == AVAILABILITY_CLAIM {
        "availability"
    } else {
        "infrastructure"
    }
}

/// The `may_not` key a topic falls under, when one does.
pub fn may_not_key(topic: &str) -> Option<&'static str> {
    match topic {
        "deposit" => Some("discuss_deposits_or_payment"),
        "guarantor" => Some("offer_guarantor"),
        "sensitive" => Some("share_sensitive_personal_info"),
        "commitment" => Some("agree_to_terms"),
        _ => None,
    }
}

/// The questions for the call: the request's own (`OutreachRequest.questions`, blank ones
/// dropped) first, then the bank's (`build_script`), a bank question whose text equals a request
/// question dropped.
pub fn plan_questions(request_questions: &[String], bank: &[CallQuestion]) -> Vec<PlannedQuestion> {
    let mut out: Vec<PlannedQuestion> = Vec::new();
    for (i, q) in request_questions.iter().enumerate() {
        let text = q.trim();
        if text.is_empty() {
            continue;
        }
        out.push(PlannedQuestion {
            id: format!("request:{i}"),
            topic: topic_of_question(text).to_owned(),
            text: text.to_owned(),
            claim_type: format!("outreach_question_{i}"),
            requirement_id: None,
            source: QuestionSource::Request,
        });
    }
    for q in bank {
        if out
            .iter()
            .any(|p| p.text.eq_ignore_ascii_case(q.question.trim()))
        {
            continue;
        }
        out.push(PlannedQuestion {
            id: format!("bank:{}", q.claim_type),
            topic: topic_of_claim(&q.claim_type).to_owned(),
            text: q.question.clone(),
            claim_type: q.claim_type.clone(),
            requirement_id: q.requirement_id.clone(),
            source: QuestionSource::Bank,
        });
    }
    out
}

/// `cents` as spoken money: "$4,500" or "$4,500.50" for USD, "4,500 EUR" otherwise.
pub fn format_money(cents: i64, currency: &str) -> String {
    let whole = cents / 100;
    let frac = cents % 100;
    let digits = whole.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    if frac != 0 {
        grouped.push_str(&format!(".{frac:02}"));
    }
    if currency == "USD" {
        format!("${grouped}")
    } else {
        format!("{grouped} {currency}")
    }
}

/// The owner's requirements the assistant may restate: the business type, the monthly rent limit
/// ("the owner is looking at up to ... a month") and the target opening date.
pub fn owner_requirements(profile: &ss_core::BusinessExpansionProfile) -> Vec<OwnerRequirement> {
    let mut out = vec![OwnerRequirement {
        topic: "business".into(),
        statement: format!(
            "the owner runs a {} business",
            profile.business_type.replace('_', " ")
        ),
    }];
    if let Some(limit) = profile.monthly_rent_limit {
        out.push(OwnerRequirement {
            topic: "rent".into(),
            statement: format!(
                "the owner is looking at up to {} a month",
                format_money(limit, profile.currency.as_str())
            ),
        });
    }
    if let Some(d) = profile.target_opening_date {
        out.push(OwnerRequirement {
            topic: "timing".into(),
            statement: format!("the owner hopes to open around {}", d.format("%B %Y")),
        });
    }
    out
}

// ---- Patterns ---------------------------------------------------------------------------------

struct Patterns {
    unsure: PyRegex,
    sensitive: PyRegex,
    ssn_shape: PyRegex,
    commitment: PyRegex,
    concession: PyRegex,
    owner_requested: PyRegex,
    legal: PyRegex,
    identity: PyRegex,
    blocking: PyRegex,
    interrogative: PyRegex,
}

/// Sensitive-data words (re.I). A bare "license" is a licensing question, not this.
pub const SENSITIVE_PATTERN: &str = r"\b(ssn|social security|tax id|tax identification|taxpayer id\w*|ein|itin|bank account|account number|routing number|card number|credit card|debit card|cvv|security code|date of birth|dob|birth ?date|birthday|driver'?s? licen[cs]e|licen[cs]e number|password|passcode|pin number|pin code|routing|acct|account (no|#|ending)|(my|your|his|her|their|client'?s|owner'?s) social|social (is|number)|born)\b";
/// A commitment asked of the owner (re.I).
pub const COMMITMENT_PATTERN: &str = r"\b(sign (the|a|this|that)? ?(lease|contract|paperwork|papers|agreement|application|today|now)|hold (it|this|that|the (space|unit|suite|property))|reserve (it|this|that|the (space|unit|suite))|put (down|a hold)|application|apply|commit(ment)?|lock (it|this|that) (in|down)|move forward|take (it|the space|the unit)|ready to proceed|we'?re agreed|agreed (at|on|to)|(we have|we've got|have we got|do we have|is it|it's|it is|that's|thats) a deal|(it's|its|it is) yours|put (you|your client|them|the owner|him|her) down|take that as a (yes|go)|verbal (yes|commitment|ok|okay|agreement)|say yes|mark (it|you|them|the (space|unit)) (as )?(pending|taken|reserved)|lock in (a |the |that |this |your )?(rate|price|date|move|rent|lease|term|deal)|(take|keep|pull|taking|keeping|pulling) (it |the (space|unit) )?off the market|for (your )?signature|send (over )?(the|a) (lease|contract)|draw up (the|a) (lease|contract)|tell (the landlord|them|him|her) (you'?re|your client'?s|you are) (in|interested|taking)|want (it|the space|the unit) or not|(owner|client) wants? (it|the space|the unit)|(write|send|get|cut) (me |us )?a (check|cheque)|wire (the|it|money|funds|a))\b";
/// A counter-offer or a concession asked (re.I).
pub const CONCESSION_PATTERN: &str = r"\b(would (the owner|your client|they|he|she) (go (to|up|higher|as high)|come (up|down)|do \$?\d|accept|take (it|that|the)|consider (a|an|the|paying|\$?\d)|pay (more|\$?\d)|agree|be (willing|open|flexible))|counter ?-?offer|counter|best and final|final offer|meet (me|us) (in the middle|halfway|at)|come (up|down) to|willing to (pay|go|do|offer)|can (the owner|your client|they) (do|go|pay) \$?\d|if (we|i) (came|come|went|go) (down|up) to)\b";
/// The counterparty asks for the owner (re.I), beyond the legacy rule.
pub const OWNER_REQUESTED_PATTERN: &str = r"\b((speak|talk|chat) (directly )?(to|with) (the |your )?(owner|client|boss|principal|decision[- ]maker|business owner|a (real )?(person|human))|(put|get) (the |your )?(owner|client|boss) on|have (the |your )?(owner|client|boss) (call|phone|ring|contact|reach)|deal (directly )?with (the |your )?(owner|client)|(is|can) (the|your) (owner|client) (available|there|join|get on|be on))\b";
/// A legal or licensing question (re.I).
pub const LEGAL_PATTERN: &str = r"\b(lawyer|attorney|legal|legally|liability|liabilities|indemnif\w*|lawsuit|litigation|licen[cs]e|licen[cs]ed|licen[cs]es|licensing|certificate of occupancy)\b";
/// "Are you a person / a recording?" (re.I).
pub const IDENTITY_PATTERN: &str = r"\b((are|is) (you|this|it) (a |an |just a |even a )?(real |live |actual )?(person|human|robot|bot|machine|recording|ai|computer|automated|recorded)|am i (talking|speaking) (to|with) (a |an )?(real |live |actual )?(person|human|machine|robot|bot|recording|computer|ai)|who am i (talking|speaking) (to|with))\b";
/// The counterparty says the unknown blocks the conversation (re.I).
pub const BLOCKING_PATTERN: &str = r"\b(before (we|i) (can|go|move|continue|proceed)|need (to know|that|this) (first|now|before)|can'?t (go|move|continue|proceed|show)|won'?t (show|continue|proceed)|need an answer|deal ?breaker)\b";
/// A question by its first word (re.I).
pub const INTERROGATIVE_PATTERN: &str = r"^\W*(what|what's|whats|how|when|where|who|which|why|do|does|did|is|are|can|could|would|will|has|have)\b";

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        unsure: PyRegex::new(UNSURE_PATTERN, true),
        sensitive: PyRegex::new(SENSITIVE_PATTERN, true),
        ssn_shape: PyRegex::new(r"\b\d{3}[- ]\d{2}[- ]\d{4}\b", false),
        commitment: PyRegex::new(COMMITMENT_PATTERN, true),
        concession: PyRegex::new(CONCESSION_PATTERN, true),
        owner_requested: PyRegex::new(OWNER_REQUESTED_PATTERN, true),
        legal: PyRegex::new(LEGAL_PATTERN, true),
        identity: PyRegex::new(IDENTITY_PATTERN, true),
        blocking: PyRegex::new(BLOCKING_PATTERN, true),
        interrogative: PyRegex::new(INTERROGATIVE_PATTERN, true),
    })
}

// ---- Cut and redact ---------------------------------------------------------------------------

/// The first [`MAX_ANALYSIS_CHARS`] characters, with typographic apostrophes read as `'`, and
/// whether anything was cut.
pub fn cut(text: &str) -> (String, bool) {
    let mut out = String::new();
    for (n, c) in text.chars().enumerate() {
        if n == MAX_ANALYSIS_CHARS {
            return (out, true);
        }
        out.push(match c {
            '\u{2019}' | '\u{2018}' | '\u{2BC}' | '`' | '\u{B4}' | '\u{2032}' | '\u{FF07}' => '\'',
            c => c,
        });
    }
    (out, false)
}

const DIGIT_WORDS: [&str; 11] = [
    "zero", "oh", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
];

/// Redaction of sensitive values. Always: every run of 9 or more digits (digits or spoken digit
/// words joined by spaces, `-`, `.`, `/` or `,`), which also catches phone numbers. When a
/// sensitive word is present: every digit and every run of two or more number words, and the word
/// after "password"/"passcode"/"pin". Returns the text, whether anything was redacted, and the
/// sensitive label (the word found, or "a social security number" / "an account or card number"
/// for an SSN-shaped, 9-digit or 12-plus-digit run without a word).
pub fn redact(text: &str) -> (String, bool, Option<String>) {
    let p = patterns();
    let keyword = p
        .sensitive
        .search(text)
        .and_then(|m| m.group(0).map(|s| s.to_lowercase()));
    let ssn_shape = p.ssn_shape.is_match(text);
    // Tokens: (start, end, kind) where kind 0 = digits, 1 = digit word, 2 = number word, 3 = other
    // word, 4 = separator, 5 = other.
    let mut toks: Vec<(usize, usize, u8)> = Vec::new();
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let mut end = i + c.len_utf8();
        if c.is_ascii_digit() {
            while let Some(&(j, d)) = it.peek() {
                if !d.is_ascii_digit() {
                    break;
                }
                end = j + d.len_utf8();
                it.next();
            }
            toks.push((i, end, 0));
        } else if c.is_alphanumeric() || c == '\'' {
            while let Some(&(j, d)) = it.peek() {
                if !(d.is_alphanumeric() || d == '\'') || d.is_ascii_digit() {
                    break;
                }
                end = j + d.len_utf8();
                it.next();
            }
            let w = text[i..end].to_lowercase();
            let kind = if DIGIT_WORDS.contains(&w.as_str()) {
                1
            } else if spoken_value(&w).is_some() {
                2
            } else {
                3
            };
            toks.push((i, end, kind));
        } else if matches!(c, ' ' | '-' | '.' | '/' | ',') {
            toks.push((i, end, 4));
        } else {
            toks.push((i, end, 5));
        }
    }
    let mut redact_tok = vec![false; toks.len()];
    let mut long_run = false;
    // Runs of digit tokens / digit words joined by single separators: (first, last, digits,
    // whether the run is spoken digit words only).
    let mut runs: Vec<(usize, usize, usize, bool)> = Vec::new();
    // A run is digits, spoken digit words, or spoken pairs ("eleven", "twenty": two digits each;
    // a digit word right after a tens word adds none, "twenty one" is two digits). "hundred",
    // "thousand" and "million" end a run.
    let lw = |k: usize| text[toks[k].0..toks[k].1].to_lowercase();
    let in_run = |k: usize| {
        matches!(toks[k].2, 0 | 1)
            || (toks[k].2 == 2 && !matches!(lw(k).as_str(), "hundred" | "thousand" | "million"))
    };
    let tens = |k: usize| toks[k].2 == 2 && spoken_value(&lw(k)).is_some_and(|v| v >= 20);
    let mut k = 0;
    while k < toks.len() {
        if !in_run(k) {
            k += 1;
            continue;
        }
        let start = k;
        let mut digits = 0usize;
        let mut last = k;
        let mut j = k;
        let mut spoken = true;
        while j < toks.len() && in_run(j) {
            digits += match toks[j].2 {
                0 => {
                    spoken = false;
                    toks[j].1 - toks[j].0
                }
                2 => 2,
                _ if j >= start + 2 && tens(j - 2) => 0,
                _ => 1,
            };
            last = j;
            if j + 2 < toks.len() && toks[j + 1].2 == 4 && in_run(j + 2) {
                j += 2;
            } else {
                break;
            }
        }
        runs.push((start, last, digits, spoken));
        if digits >= 9 {
            for r in redact_tok.iter_mut().take(last + 1).skip(start) {
                *r = true;
            }
            // Nine digits is the SSN, EIN and routing shape; 10 and 11 are phone numbers.
            if digits == 9 || digits >= 12 {
                long_run = true;
            }
        }
        k = last + 1;
    }
    // A number given in parts ("078 05 and the rest, um, 1120"): the digits of every run in the
    // utterance that is not explained (money, a measurement or count, a suite or street number, a
    // date or a year; a lone spoken digit word is never counted) are added up; two or more such
    // runs with 9 or more digits together are redacted and hold the call.
    let mut split = false;
    if runs.iter().all(|r| r.2 < 9) {
        // A date anywhere in the utterance ("3/1/2027") gives its years a date meaning.
        let dated = runs.iter().any(|r| date_shaped(text, &toks, r.0, r.1));
        let mut strength: Vec<u8> = runs
            .iter()
            .map(|r| explained_run(text, &toks, r.0, r.1, r.3, dated))
            .collect();
        // A list ("Units 101 and 102", "1,200 and 1,350 feet", "Suites 101, 102 or 103"): runs
        // joined only by separators, "and", "or", "to", "through" share a place word before the
        // first or a unit after the last. A year explained by its cue word alone does not spread.
        let joined = |a: usize, b: usize| {
            let (x, y) = (runs[a].1 + 1, runs[b].0);
            y > x
                && y - x <= 6
                && (x..y).all(|k| match toks[k].2 {
                    4 => &text[toks[k].0..toks[k].1] != ".",
                    3 => matches!(
                        lw(k).as_str(),
                        "and" | "or" | "to" | "through" | "thru" | "plus"
                    ),
                    _ => false,
                })
        };
        for a in 1..runs.len() {
            if strength[a - 1] == 2 && strength[a] < 2 && joined(a - 1, a) {
                strength[a] = 2;
            }
        }
        for a in (1..runs.len()).rev() {
            if strength[a] == 2 && strength[a - 1] < 2 && joined(a - 1, a) {
                strength[a - 1] = 2;
            }
        }
        let parts: Vec<&(usize, usize, usize, bool)> = runs
            .iter()
            .zip(&strength)
            .filter(|(r, s)| !(r.3 && r.1 == r.0) && **s == 0)
            .map(|(r, _)| r)
            .collect();
        if parts.len() >= 2 && parts.iter().map(|r| r.2).sum::<usize>() >= 9 {
            split = true;
            for r in parts {
                for t in redact_tok.iter_mut().take(r.1 + 1).skip(r.0) {
                    *t = true;
                }
            }
        }
    }
    // Once the utterance is sensitive, every digit, every number word, ordinal and month name
    // goes (an expiry after a card number, "April twelfth" after "born", an EIN's first word).
    if keyword.is_some() || ssn_shape || long_run || split {
        let mut j = 0;
        while j < toks.len() {
            let kind = toks[j].2;
            if matches!(kind, 0..=2) {
                redact_tok[j] = true;
            } else if kind == 3 {
                let w = text[toks[j].0..toks[j].1].to_lowercase();
                if ORDINAL_WORDS.contains(&w.as_str()) || MONTHS.contains(&w.as_str()) {
                    redact_tok[j] = true;
                }
            }
            j += 1;
        }
    }
    if keyword.is_some() {
        let mut j = 0;
        while j < toks.len() {
            let kind = toks[j].2;
            if kind == 3 {
                let w = text[toks[j].0..toks[j].1].to_lowercase();
                if matches!(w.as_str(), "password" | "passcode" | "pin") {
                    // The next word, past "is", "it's", ":" and spaces.
                    let mut n = j + 1;
                    while n < toks.len() {
                        let t = &text[toks[n].0..toks[n].1];
                        let tl = t.to_lowercase();
                        if toks[n].2 == 4 || t == ":" || tl == "is" || tl == "it's" {
                            n += 1;
                            continue;
                        }
                        if toks[n].2 != 5 {
                            redact_tok[n] = true;
                        }
                        break;
                    }
                }
            }
            j += 1;
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut any = false;
    let mut k = 0;
    while k < toks.len() {
        if redact_tok[k] {
            any = true;
            out.push_str(REDACTED);
            // Collapse a redacted run with the separators inside it.
            let mut j = k + 1;
            while j < toks.len()
                && (redact_tok[j] || (toks[j].2 == 4 && j + 1 < toks.len() && redact_tok[j + 1]))
            {
                j += 1;
            }
            k = j;
        } else {
            out.push_str(&text[toks[k].0..toks[k].1]);
            k += 1;
        }
    }
    let label = keyword.or_else(|| {
        if ssn_shape {
            Some("a social security number".into())
        } else if long_run {
            Some("an account or card number".into())
        } else if split {
            Some("a long number given in parts".into())
        } else {
            None
        }
    });
    (out, any, label)
}

const ORDINAL_WORDS: [&str; 25] = [
    "first",
    "second",
    "third",
    "fourth",
    "fifth",
    "sixth",
    "seventh",
    "eighth",
    "ninth",
    "tenth",
    "eleventh",
    "twelfth",
    "thirteenth",
    "fourteenth",
    "fifteenth",
    "sixteenth",
    "seventeenth",
    "eighteenth",
    "nineteenth",
    "twentieth",
    "thirtieth",
    "st",
    "nd",
    "rd",
    "th",
];
/// Words after a number that make it a measurement, a count, money or a time.
const UNIT_WORDS: [&str; 75] = [
    "amp",
    "amps",
    "ampere",
    "amperage",
    "volt",
    "volts",
    "v",
    "kw",
    "kva",
    "square",
    "sq",
    "sf",
    "sqft",
    "ft",
    "feet",
    "foot",
    "space",
    "spaces",
    "spot",
    "spots",
    "stall",
    "stalls",
    "parking",
    "unit",
    "units",
    "suite",
    "suites",
    "room",
    "rooms",
    "office",
    "offices",
    "people",
    "employees",
    "cars",
    "percent",
    "day",
    "days",
    "week",
    "weeks",
    "month",
    "months",
    "mo",
    "year",
    "years",
    "yr",
    "yrs",
    "hour",
    "hours",
    "hrs",
    "minutes",
    "k",
    "m",
    "dollar",
    "dollars",
    "bucks",
    "grand",
    "thousand",
    "hundred",
    "million",
    "ton",
    "tons",
    "gallons",
    "inch",
    "inches",
    "stories",
    "floors",
    "bathrooms",
    "restrooms",
    "seats",
    "chairs",
    "sinks",
    "stations",
    "pm",
    "am",
    "o'clock",
];
const STREET_WORDS: [&str; 18] = [
    "street",
    "st",
    "avenue",
    "ave",
    "boulevard",
    "blvd",
    "road",
    "rd",
    "drive",
    "dr",
    "way",
    "lane",
    "ln",
    "place",
    "pl",
    "court",
    "ct",
    "highway",
];
/// Words before a number that make it a suite, unit or lot number (singular and plural).
const PLACE_WORDS: [&str; 28] = [
    "suite",
    "suites",
    "ste",
    "unit",
    "units",
    "apt",
    "apts",
    "apartment",
    "apartments",
    "floor",
    "floors",
    "room",
    "rooms",
    "building",
    "buildings",
    "bldg",
    "lot",
    "lots",
    "space",
    "spaces",
    "office",
    "offices",
    "bay",
    "bays",
    "dock",
    "docks",
    "stall",
    "stalls",
];
/// Words in the four words before a year that give it a date or period meaning.
const YEAR_CUES: [&str; 40] = [
    "in",
    "since",
    "built",
    "renovated",
    "remodeled",
    "updated",
    "through",
    "thru",
    "until",
    "till",
    "from",
    "to",
    "of",
    "by",
    "for",
    "year",
    "lease",
    "leased",
    "term",
    "expires",
    "expiring",
    "expiration",
    "ends",
    "ending",
    "start",
    "starting",
    "starts",
    "begins",
    "beginning",
    "signed",
    "opened",
    "around",
    "circa",
    "before",
    "after",
    "early",
    "late",
    "mid",
    "renew",
    "renewal",
];

/// Whether the run `s..=e` is a written date: m/d, m/d/y (or with `-`), or y-m-d.
fn date_shaped(text: &str, toks: &[(usize, usize, u8)], s: usize, e: usize) -> bool {
    let groups: Vec<&str> = (s..=e)
        .step_by(2)
        .map(|k| &text[toks[k].0..toks[k].1])
        .collect();
    let seps: Vec<&str> = (s + 1..e)
        .step_by(2)
        .map(|k| &text[toks[k].0..toks[k].1])
        .collect();
    if !(2..=3).contains(&groups.len())
        || (s..=e).step_by(2).any(|k| toks[k].2 != 0)
        || !(seps.iter().all(|x| *x == "/") || seps.iter().all(|x| *x == "-"))
    {
        return false;
    }
    let v = |g: &str| g.parse::<u32>().unwrap_or(0);
    let md = |m: &str, d: &str| {
        m.len() <= 2 && d.len() <= 2 && (1..=12).contains(&v(m)) && (1..=31).contains(&v(d))
    };
    match groups.as_slice() {
        [m, d] => md(m, d),
        [m, d, y] if md(m, d) && matches!(y.len(), 2 | 4) => true,
        [y, m, d] => y.len() == 4 && md(m, d),
        _ => false,
    }
}

/// Whether the run `s..=e` of [`redact`]'s tokens is money, a measurement or a count, a suite or
/// street number, a date or a year (so it is not part of a number given in parts): 0 no, 1 a
/// year by its cue word only, 2 otherwise. A year (19xx/20xx written, or spoken "nineteen
/// eighty five", "twenty twenty seven") is explained only when one of the four words before it
/// is a month or a [`YEAR_CUES`] word, or the utterance holds a written date (`dated`); its
/// shape alone explains nothing.
fn explained_run(
    text: &str,
    toks: &[(usize, usize, u8)],
    s: usize,
    e: usize,
    spoken: bool,
    dated: bool,
) -> u8 {
    let lw = |k: usize| text[toks[k].0..toks[k].1].to_lowercase();
    if date_shaped(text, toks, s, e) {
        return 2;
    }
    let year_shape = if spoken {
        matches!(lw(s).as_str(), "nineteen" | "twenty")
            && (s..=e)
                .step_by(2)
                .map(|k| match toks[k].2 {
                    2 => 2,
                    1 if k >= s + 2 && toks[k - 2].2 == 2 && lw(k - 2).ends_with('y') => 0,
                    _ => 1,
                })
                .sum::<usize>()
                == 4
    } else {
        let t = &text[toks[s].0..toks[s].1];
        s == e && t.len() == 4 && (t.starts_with("19") || t.starts_with("20"))
    };
    if year_shape {
        let mut cue = dated;
        let mut k = s;
        let mut words = 0;
        while !cue && k > 0 && words < 4 {
            k -= 1;
            match toks[k].2 {
                4 if &text[toks[k].0..toks[k].1] == "." => break,
                4 => {}
                5 => break,
                3 => {
                    let w = lw(k);
                    cue = YEAR_CUES.contains(&w.as_str()) || MONTHS.contains(&w.as_str());
                    words += 1;
                }
                _ => words += 1,
            }
        }
        if cue {
            return 1;
        }
    }
    // Before: `$`, `#`, a place word or a month name (separators skipped).
    let mut b = s;
    while b > 0 && toks[b - 1].2 == 4 && &text[toks[b - 1].0..toks[b - 1].1] == " " {
        b -= 1;
    }
    if b > 0 {
        let k = b - 1;
        let w = lw(k);
        if matches!(w.as_str(), "$" | "#")
            || (toks[k].2 == 3
                && (PLACE_WORDS.contains(&w.as_str()) || MONTHS.contains(&w.as_str())))
        {
            return 2;
        }
    }
    // After: the next three words.
    let mut after: Vec<String> = Vec::new();
    let mut k = e + 1;
    while k < toks.len() && after.len() < 3 {
        match toks[k].2 {
            4 => {}
            5 => {
                if &text[toks[k].0..toks[k].1] == "%" && after.is_empty() {
                    return 2;
                }
                break;
            }
            _ => after.push(lw(k)),
        }
        k += 1;
    }
    let first = after.first().map(String::as_str).unwrap_or("");
    let second = after.get(1).map(String::as_str).unwrap_or("");
    let explained = UNIT_WORDS.contains(&first)
        || ORDINAL_WORDS.contains(&first)
        || MONTHS.contains(&first)
        || first == "each"
        || (matches!(first, "a" | "an" | "per")
            && matches!(
                second,
                "month" | "year" | "square" | "sq" | "foot" | "mo" | "yr"
            ))
        || after.iter().any(|w| STREET_WORDS.contains(&w.as_str()));
    if explained {
        2
    } else {
        0
    }
}

// ---- Lexer ------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// Lower case.
    Word(String),
    /// A written number: its cents-free value as a decimal mantissa, the digits after the point,
    /// the `k`/`m` multiplier, whether `$` came before.
    Num {
        mantissa: i128,
        frac_len: u32,
        mult: i128,
        dollar: bool,
    },
    /// A clause break (`. ; ! ?` or a line break).
    Break,
    Punct(char),
}

fn mark(out: &[Tok], spans: &mut Vec<(usize, usize)>, start: usize, end: usize) {
    while spans.len() < out.len() {
        spans.push((start, end));
    }
}

/// Tokens with their character spans in the text.
struct Lexed {
    toks: Vec<Tok>,
    spans: Vec<(usize, usize)>,
    chars: Vec<char>,
}

fn lex(text: &str) -> Lexed {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();

    let mut i = 0;
    let mut dollar = false;
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        if c.is_ascii_digit() {
            let mut mantissa: i128 = 0;
            let mut frac_len = 0u32;
            let mut seen_point = false;
            let mut ok = true;
            while i < chars.len() {
                let d = chars[i];
                if d.is_ascii_digit() {
                    if mantissa < 1_000_000_000_000_000 {
                        mantissa = mantissa * 10 + i128::from(d as u8 - b'0');
                    } else {
                        ok = false;
                    }
                    if seen_point {
                        frac_len += 1;
                    }
                    i += 1;
                } else if (d == ',' || (d == '.' && !seen_point))
                    && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit())
                {
                    seen_point |= d == '.';
                    i += 1;
                } else {
                    break;
                }
            }
            let mut mult = 1;
            if let Some(&s) = chars.get(i) {
                let after_ok = chars.get(i + 1).is_none_or(|n| !n.is_alphanumeric());
                if after_ok && matches!(s, 'k' | 'K') {
                    mult = 1_000;
                    i += 1;
                } else if after_ok && matches!(s, 'm' | 'M') {
                    mult = 1_000_000;
                    i += 1;
                }
            }
            if ok && frac_len <= 6 {
                out.push(Tok::Num {
                    mantissa,
                    frac_len,
                    mult,
                    dollar,
                });
            } else {
                out.push(Tok::Punct('#'));
            }
            mark(&out, &mut spans, start, i);
            dollar = false;
            continue;
        }
        dollar = false;
        if c.is_alphanumeric() || c == '\'' {
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '\'')
                && !chars[i].is_ascii_digit()
            {
                i += 1;
            }
            let w: String = chars[start..i].iter().collect::<String>().to_lowercase();
            out.push(Tok::Word(w));
            mark(&out, &mut spans, start, i);
            continue;
        }
        if c == '$' {
            dollar = true;
            out.push(Tok::Punct('$'));
            mark(&out, &mut spans, start, start + 1);
            // A space between `$` and the number is allowed.
            i += 1;
            while i < chars.len() && chars[i] == ' ' {
                i += 1;
            }
            continue;
        }
        if matches!(c, '.' | ';' | '!' | '?' | '\n') {
            out.push(Tok::Break);
        } else if !c.is_whitespace() {
            out.push(Tok::Punct(c));
        }
        mark(&out, &mut spans, start, start + 1);
        i += 1;
    }
    Lexed {
        toks: out,
        spans,
        chars,
    }
}

fn word(t: Option<&Tok>) -> Option<&str> {
    match t {
        Some(Tok::Word(w)) => Some(w.as_str()),
        _ => None,
    }
}

/// The value of one spoken number word, when it is one.
fn spoken_value(w: &str) -> Option<i128> {
    const SMALL: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 8] = [
        "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    if let Some(i) = SMALL.iter().position(|s| *s == w) {
        return Some(i as i128);
    }
    if let Some(i) = TENS.iter().position(|s| *s == w) {
        return Some((i as i128 + 2) * 10);
    }
    match w {
        "hundred" => Some(100),
        "thousand" => Some(1_000),
        "million" => Some(1_000_000),
        _ => None,
    }
}

/// A spoken number starting at `i`: its value and the index past it. "forty thousand" = 40000,
/// "two thousand five hundred" = 2500, "twelve hundred" = 1200, and the colloquial "eighteen
/// fifty" = 1850 (a number from 10 to 99 followed by another from 10 to 99, no multiplier
/// between). "a" counts as one before "hundred" or "thousand". At most 12 words.
fn spoken_number(toks: &[Tok], i: usize) -> Option<(i128, usize)> {
    let mut total: i128 = 0;
    let mut current: i128 = 0;
    let mut j = i;
    let mut any = false;
    let mut last_small: Option<i128> = None;
    while j < toks.len() && j < i + 12 {
        let Some(w) = word(toks.get(j)) else {
            if matches!(toks.get(j), Some(Tok::Punct('-'))) && any {
                j += 1;
                continue;
            }
            break;
        };
        if w == "a" && !any && matches!(word(toks.get(j + 1)), Some("hundred" | "thousand")) {
            current = 1;
            any = true;
            j += 1;
            continue;
        }
        if w == "and" && any && word(toks.get(j + 1)).and_then(spoken_value).is_some() {
            j += 1;
            continue;
        }
        let Some(v) = spoken_value(w) else { break };
        match v {
            100 => {
                current = current.max(1) * 100;
                last_small = None;
            }
            1_000 | 1_000_000 => {
                total += current.max(1) * v;
                current = 0;
                last_small = None;
            }
            _ => {
                if let Some(prev) = last_small {
                    if (10..100).contains(&prev) && v >= 10 && current % 100 == prev {
                        current = current * 100 + v; // "eighteen fifty"
                    } else if prev % 10 == 0 && prev >= 20 && v < 10 {
                        current += v; // "twenty five"
                    } else {
                        break;
                    }
                } else {
                    current += v;
                }
                last_small = Some(v);
            }
        }
        any = true;
        j += 1;
    }
    if any {
        Some((total + current, j))
    } else {
        None
    }
}

// ---- Extraction -------------------------------------------------------------------------------

/// The clause (between breaks) around token `i`.
fn clause(toks: &[Tok], i: usize) -> (usize, usize) {
    let mut s = i;
    while s > 0 && toks[s - 1] != Tok::Break {
        s -= 1;
    }
    let mut e = i;
    while e < toks.len() && toks[e] != Tok::Break {
        e += 1;
    }
    (s, e)
}

/// The clause's own words, verbatim (at most 200 characters, trimmed).
fn clause_text(lx: &Lexed, s: usize, e: usize) -> String {
    if s >= e {
        return String::new();
    }
    let text: String = lx.chars[lx.spans[s].0..lx.spans[e - 1].1].iter().collect();
    text.trim().chars().take(200).collect()
}

fn has_any(toks: &[Tok], s: usize, e: usize, words: &[&str]) -> bool {
    toks[s..e]
        .iter()
        .any(|t| matches!(t, Tok::Word(w) if words.contains(&w.as_str())))
}

/// The nearest token index in the clause holding one of `words`, and its distance.
fn nearest(toks: &[Tok], i: usize, s: usize, e: usize, words: &[&str]) -> Option<(usize, usize)> {
    (s..e)
        .filter(|&k| matches!(&toks[k], Tok::Word(w) if words.contains(&w.as_str())))
        .map(|k| (k.abs_diff(i), k))
        .min()
}

/// The period words right after a money mention: (period, index past them).
fn period_after(toks: &[Tok], j: usize) -> (Option<MoneyPeriod>, usize) {
    let w0 = word(toks.get(j));
    let w1 = word(toks.get(j + 1));
    let w2 = word(toks.get(j + 2));
    let slash = matches!(toks.get(j), Some(Tok::Punct('/')));
    let per = matches!(w0, Some("a" | "per" | "an" | "each")) || slash;
    if per {
        match (w1, w2) {
            (Some("month" | "mo"), _) => return (Some(MoneyPeriod::Monthly), j + 2),
            (Some("year" | "yr"), _) => return (Some(MoneyPeriod::Annual), j + 2),
            (Some("square"), Some("foot" | "feet")) => return (Some(MoneyPeriod::PerSqft), j + 3),
            (Some("sf" | "sq" | "foot"), _) => return (Some(MoneyPeriod::PerSqft), j + 2),
            _ => {}
        }
    }
    match w0 {
        Some("monthly") => (Some(MoneyPeriod::Monthly), j + 1),
        Some("annually" | "yearly" | "annual") => (Some(MoneyPeriod::Annual), j + 1),
        Some("psf") => (Some(MoneyPeriod::PerSqft), j + 1),
        _ => (None, j),
    }
}

const RENT_WORDS: [&str; 6] = ["rent", "rents", "rent's", "asking", "rental", "base"];
/// Words that make a money mention a question or a proposal, not a statement of fact.
const PROPOSAL_WORDS: [&str; 9] = [
    "agreed", "agree", "deal", "accept", "settle", "okay", "ok", "fine", "counter",
];
/// A segment's first word that makes it a question (after "so", "and", "but", "well", "then").
const QUESTION_WORDS: [&str; 22] = [
    "what", "what's", "whats", "how", "when", "where", "who", "which", "why", "do", "does", "did",
    "is", "are", "can", "could", "would", "will", "should", "shall", "wouldn't", "isn't",
];
const RANGE_WORDS: [&str; 4] = ["to", "through", "or", "and"];

/// The comma-delimited part of the clause `s..e` around token `i`.
fn segment(toks: &[Tok], i: usize, s: usize, e: usize) -> (usize, usize) {
    let mut a = i;
    while a > s && toks[a - 1] != Tok::Punct(',') {
        a -= 1;
    }
    let mut b = i;
    while b < e && toks[b] != Tok::Punct(',') {
        b += 1;
    }
    (a, b)
}

/// Whether the money mention at `i` is asked or proposed ("Does $4,600 work for your client?",
/// "So we're agreed at $4,500?", "Would the owner go to $5,000?"), not stated.
fn proposed(toks: &[Tok], i: usize, s: usize, e: usize) -> bool {
    let (a, b) = segment(toks, i, s, e);
    // The segment's first word past leading filler ("Ok the rent is $4,500" is a statement: a
    // leading "ok" is filler, not a proposal word).
    let lead = (a..b)
        .find(|&k| match &toks[k] {
            Tok::Word(w) => !matches!(
                w.as_str(),
                "so" | "and"
                    | "but"
                    | "well"
                    | "then"
                    | "okay"
                    | "ok"
                    | "um"
                    | "uh"
                    | "yeah"
                    | "yes"
                    | "alright"
            ),
            _ => false,
        })
        .unwrap_or(b);
    if word(toks.get(lead)).is_some_and(|w| lead < b && QUESTION_WORDS.contains(&w)) {
        return true;
    }
    if has_any(toks, lead, b, &PROPOSAL_WORDS) {
        return true;
    }
    let pair = |x: &str, ys: &[&str]| {
        (a..b.saturating_sub(1)).any(|k| {
            word(toks.get(k)) == Some(x) && word(toks.get(k + 1)).is_some_and(|y| ys.contains(&y))
        })
    };
    pair("work", &["for"])
        || pair("works", &["for"])
        || pair("how", &["about"])
        || pair("what", &["about"])
        || pair("go", &["to", "up"])
        || pair("come", &["down", "up"])
        || pair("meet", &["at", "in"])
}

/// A number token at `i` (written or spoken): its value in cents, the index past it, `$` before,
/// a `k`/`m` multiplier on it.
fn number_at(toks: &[Tok], i: usize) -> Option<(i128, usize, bool, bool)> {
    match &toks[i] {
        Tok::Num {
            mantissa,
            frac_len,
            mult,
            dollar,
        } => {
            let scale = 10i128.pow(*frac_len);
            let num = mantissa * mult * 100;
            (num % scale == 0).then(|| (num / scale, i + 1, *dollar, *mult > 1))
        }
        Tok::Word(_) => match spoken_number(toks, i) {
            Some((v, j)) if j > i => Some((v * 100, j, false, false)),
            _ => None,
        },
        _ => None,
    }
}
const DEPOSIT_WORDS: [&str; 2] = ["deposit", "security"];
const UTILITY_WORDS: [&str; 2] = ["utilities", "utility"];
const OPEX_WORDS: [&str; 6] = ["cam", "nnn", "triple", "common", "fees", "taxes"];
const IMPROVEMENT_WORDS: [&str; 16] = [
    "retrofit",
    "upgrade",
    "upgrades",
    "repair",
    "repairs",
    "replacement",
    "replace",
    "install",
    "installation",
    "improvement",
    "improvements",
    "buildout",
    "renovation",
    "work",
    "fix",
    "remodel",
];
const SYSTEM_WORDS: [&str; 12] = [
    "plumbing",
    "electrical",
    "electric",
    "hvac",
    "roof",
    "sprinkler",
    "ada",
    "panel",
    "sewer",
    "drain",
    "ventilation",
    "gas",
];
const ALLOWANCE_WORDS: [&str; 2] = ["allowance", "ti"];

/// Money mentions: `$40,000`, `40k`, `$1.2m`, `1,850 dollars`, "a forty thousand dollar plumbing
/// retrofit", "eighteen fifty a month". A number is money when `$` comes before it, a dollar word
/// after it, a `k`/`m` multiplier on it with a cost word in its clause, or a period ("a month",
/// "per square foot") right after it. The purpose is the nearest cost word in the clause.
fn money_findings(lx: &Lexed) -> Vec<AgentFinding> {
    let toks = &lx.toks[..];
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let Some((value_cents, mut next, mut dollar_marked, mut mult_marked)) = number_at(toks, i)
        else {
            i += 1;
            continue;
        };
        let first_end = next;
        let (s, e) = clause(toks, i);
        // A range ("$4,000 to $5,000", "between four and five thousand", "4000-5000"): one
        // finding that carries the whole range and no amount, or nothing.
        let mut value_cents = value_cents;
        let mut range = false;
        {
            let between = (s..i).any(|k| word(toks.get(k)) == Some("between"));
            let mut c = next;
            if matches!(word(toks.get(c)), Some("dollar" | "dollars" | "bucks")) {
                c += 1;
            }
            // `spoken_number` swallows an "and" it cannot continue past.
            let swallowed_and = matches!(toks[i], Tok::Word(_))
                && c > 0
                && word(toks.get(c - 1)) == Some("and")
                && between;
            let conn = if swallowed_and {
                Some(c)
            } else {
                match toks.get(c) {
                    Some(Tok::Punct('-')) => Some(c + 1),
                    Some(Tok::Word(w))
                        if RANGE_WORDS.contains(&w.as_str()) && (w != "and" || between) =>
                    {
                        Some(c + 1)
                    }
                    _ => None,
                }
            };
            if let Some(mut b) = conn {
                if matches!(toks.get(b), Some(Tok::Punct('$'))) {
                    b += 1;
                }
                if b < toks.len() && b < e {
                    if let Some((v2, n2, d2, m2)) = number_at(toks, b) {
                        range = true;
                        value_cents = v2;
                        next = n2;
                        dollar_marked |= d2;
                        mult_marked |= m2;
                    }
                }
            }
        }
        let mut j = next;
        // "four grand" is four thousand dollars.
        let grand = word(toks.get(j)) == Some("grand");
        if grand {
            value_cents = value_cents.saturating_mul(1_000);
        }
        let dollar_word = grand
            || matches!(
                word(toks.get(j)),
                Some("dollar" | "dollars" | "bucks" | "usd")
            );
        if dollar_word {
            j += 1;
        }
        // A spoken "thousand" or "million" counts as a multiplier ("would cost forty thousand"),
        // unless a measurement follows ("forty thousand square feet").
        let measured = matches!(
            word(toks.get(j)),
            Some(
                "square"
                    | "sq"
                    | "sf"
                    | "sqft"
                    | "feet"
                    | "foot"
                    | "ft"
                    | "amps"
                    | "amp"
                    | "people"
                    | "units"
                    | "spaces"
                    | "visitors"
                    | "cars"
                    | "residents"
            )
        );
        if matches!(toks[i], Tok::Word(_))
            && (i..next).any(|k| matches!(word(toks.get(k)), Some("thousand" | "million")))
        {
            mult_marked = true;
        }
        if measured {
            mult_marked = false;
        }
        let (period, after) = period_after(toks, j);
        let cost_context = has_any(toks, s, e, &RENT_WORDS)
            || has_any(toks, s, e, &DEPOSIT_WORDS)
            || has_any(toks, s, e, &UTILITY_WORDS)
            || has_any(toks, s, e, &OPEX_WORDS)
            || has_any(toks, s, e, &IMPROVEMENT_WORDS)
            || has_any(
                toks,
                s,
                e,
                &["cost", "costs", "price", "pay", "run", "runs"],
            );
        let is_money = dollar_marked
            || dollar_word
            || (mult_marked && cost_context)
            || (period.is_some() && value_cents >= 10_000);
        if !is_money || value_cents <= 0 || value_cents > MAX_AMOUNT_CENTS {
            i = if range { first_end } else { next };
            continue;
        }
        // A number the counterparty asks about or proposes is not a claim.
        if proposed(toks, i, s, e) {
            i = after.max(next);
            continue;
        }
        let pick = [
            ("rent", "rent", &RENT_WORDS[..]),
            ("deposit", "security_deposit", &DEPOSIT_WORDS[..]),
            ("utilities", "utilities_cost", &UTILITY_WORDS[..]),
            ("operating_expenses", "operating_expenses", &OPEX_WORDS[..]),
            ("improvement", "improvement_cost", &IMPROVEMENT_WORDS[..]),
            (
                "allowance",
                "tenant_improvement_allowance",
                &ALLOWANCE_WORDS[..],
            ),
        ]
        .iter()
        .filter_map(|(topic, claim, words)| {
            nearest(toks, i, s, e, words).map(|(d, k)| (d, k, *topic, *claim))
        })
        .min();
        let (topic, claim, purpose) = match pick {
            Some((_, k, "improvement", claim)) => {
                let system = nearest(toks, k, s, e, &SYSTEM_WORDS)
                    .and_then(|(_, sk)| word(toks.get(sk)).map(str::to_owned));
                let what = word(toks.get(k)).unwrap_or("work").to_owned();
                (
                    "improvement",
                    claim,
                    Some(match system {
                        Some(sys) => format!("{sys} {what}"),
                        None => what,
                    }),
                )
            }
            Some((_, k, topic, claim)) => (topic, claim, word(toks.get(k)).map(str::to_owned)),
            None => ("cost", "other_cost", None),
        };
        let mut specialists = vec![Specialist::Finance];
        if topic == "improvement" {
            specialists.push(Specialist::Feasibility);
        }
        let range_text = || -> String {
            let end = after.max(next).min(lx.spans.len());
            if end == 0 {
                return String::new();
            }
            let from = if i > 0 && toks[i - 1] == Tok::Punct('$') {
                i - 1
            } else {
                i
            };
            lx.chars[lx.spans[from].0..lx.spans[end - 1].1]
                .iter()
                .collect::<String>()
                .trim()
                .chars()
                .take(200)
                .collect()
        };
        out.push(AgentFinding {
            topic: topic.into(),
            claim_type: claim.into(),
            reported: clause_text(lx, s, e),
            value: if range {
                Value::String(range_text())
            } else {
                Value::from(value_cents as i64)
            },
            unit: if range {
                Some("USD_range".into())
            } else {
                Some("USD_cents".into())
            },
            amount_cents: (!range).then_some(value_cents as i64),
            period,
            purpose,
            reported_by: "counterparty".into(),
            classification: EvidenceClassification::OwnerReported,
            needs_verification: true,
            specialists,
            question_id: None,
            answer_kind: Some(AnswerKind::Stated),
            requirement_id: None,
        });
        i = after.max(next);
    }
    out
}

fn finding(
    topic: &str,
    claim: &str,
    reported: String,
    value: Value,
    unit: Option<&str>,
    specialists: Vec<Specialist>,
) -> AgentFinding {
    AgentFinding {
        topic: topic.into(),
        claim_type: claim.into(),
        reported,
        needs_verification: !value.is_null(),
        value,
        unit: unit.map(str::to_owned),
        amount_cents: None,
        period: None,
        purpose: None,
        reported_by: "counterparty".into(),
        classification: EvidenceClassification::OwnerReported,
        specialists,
        question_id: None,
        answer_kind: Some(AnswerKind::Stated),
        requirement_id: None,
    }
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// Lease term, availability, parking and utilities-included, read from the clauses.
fn term_findings(lx: &Lexed) -> Vec<AgentFinding> {
    let toks = &lx.toks[..];
    let mut out: Vec<AgentFinding> = Vec::new();
    let mut s = 0;
    while s < toks.len() {
        let (_, e) = clause(toks, s);
        let text = clause_text(lx, s, e);
        let low = text.to_lowercase();
        let seen = |out: &Vec<AgentFinding>, c: &str| out.iter().any(|f| f.claim_type == c);
        // Lease term: "<n>-year lease", "a five year term", "36 month lease", "month-to-month".
        if has_any(toks, s, e, &["lease", "term", "terms"]) && !seen(&out, "lease_term_months") {
            let mut months: Option<i128> = None;
            for k in s..e {
                let (n, j) = match &toks[k] {
                    Tok::Num {
                        mantissa,
                        frac_len: 0,
                        mult: 1,
                        dollar: false,
                    } => (*mantissa, k + 1),
                    Tok::Word(w) if w == "a" || w == "one" => (1, k + 1),
                    Tok::Word(_) => match spoken_number(toks, k) {
                        Some((v, j)) => (v, j),
                        None => continue,
                    },
                    _ => continue,
                };
                let j = if matches!(toks.get(j), Some(Tok::Punct('-'))) {
                    j + 1
                } else {
                    j
                };
                match word(toks.get(j)) {
                    Some("year" | "years" | "yr" | "yrs") if (1..=50).contains(&n) => {
                        months = Some(n * 12);
                        break;
                    }
                    Some("month" | "months")
                        if (1..=600).contains(&n)
                            && !(word(toks.get(j + 1)) == Some("to")
                                || n == 1 && word(toks.get(k)) == Some("a")) =>
                    {
                        months = Some(n);
                        break;
                    }
                    _ => {}
                }
            }
            let mtm = low.contains("month-to-month") || low.contains("month to month");
            if let Some(m) = months {
                out.push(finding(
                    "lease_terms",
                    "lease_term_months",
                    text.clone(),
                    Value::from(m as i64),
                    Some("months"),
                    vec![Specialist::Finance, Specialist::Property],
                ));
            } else if mtm {
                out.push(finding(
                    "lease_terms",
                    "lease_term_months",
                    text.clone(),
                    Value::String("month-to-month".into()),
                    None,
                    vec![Specialist::Finance, Specialist::Property],
                ));
            }
        }
        // Availability: a date phrase in a clause about availability or moving in.
        if has_any(
            toks,
            s,
            e,
            &[
                "available",
                "availability",
                "vacant",
                "move",
                "ready",
                "free",
                "open",
            ],
        ) && !seen(&out, AVAILABILITY_CLAIM)
        {
            let mut phrase: Option<String> = None;
            for k in s..e {
                let Some(w) = word(toks.get(k)) else { continue };
                let ordinal = |d: &str| matches!(d, "st" | "nd" | "rd" | "th");
                let dated = matches!(toks.get(k + 1), Some(Tok::Num { .. }))
                    || word(toks.get(k + 1)).is_some_and(|d| {
                        d.ends_with("st")
                            || d.ends_with("nd")
                            || d.ends_with("rd")
                            || d.ends_with("th")
                    });
                let led = k > 0
                    && matches!(
                        word(toks.get(k - 1)),
                        Some(
                            "in" | "by"
                                | "of"
                                | "early"
                                | "mid"
                                | "late"
                                | "end"
                                | "from"
                                | "starting"
                                | "around"
                        )
                    );
                // "may" is a month only with a day after it or a preposition before it.
                if MONTHS.contains(&w) && (w != "may" || dated || led) {
                    let mut p = w.to_owned();
                    if let Some(Tok::Num {
                        mantissa,
                        frac_len: 0,
                        mult: 1,
                        ..
                    }) = toks.get(k + 1)
                    {
                        p.push_str(&format!(" {mantissa}"));
                        if let Some(o) = word(toks.get(k + 2)).filter(|o| ordinal(o)) {
                            p.push_str(o);
                        }
                    } else if let Some(d) = word(toks.get(k + 1)).filter(|d| {
                        d.ends_with("st")
                            || d.ends_with("nd")
                            || d.ends_with("rd")
                            || d.ends_with("th")
                    }) {
                        p.push(' ');
                        p.push_str(d);
                    }
                    phrase = Some(p);
                    break;
                }
                if matches!(w, "immediately" | "now")
                    || (w == "right" && word(toks.get(k + 1)) == Some("away"))
                {
                    phrase = Some(if w == "right" {
                        "right away".into()
                    } else {
                        w.to_owned()
                    });
                    break;
                }
                if w == "next"
                    && matches!(
                        word(toks.get(k + 1)),
                        Some("week" | "month" | "year" | "quarter")
                    )
                {
                    phrase = Some(format!("next {}", word(toks.get(k + 1)).unwrap_or("")));
                    break;
                }
            }
            if let Some(p) = phrase {
                out.push(finding(
                    "availability",
                    AVAILABILITY_CLAIM,
                    text.clone(),
                    Value::String(p),
                    None,
                    vec![Specialist::Property, Specialist::Profile],
                ));
            }
        }
        // Parking.
        if has_any(toks, s, e, &["parking", "spaces", "spots", "stalls"])
            && has_any(toks, s, e, &["parking", "park"])
            && !seen(&out, "parking")
        {
            let mut count: Option<i128> = None;
            for k in s..e {
                let (n, j) = match &toks[k] {
                    Tok::Num {
                        mantissa,
                        frac_len: 0,
                        mult: 1,
                        dollar: false,
                    } => (*mantissa, k + 1),
                    Tok::Word(_) => match spoken_number(toks, k) {
                        Some((v, j)) => (v, j),
                        None => continue,
                    },
                    _ => continue,
                };
                let j = if word(toks.get(j)) == Some("parking") {
                    j + 1
                } else {
                    j
                };
                if matches!(
                    word(toks.get(j)),
                    Some("spaces" | "spots" | "stalls" | "space" | "spot")
                ) && n <= 10_000
                {
                    count = Some(n);
                    break;
                }
            }
            let no_parking = low.contains("no parking")
                || low.contains("isn't any parking")
                || low.contains("there's no")
                || low.contains("no dedicated parking");
            if let Some(n) = count {
                out.push(finding(
                    "parking",
                    "parking_spaces",
                    text.clone(),
                    Value::from(n as i64),
                    Some("spaces"),
                    vec![Specialist::Property],
                ));
            } else if no_parking {
                out.push(finding(
                    "parking",
                    "parking_available",
                    text.clone(),
                    Value::Bool(false),
                    None,
                    vec![Specialist::Property],
                ));
            } else if let Some((_, k)) = nearest(
                toks,
                s,
                s,
                e,
                &[
                    "street",
                    "shared",
                    "free",
                    "validated",
                    "garage",
                    "lot",
                    "structure",
                    "metered",
                ],
            ) {
                out.push(finding(
                    "parking",
                    "parking_description",
                    text.clone(),
                    Value::String(word(toks.get(k)).unwrap_or("").to_owned()),
                    None,
                    vec![Specialist::Property],
                ));
            }
        }
        // Utilities included or not.
        if has_any(toks, s, e, &UTILITY_WORDS) && !seen(&out, "utilities_included") {
            let not = low.contains("not included")
                || low.contains("aren't included")
                || low.contains("isn't included")
                || low.contains("tenant pays")
                || low.contains("separately metered")
                || low.contains("own utilities")
                || low.contains("pay utilities")
                || low.contains("pays utilities")
                || low.contains("pay for utilities");
            let yes = low.contains("included") || low.contains("includes");
            if not {
                out.push(finding(
                    "utilities",
                    "utilities_included",
                    text.clone(),
                    Value::Bool(false),
                    None,
                    vec![Specialist::Finance],
                ));
            } else if yes {
                out.push(finding(
                    "utilities",
                    "utilities_included",
                    text.clone(),
                    Value::Bool(true),
                    None,
                    vec![Specialist::Finance],
                ));
            }
        }
        s = e + 1;
    }
    out
}

fn answer_kind(value: &Value, text: &str) -> AnswerKind {
    match value {
        Value::Bool(true) => AnswerKind::Yes,
        Value::Bool(false) => AnswerKind::No,
        Value::Null if patterns().unsure.is_match(text) => AnswerKind::DontKnow,
        Value::Null => AnswerKind::Unclear,
        _ => AnswerKind::Stated,
    }
}

fn specialists_for_topic(topic: &str, claim: &str) -> Vec<Specialist> {
    match (topic, claim) {
        (_, "usable_area_sqft") => vec![Specialist::Feasibility, Specialist::Property],
        ("infrastructure", _) => vec![Specialist::Feasibility],
        ("availability" | "timing", _) => vec![Specialist::Property, Specialist::Profile],
        ("rent" | "deposit" | "utilities", _) => vec![Specialist::Finance],
        ("lease_terms", _) => vec![Specialist::Finance, Specialist::Property],
        ("business", _) => vec![Specialist::Profile],
        _ => vec![Specialist::Property],
    }
}

/// The answer to the question just asked, through the existing `extract` (unchanged). For a
/// request question (free text) `extract` keeps the answer's first 200 characters; the yes, no or
/// "I don't know" reading then comes from `extract`'s own yes/no reader (asked with the claim
/// type `in_unit_plumbing`) and its hedge pattern.
fn current_answer(q: &PlannedQuestion, text: &str) -> AgentFinding {
    let legacy = extract(&q.as_call_question(), text);
    let kind = if q.source == QuestionSource::Request {
        let yn = extract(
            &CallQuestion {
                question: String::new(),
                claim_type: "in_unit_plumbing".into(),
                requirement_id: None,
            },
            text,
        );
        match (&legacy.value, &yn.value) {
            (Value::Null, _) => answer_kind(&Value::Null, text),
            (_, Value::Bool(b)) => {
                if *b {
                    AnswerKind::Yes
                } else {
                    AnswerKind::No
                }
            }
            _ => AnswerKind::Stated,
        }
    } else if legacy.value.is_null() && !patterns().unsure.is_match(text) {
        // A bank question with no value read: a plain yes or no still answers it.
        let yn = extract(
            &CallQuestion {
                question: String::new(),
                claim_type: "in_unit_plumbing".into(),
                requirement_id: None,
            },
            text,
        );
        match yn.value {
            Value::Bool(true) => AnswerKind::Yes,
            Value::Bool(false) => AnswerKind::No,
            _ => AnswerKind::Unclear,
        }
    } else {
        answer_kind(&legacy.value, text)
    };
    let value = legacy.value.clone();
    AgentFinding {
        topic: q.topic.clone(),
        claim_type: q.claim_type.clone(),
        reported: text.chars().take(200).collect(),
        needs_verification: !value.is_null(),
        value,
        unit: legacy.unit.clone(),
        amount_cents: None,
        period: None,
        purpose: None,
        reported_by: "counterparty".into(),
        classification: EvidenceClassification::OwnerReported,
        specialists: specialists_for_topic(&q.topic, &q.claim_type),
        question_id: Some(q.id.clone()),
        answer_kind: Some(kind),
        requirement_id: q.requirement_id.clone(),
    }
}

/// Amps and usable area stated outside their question, through the existing `extract` with the
/// bank question of that claim type.
fn infrastructure_mentions(text: &str, skip_claim: Option<&str>) -> Vec<AgentFinding> {
    let lower = text.to_lowercase();
    let mut out = Vec::new();
    for (_, question, claim) in QUESTION_BANK {
        if Some(claim) == skip_claim {
            continue;
        }
        let mentioned = match claim {
            "tenant_subpanel_amps" => lower.contains("amp"),
            "usable_area_sqft" => {
                (lower.contains("sq") || lower.contains("square f") || lower.contains(" sf"))
                    && !lower.contains("per square")
                    && !lower.contains("per sq")
            }
            _ => false,
        };
        if !mentioned {
            continue;
        }
        let f = extract(
            &CallQuestion {
                question: question.to_owned(),
                claim_type: claim.to_owned(),
                requirement_id: None,
            },
            text,
        );
        if f.value.is_null() {
            continue;
        }
        let a = finding(
            "infrastructure",
            claim,
            text.chars().take(200).collect(),
            f.value,
            f.unit.as_deref(),
            specialists_for_topic("infrastructure", claim),
        );
        out.push(a);
    }
    out
}

/// Every finding in one utterance (already cut and redacted), at most
/// [`MAX_FINDINGS_PER_TURN`]: the answer to the current question first, then money in order,
/// then terms, then infrastructure mentions; one per claim type (the first wins).
pub fn extract_findings(current: Option<&PlannedQuestion>, text: &str) -> Vec<AgentFinding> {
    let lx = lex(text);
    let mut all: Vec<AgentFinding> = Vec::new();
    if let Some(q) = current {
        all.push(current_answer(q, text));
    }
    all.extend(money_findings(&lx));
    all.extend(term_findings(&lx));
    all.extend(infrastructure_mentions(
        text,
        current.map(|q| q.claim_type.as_str()),
    ));
    let mut out: Vec<AgentFinding> = Vec::new();
    for f in all {
        let dup = out.iter().any(|o| {
            o.claim_type == f.claim_type
                || (o.question_id.is_some()
                    && !o.value.is_null()
                    && o.claim_type == AVAILABILITY_CLAIM
                    && f.claim_type == AVAILABILITY_CLAIM)
        });
        // Money mentions of the same claim type are distinct claims ("$40k plumbing, $8k HVAC").
        if dup && f.amount_cents.is_none() {
            continue;
        }
        if dup
            && out
                .iter()
                .any(|o| o.amount_cents == f.amount_cents && o.claim_type == f.claim_type)
        {
            continue;
        }
        out.push(f);
        if out.len() == MAX_FINDINGS_PER_TURN {
            break;
        }
    }
    out
}

// ---- Escalation -------------------------------------------------------------------------------

fn snippet(text: &str) -> String {
    let t: String = text.chars().take(200).collect();
    format!("The listing contact said: \"{t}\"")
}

/// The first escalation rule that fires, in this order: sensitive data, a commitment, the five
/// legacy rules (`detect_escalation`, unchanged), a concession, a request for the owner, a legal
/// or licensing question, a topic in `may_not`. `text` is cut and redacted; `sensitive` is
/// [`redact`]'s label.
pub fn escalation_for(
    ctx: &CallContext,
    text: &str,
    sensitive: Option<&str>,
) -> Option<Escalation> {
    let p = patterns();
    let esc = |reason, rule, hold: bool, urgency: &str| Escalation {
        reason,
        rule,
        question_for_owner: snippet(text),
        urgency: urgency.into(),
        hold,
        sensitive_handoff: false,
    };
    if let Some(label) = sensitive {
        return Some(Escalation {
            reason: HandoffReason::SensitivePersonalInfo,
            rule: EscalationRule::SensitiveData,
            question_for_owner: format!(
                "The listing contact brought up {label} (not recorded). Can you speak with them \
                 directly?"
            ),
            urgency: "HIGH".into(),
            hold: true,
            sensitive_handoff: true,
        });
    }
    if p.commitment.is_match(text) {
        return Some(esc(
            HandoffReason::BindingCommitment,
            EscalationRule::Commitment,
            true,
            "HIGH",
        ));
    }
    if let Some(reason) = detect_escalation(text) {
        if reason == HandoffReason::SensitivePersonalInfo {
            // Only reachable if the legacy words differ from SENSITIVE_PATTERN; keep it sensitive.
            let mut e = esc(reason, EscalationRule::Legacy, true, "HIGH");
            e.sensitive_handoff = true;
            e.question_for_owner = "The listing contact asked for personal information (not \
                                    recorded). Can you speak with them directly?"
                .into();
            return Some(e);
        }
        return Some(esc(reason, EscalationRule::Legacy, true, "HIGH"));
    }
    if p.concession.is_match(text) {
        return Some(esc(
            HandoffReason::ExceedsAuthority,
            EscalationRule::Concession,
            false,
            "NORMAL",
        ));
    }
    if p.owner_requested.is_match(text) {
        return Some(esc(
            HandoffReason::CounterpartyRequestedOwner,
            EscalationRule::OwnerRequested,
            true,
            "HIGH",
        ));
    }
    if p.legal.is_match(text) {
        return Some(esc(
            HandoffReason::ExceedsAuthority,
            EscalationRule::LegalOrLicensing,
            false,
            "NORMAL",
        ));
    }
    let in_may_not = topics_in(text).iter().any(|topic| {
        ctx.authority
            .may_not
            .iter()
            .any(|m| m == topic || may_not_key(topic) == Some(m.as_str()))
    });
    if in_may_not {
        return Some(esc(
            HandoffReason::ExceedsAuthority,
            EscalationRule::MayNotTopic,
            true,
            "HIGH",
        ));
    }
    None
}

fn is_counterparty_question(text: &str) -> bool {
    text.contains('?') || patterns().interrogative.is_match(text)
}

/// The answer to a counterparty question from the context: the owner's stated requirement for the
/// topic, then a known fact; `None` when the context does not have one.
fn answer_from_context(ctx: &CallContext, text: &str) -> Option<String> {
    // The last sentence holding the question.
    let q = text
        .split(['.', '!', ';'])
        .rev()
        .find(|s| s.contains('?') || patterns().interrogative.is_match(s))
        .unwrap_or(text);
    let topic = topic_of_question(q);
    if let Some(r) = ctx.owner_requirements.iter().find(|r| r.topic == topic) {
        return Some(format!(
            "Tell them, as the owner's stated requirement and not as an offer: {}.",
            r.statement
        ));
    }
    if let Some(f) = ctx.known_facts.iter().find(|f| f.topic == topic) {
        return Some(format!(
            "Tell them what our record says, and that it is {} information, not confirmed: {}.",
            f.classification.as_str().to_lowercase().replace('_', " "),
            f.statement
        ));
    }
    None
}

// ---- The loop ---------------------------------------------------------------------------------

fn allowed(ctx: &CallContext, q: &PlannedQuestion) -> Result<(), String> {
    if let Some(key) = may_not_key(&q.topic) {
        if ctx.authority.may_not.iter().any(|m| m == key) {
            return Err(format!("topic {} is in may_not ({key})", q.topic));
        }
    }
    if ctx.authority.may_not.contains(&q.topic) {
        return Err(format!("topic {} is in may_not", q.topic));
    }
    if !ctx.authority.may_ask_about.contains(&q.topic) {
        return Err(format!("topic {} is not in may_ask_about", q.topic));
    }
    Ok(())
}

/// The next question to ask, marking every unaskable one before it as skipped.
fn take_next(ctx: &CallContext, state: &mut ConversationState) -> Option<PlannedQuestion> {
    for q in &ctx.questions {
        if state.asked.contains(&q.id) || state.skipped.iter().any(|(id, _)| *id == q.id) {
            continue;
        }
        match allowed(ctx, q) {
            Ok(()) => {
                state.asked.push(q.id.clone());
                state.current = Some(q.id.clone());
                return Some(q.clone());
            }
            Err(why) => state.skipped.push((q.id.clone(), why)),
        }
    }
    state.current = None;
    None
}

fn closing(state: &ConversationState) -> String {
    format!(
        "That's everything I needed. Thank you so much. {}Have a great day.",
        if state.owner_followups.is_empty() {
            ""
        } else {
            "The owner will follow up directly on the other items. "
        }
    )
}

fn ask_line(prefix: &str, q: &PlannedQuestion) -> String {
    format!("{prefix}Then ask exactly: \"{}\"", q.text)
}

fn union_specialists(findings: &[AgentFinding]) -> Vec<Specialist> {
    let mut s: Vec<Specialist> = findings
        .iter()
        .flat_map(|f| f.specialists.iter().copied())
        .collect();
    s.sort();
    s.dedup();
    s
}

/// The opening: the first authorised question (turn id 0), or the closing when there is none.
pub fn start(ctx: &CallContext) -> (Decision, ConversationState) {
    let mut state = ConversationState::default();
    let mut d = Decision::silent(0, String::new(), false, false);
    match take_next(ctx, &mut state) {
        Some(q) => {
            d.say = Say::Instruct(ask_line("After the introduction and disclosure, ", &q));
            d.next_question = Some(q);
        }
        None => {
            d.say = Say::Exact(closing(&state));
            d.end_call = true;
            state.ended = true;
        }
    }
    state.decisions.push(d.clone());
    (d, state)
}

/// After a handoff ends and the owner authorised the assistant to resume: re-ask the open
/// question, or the next one, or close.
pub fn resume(
    ctx: &CallContext,
    state: &ConversationState,
    turn_id: u64,
) -> (Decision, ConversationState) {
    if let Some(d) = state.decisions.iter().find(|d| d.turn_id == turn_id) {
        return (d.clone(), state.clone());
    }
    let mut next = state.clone();
    next.awaiting_owner = false;
    let mut d = Decision::silent(turn_id, String::new(), false, false);
    let open = next
        .current
        .as_ref()
        .and_then(|id| ctx.questions.iter().find(|q| q.id == *id).cloned());
    let q = open.or_else(|| take_next(ctx, &mut next));
    match q {
        Some(q) => {
            d.say = Say::Instruct(ask_line(
                "Thank them for waiting; do not repeat the earlier discussion. ",
                &q,
            ));
            d.next_question = Some(q);
        }
        None => {
            d.say = Say::Exact(closing(&next));
            d.end_call = true;
            next.ended = true;
        }
    }
    next.decisions.push(d.clone());
    (d, next)
}

/// How many times one question is put before it is skipped unanswered.
const MAX_ASKS: usize = 2;

/// One turn, one decision. See the module documentation and the report for the order.
pub fn step(
    ctx: &CallContext,
    state: &ConversationState,
    utt: &Utterance,
) -> (Decision, ConversationState) {
    // 1. A turn already decided returns that decision.
    if let Some(d) = state.decisions.iter().find(|d| d.turn_id == utt.turn_id) {
        return (d.clone(), state.clone());
    }
    // 2. Cut, then redact, before anything else reads the text.
    let (cut_text, truncated) = cut(&utt.text);
    let (text, redacted, sensitive) = redact(&cut_text);
    let mut next = state.clone();
    next.turns.push(StoredTurn {
        turn_id: utt.turn_id,
        speaker: utt.speaker,
        text: text.clone(),
    });
    let mut d = Decision::silent(utt.turn_id, text.clone(), redacted, truncated);
    // 3. A stale turn, a turn after the end, or not the counterparty: store it, say nothing.
    let latest = state.decisions.iter().map(|d| d.turn_id).max();
    let stale = latest.is_some_and(|l| utt.turn_id < l);
    if stale || state.ended {
        d.ignored = true;
        next.decisions.push(d.clone());
        return (d, next);
    }
    if utt.speaker != Speaker::Counterparty {
        next.decisions.push(d.clone());
        return (d, next);
    }
    // 4. During a handoff the assistant stays quiet, except that sensitive data is still redacted
    //    (done above).
    if state.awaiting_owner {
        next.decisions.push(d.clone());
        return (d, next);
    }
    let current = state
        .current
        .as_ref()
        .and_then(|id| ctx.questions.iter().find(|q| q.id == *id).cloned());
    // 5. Findings (none from a turn that carried sensitive data: its numbers are redacted).
    let findings = if sensitive.is_some() {
        Vec::new()
    } else {
        extract_findings(current.as_ref(), &text)
    };
    d.specialists = union_specialists(&findings);
    // 6. Escalation.
    let escalation = escalation_for(ctx, &text, sensitive.as_deref());
    // 7. Identity question.
    let identity = escalation.is_none() && patterns().identity.is_match(&text);
    // 8. A question from the counterparty that the context can or cannot answer.
    let asks = escalation.is_none() && !identity && is_counterparty_question(&text);
    let context_answer = if asks {
        answer_from_context(ctx, &text)
    } else {
        None
    };
    let blocking = asks && context_answer.is_none() && patterns().blocking.is_match(&text);
    let escalation = escalation.or_else(|| {
        blocking.then(|| Escalation {
            reason: HandoffReason::InsufficientEvidence,
            rule: EscalationRule::BlockingUnknown,
            question_for_owner: snippet(&text),
            urgency: "HIGH".into(),
            hold: true,
            sensitive_handoff: false,
        })
    });
    // 9. Did the turn answer the open question?
    let current_finding = findings.iter().find(|f| f.question_id.is_some());
    let holds = escalation.as_ref().is_some_and(|e| e.hold);
    // Only an answer closes the open question: a value, yes, no or "I don't know". An unrelated
    // statement ("Rent is $4,500 a month" while the amps question is open) leaves it open.
    let responsive = current_finding.is_some_and(|f| {
        !f.value.is_null()
            || matches!(
                f.answer_kind,
                Some(AnswerKind::DontKnow | AnswerKind::Yes | AnswerKind::No)
            )
    });
    let answered = current.is_some()
        && responsive
        && !holds
        && !identity
        && !(asks
            && current_finding.is_none_or(|f| {
                // A free-text request question keeps any answer as its value, so only a yes or a
                // no counts as answering it when the counterparty asks something back.
                f.value.is_null()
                    || (current
                        .as_ref()
                        .is_some_and(|q| q.source == QuestionSource::Request)
                        && f.answer_kind == Some(AnswerKind::Stated))
            }));
    let mut findings = findings;
    if !answered {
        // The open question stays open; its tentative answer is not a finding.
        findings.retain(|f| f.question_id.is_none());
    }
    d.specialists = union_specialists(&findings);
    if answered {
        let q = current
            .as_ref()
            .expect("answered implies a current question");
        next.answered.push(q.id.clone());
        next.current = None;
        d.answered_question = Some(q.id.clone());
    }
    next.findings.extend(findings.iter().cloned());
    d.findings = findings;
    // 10. What to say.
    if let Some(e) = &escalation {
        if e.hold {
            d.say = Say::Exact(
                match e.rule {
                    EscalationRule::SensitiveData => SENSITIVE_LINE,
                    EscalationRule::Legacy if e.sensitive_handoff => SENSITIVE_LINE,
                    EscalationRule::OwnerRequested => OWNER_REQUESTED_LINE,
                    EscalationRule::MayNotTopic => OUT_OF_SCOPE_LINE,
                    EscalationRule::BlockingUnknown => BLOCKING_LINE,
                    _ => HOLD_LINE,
                }
                .to_owned(),
            );
            next.awaiting_owner = true;
            d.escalation = escalation;
            next.decisions.push(d.clone());
            return (d, next);
        }
        next.owner_followups.push(e.question_for_owner.clone());
    }
    let lead: Option<String> = match &escalation {
        Some(e) if e.rule == EscalationRule::Concession => Some(CONCESSION_LINE.into()),
        Some(_) => Some(LEGAL_LINE.into()),
        None if identity => Some(IDENTITY_LINE.into()),
        None if asks && context_answer.is_none() => {
            let f = snippet(&text);
            next.owner_followups.push(f.clone());
            d.owner_followup = Some(f);
            Some(DONT_KNOW_LINE.into())
        }
        None => None,
    };
    d.escalation = escalation;
    // The bound: a question that has been put twice (the first ask and one re-ask) and is still
    // not answered is skipped, so the call never loops on it.
    let put = |id: &str| {
        state
            .decisions
            .iter()
            .filter(|x| x.next_question.as_ref().is_some_and(|q| q.id == id))
            .count()
    };
    let exhausted = !answered && current.as_ref().is_some_and(|q| put(&q.id) >= MAX_ASKS);
    if exhausted {
        if let Some(q) = &current {
            next.skipped
                .push((q.id.clone(), "not answered after being asked twice".into()));
            next.current = None;
        }
    }
    let q = if answered || current.is_none() || exhausted {
        take_next(ctx, &mut next)
    } else {
        current.clone()
    };
    match (q, lead, context_answer) {
        (Some(q), Some(lead), _) => {
            d.say = Say::Exact(format!("{lead} {}", q.text));
            d.next_question = Some(q);
        }
        (Some(q), None, Some(answer)) => {
            d.say = Say::Instruct(ask_line(&format!("{answer} "), &q));
            d.next_question = Some(q);
        }
        (Some(q), None, None) => {
            d.say = Say::Instruct(ask_line(
                "Briefly acknowledge the answer without agreeing to any amount or term. ",
                &q,
            ));
            d.next_question = Some(q);
        }
        (None, lead, answer) => {
            let close = closing(&next);
            d.say = match (lead, answer) {
                (Some(l), _) => Say::Exact(format!("{l} {close}")),
                (None, Some(a)) => Say::Instruct(format!("{a} Then say exactly: \"{close}\"")),
                (None, None) => Say::Exact(close),
            };
            d.end_call = true;
            next.ended = true;
        }
    }
    next.decisions.push(d.clone());
    (d, next)
}
