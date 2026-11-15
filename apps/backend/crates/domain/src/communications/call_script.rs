//! Port of `apps/api/app/domains/communications/call_script.py`: what the assistant asks a
//! broker, and how answers become structured findings.
//!
//! Questions are grounded in the property's open feasibility assessments (UNKNOWN /
//! MAY_REQUIRE_WORK), so every call exists to resolve a specific unknown. Extraction is
//! deterministic and conservative: an answer that does not clearly state a value yields a finding
//! with `value = null` ("unclear"), never a guessed number. Every finding is
//! `classification = "OWNER_REPORTED"` and `speaker = "counterparty"`: what a broker says stays
//! owner- or broker-reported evidence.

use std::sync::OnceLock;

use serde_json::Value;

use crate::contracts::communications::{CallQuestion, StructuredFinding};
use crate::contracts::infrastructure::{AssessmentStatus, InfrastructureAssessment};
use crate::py::{self, PyRegex};

/// `(requirement suffix, question)` in Python's order: `QUESTION_BANK`.
pub const QUESTION_BANK: [(&str, &str, &str); 4] = [
    (
        "electrical.tenant_service",
        "How many amps is the electrical service to the suite's own panel, and roughly how much is in use?",
        "tenant_subpanel_amps",
    ),
    (
        "plumbing.treatment_sinks",
        "Is there existing plumbing inside the suite, like a sink or a floor drain?",
        "in_unit_plumbing",
    ),
    (
        "hvac.ventilation",
        "Could the HVAC be zoned so that separate small rooms each get supply and return air?",
        "hvac_zonable",
    ),
    (
        "space.min_area",
        "What is the usable square footage of the suite?",
        "usable_area_sqft",
    ),
];

/// `AVAILABILITY.question` / `.claim_type`.
pub const AVAILABILITY_QUESTION: &str = "When would the space be available to start a lease?";
pub const AVAILABILITY_CLAIM: &str = "available_from";

/// `MAX_QUESTIONS`.
pub const MAX_QUESTIONS: usize = 4;

/// `AVAILABILITY`: the question every script ends with.
pub fn availability() -> CallQuestion {
    CallQuestion {
        question: AVAILABILITY_QUESTION.to_owned(),
        claim_type: AVAILABILITY_CLAIM.to_owned(),
        requirement_id: None,
    }
}

/// Python's patterns, verbatim (all compiled with `re.I`).
pub const YES_PATTERN: &str = r"\b(yes|yeah|yep|there is|there's|it has|it does|we have|definitely|absolutely|correct|sure)\b";
pub const NO_PATTERN: &str =
    r"\b(no|nope|none|not really|there isn'?t|doesn'?t|don'?t have|it's all open|no plumbing)\b";
pub const UNSURE_PATTERN: &str =
    r"\b(not sure|don'?t know|no idea|would have to check|have to check|unsure|i'?ll find out)\b";
pub const AMPS_PATTERN: &str = r"(\d{2,4})\s*(?:-|\s)?(?:amps?|amperes?|a\b)";
pub const SQFT_PATTERN: &str =
    r"(\d[\d,]{2,6})\s*(?:usable\s*)?(?:sq(?:uare)?\.?\s*(?:ft|feet)|sf\b)";

struct Patterns {
    yes: PyRegex,
    no: PyRegex,
    unsure: PyRegex,
    amps: PyRegex,
    sqft: PyRegex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        yes: PyRegex::new(YES_PATTERN, true),
        no: PyRegex::new(NO_PATTERN, true),
        unsure: PyRegex::new(UNSURE_PATTERN, true),
        amps: PyRegex::new(AMPS_PATTERN, true),
        sqft: PyRegex::new(SQFT_PATTERN, true),
    })
}

/// `build_script(assessments)`: the bank questions whose requirement suffix (the part after the
/// first `.`) belongs to an UNKNOWN or MAY_REQUIRE_WORK assessment, in bank order, at most
/// `MAX_QUESTIONS - 1` of them, each carrying that assessment's `requirement_id`, then
/// [`availability`]. When several open assessments share a suffix, the last one wins (Python
/// builds a dict).
pub fn build_script(assessments: &[InfrastructureAssessment]) -> Vec<CallQuestion> {
    let mut open: Vec<(&str, &str)> = Vec::new();
    for a in assessments {
        if !matches!(
            a.status,
            AssessmentStatus::Unknown | AssessmentStatus::MayRequireWork
        ) {
            continue;
        }
        let Some((_, suffix)) = a.requirement_id.split_once('.') else {
            continue;
        };
        match open.iter_mut().find(|(s, _)| *s == suffix) {
            Some(entry) => entry.1 = &a.requirement_id,
            None => open.push((suffix, &a.requirement_id)),
        }
    }
    let mut script: Vec<CallQuestion> = QUESTION_BANK
        .iter()
        .filter_map(|(suffix, question, claim)| {
            open.iter()
                .find(|(s, _)| s == suffix)
                .map(|(_, rid)| CallQuestion {
                    question: (*question).to_owned(),
                    claim_type: (*claim).to_owned(),
                    requirement_id: Some((*rid).to_owned()),
                })
        })
        .take(MAX_QUESTIONS - 1)
        .collect();
    script.push(availability());
    script
}

/// `extract(question, answer)`: one finding, value `null` unless the answer clearly states one.
///
/// - Any hedge ("not sure", "would have to check", ...) leaves the value unclear.
/// - `tenant_subpanel_amps`: the first `NN[NN] amps`-style number, as an integer, unit `A`.
/// - `usable_area_sqft`: the first `N,NNN sq ft`-style number (commas dropped), unit `sqft`.
/// - `in_unit_plumbing`, `hvac_zonable`: `true` or `false` only when exactly one of the yes and
///   no patterns matches.
/// - any other claim: the stripped answer's first 200 characters, or `null` when empty.
pub fn extract(question: &CallQuestion, answer: &str) -> StructuredFinding {
    let p = patterns();
    let mut value = Value::Null;
    let mut unit: Option<String> = None;
    if !p.unsure.is_match(answer) {
        match question.claim_type.as_str() {
            "tenant_subpanel_amps" => {
                if let Some(n) = p
                    .amps
                    .search(answer)
                    .and_then(|m| m.group(1))
                    .and_then(|g| py::int_of_str(g).ok())
                {
                    value = Value::from(n);
                    unit = Some("A".into());
                }
            }
            "usable_area_sqft" => {
                if let Some(n) = p
                    .sqft
                    .search(answer)
                    .and_then(|m| m.group(1))
                    .and_then(|g| py::int_of_str(&g.replace(',', "")).ok())
                {
                    value = Value::from(n);
                    unit = Some("sqft".into());
                }
            }
            "in_unit_plumbing" | "hvac_zonable" => {
                let (yes, no) = (p.yes.is_match(answer), p.no.is_match(answer));
                if yes != no {
                    value = Value::Bool(yes);
                }
            }
            _ => {
                let s = py::prefix_chars(py::strip(answer), 200);
                if !s.is_empty() {
                    value = Value::String(s.to_owned());
                }
            }
        }
    }
    StructuredFinding {
        question: question.question.clone(),
        answer: Some(answer.to_owned()),
        claim_type: Some(question.claim_type.clone()),
        value,
        unit,
        speaker: Some("counterparty".into()),
        transcript_offset_s: None,
        classification: "OWNER_REPORTED".into(),
    }
}

/// `describe(finding)`: `"<claim_type>: unclear"`, `": yes"` / `": no"` for booleans, otherwise
/// `str(value)` followed by `" <unit>"` when the unit is non-empty. A missing claim type prints
/// as `None`, as in Python.
pub fn describe(f: &StructuredFinding) -> String {
    let claim = f.claim_type.as_deref().unwrap_or("None");
    match &f.value {
        Value::Null => format!("{claim}: unclear"),
        Value::Bool(b) => format!("{claim}: {}", if *b { "yes" } else { "no" }),
        v => {
            let unit = match f.unit.as_deref() {
                Some(u) if !u.is_empty() => format!(" {u}"),
                _ => String::new(),
            };
            format!("{claim}: {}{unit}", py::value_str(v))
        }
    }
}
