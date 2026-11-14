//! Port of `apps/api/app/integrations/xai_voice/__init__.py`: the xAI realtime voice
//! (speech to speech) `session.update` payload and the provider status. STATUS: NOT_STARTED for
//! live audio; this module defines the session config builder only.

use serde_json::{json, Value};
use ss_core::IntegrationStatus;

use crate::contracts::communications::CallSession;
use crate::contracts::ProviderStatusEntry;
use crate::integrations::base::status_entry;

/// `REALTIME_URL` (`{call_id}` is replaced by the xAI call id).
pub const REALTIME_URL: &str = "wss://api.x.ai/v1/realtime?call_id={call_id}";

/// `build_session_update(call, property_summary, questions)`: the `session.update` payload. The
/// instructions encode the automation disclosure and the authority limits; tool calls go back to
/// the backend (`escalate_to_owner`, `record_finding`).
pub fn build_session_update<S: AsRef<str>>(
    call: &CallSession,
    property_summary: &str,
    questions: &[S],
) -> Value {
    let scope = &call.authority_scope;
    let instructions = format!(
        "{} You are the business owner's assistant. You are not a human employee and must say so if asked. Ask only about: {}. Never: {} \u{2014} call escalate_to_owner instead. Property: {}. Questions: {}",
        call.automation_disclosure,
        scope.may_ask_about.join(", "),
        scope.may_not.join(", "),
        property_summary,
        questions
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<&str>>()
            .join(" | "),
    );
    json!({
        "type": "session.update",
        "session": {
            "instructions": instructions,
            "tools": [
                {
                    "type": "function",
                    "name": "escalate_to_owner",
                    "parameters": {
                        "type": "object",
                        "properties": {"reason": {"type": "string"}, "question": {"type": "string"}},
                        "required": ["reason", "question"],
                    },
                },
                {
                    "type": "function",
                    "name": "record_finding",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string"},
                            "answer": {"type": "string"},
                            "claim_type": {"type": "string"},
                        },
                        "required": ["question", "answer"],
                    },
                },
            ],
        },
    })
}

/// `status(has_key)`: mode `fixture`, `NOT_STARTED`.
pub fn status(has_key: bool) -> ProviderStatusEntry {
    status_entry(
        "xai_voice",
        "fixture",
        IntegrationStatus::NotStarted,
        has_key,
        "Session config builder only; realtime audio bridge not implemented.",
    )
}
