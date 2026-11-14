//! Port of `apps/api/app/domains/communications/escalation.py`: when the assistant must hand off
//! to the owner (deterministic keyword rules). The voice model may also call
//! `escalate_to_owner`; this function is the server-side backstop.

use std::sync::OnceLock;

use crate::contracts::communications::HandoffReason;
use crate::py::PyRegex;

/// `RULES`, in Python's order (the first match wins); every pattern is compiled with `re.I`.
pub const RULES: [(HandoffReason, &str); 5] = [
    (HandoffReason::GuarantorRequested, r"\bguarant(or|ee)\b"),
    (
        HandoffReason::FinancialDetails,
        r"\b(deposit|first month|wire|payment|bank|credit (check|score))\b",
    ),
    (
        HandoffReason::BindingCommitment,
        r"\b(sign|commit|lock (it|this) in|agree to|LOI|letter of intent)\b",
    ),
    (
        HandoffReason::SensitivePersonalInfo,
        r"\b(ssn|social security|date of birth|driver'?s license)\b",
    ),
    (
        HandoffReason::CounterpartyRequestedOwner,
        r"\b(speak|talk) (to|with) (the )?(owner|you(r)? (boss|client))\b",
    ),
];

fn compiled() -> &'static [(HandoffReason, PyRegex)] {
    static R: OnceLock<Vec<(HandoffReason, PyRegex)>> = OnceLock::new();
    R.get_or_init(|| {
        RULES
            .iter()
            .map(|(reason, pat)| (*reason, PyRegex::new(pat, true)))
            .collect()
    })
}

/// `detect_escalation(utterance)`: the reason of the first rule whose pattern occurs in the
/// utterance, or `None`.
pub fn detect_escalation(utterance: &str) -> Option<HandoffReason> {
    compiled()
        .iter()
        .find(|(_, re)| re.is_match(utterance))
        .map(|(reason, _)| *reason)
}
