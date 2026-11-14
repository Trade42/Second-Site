//! Port of `apps/api/app/contracts/api.py`: HTTP request/response shapes that are not domain
//! records. `ProblemDetail` lives in [`crate::error`] and is re-exported here.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use ss_core::{BusinessExpansionProfile, Checker, Contract, Market, ProjectContext, Validate};
use uuid::Uuid;

use super::{plain_contract, JsonMap, TaskRecord};
pub use crate::error::ProblemDetail;
/// One definition per contract: `ProviderStatusEntry` is `ss-domain`'s (provider `status()`).
pub use ss_domain::contracts::ProviderStatusEntry;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProjectRequest {
    /// 1..=200 characters.
    pub project_name: String,
    pub market: Market,
    /// Optional BusinessExpansionProfile fields (project_id/profile_id assigned).
    #[serde(default)]
    pub business_profile: Option<JsonMap>,
}

impl Validate for CreateProjectRequest {
    fn check(&self, c: &mut Checker) {
        c.min_len("project_name", &self.project_name, 1);
        c.max_len("project_name", &self.project_name, 200);
        c.nested("market", &self.market);
    }
}

impl Contract for CreateProjectRequest {
    const NAME: &'static str = "CreateProjectRequest";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectEnvelope {
    pub project: ProjectContext,
    #[serde(default)]
    pub business_profile: Option<BusinessExpansionProfile>,
}

impl Validate for ProjectEnvelope {
    fn check(&self, c: &mut Checker) {
        c.nested("project", &self.project);
        c.nested_opt("business_profile", &self.business_profile);
    }
}

impl Contract for ProjectEnvelope {
    const NAME: &'static str = "ProjectEnvelope";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRequest {
    /// 1..=8000 characters.
    pub message: String,
}

impl Validate for ChatRequest {
    fn check(&self, c: &mut Checker) {
        c.min_len("message", &self.message, 1);
        c.max_len("message", &self.message, 8000);
    }
}

impl Contract for ChatRequest {
    const NAME: &'static str = "ChatRequest";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatResponse {
    pub reply: String,
    #[serde(default)]
    pub task_ids: Vec<Uuid>,
    /// 'deep_agent' (live model) or 'deterministic' (no model configured).
    pub mode: String,
    #[serde(default)]
    pub grounded_record_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAccepted {
    pub task: TaskRecord,
    pub status_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyResponse {
    pub status: String,
    pub checks: IndexMap<String, String>,
    pub providers: Vec<ProviderStatusEntry>,
}

fn bearer() -> String {
    "bearer".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevTokenResponse {
    pub user_id: Uuid,
    pub access_token: String,
    #[serde(default = "bearer")]
    pub token_type: String,
}

plain_contract!(
    ChatResponse,
    TaskAccepted,
    HealthResponse,
    ReadyResponse,
    DevTokenResponse,
);

fn web() -> String {
    "web".into()
}

/// `RevisionBody` (`api/routes/design.py`): the body of `POST .../layouts/{id}/revisions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionBody {
    /// At least one entry.
    pub requested_changes: Vec<String>,
    #[serde(default)]
    pub room_requirement_overrides: Vec<ss_domain::contracts::RoomRequirement>,
    #[serde(default)]
    pub preserved_constraints: Vec<String>,
    #[serde(default = "web")]
    pub source_channel: String,
}

impl Validate for RevisionBody {
    fn check(&self, c: &mut Checker) {
        if self.requested_changes.is_empty() {
            c.push(
                Some("requested_changes"),
                "too_short",
                "List should have at least 1 item after validation, not 0",
            );
        }
        c.each(
            "room_requirement_overrides",
            &self.room_requirement_overrides,
        );
    }
}

impl Contract for RevisionBody {
    const NAME: &'static str = "RevisionBody";
}
