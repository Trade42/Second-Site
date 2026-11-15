//! Ports of `apps/api/app/domains/communications/{call_script,escalation}.py` and the pure part of
//! `apps/api/app/services/call_orchestration/__init__.py` (`call_payload`). The orchestrator,
//! the owner channel, inbound routing and the routes belong to the server.

pub mod call_orchestration;
pub mod call_script;
pub mod call_summary;
pub mod escalation;
pub mod response_agent;

pub use call_orchestration::call_payload;
pub use call_script::{availability, build_script, describe, extract};
pub use escalation::detect_escalation;
