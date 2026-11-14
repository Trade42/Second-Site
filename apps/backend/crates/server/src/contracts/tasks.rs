//! Port of `apps/api/app/contracts/tasks.py`.

use serde::{Deserialize, Serialize};
use ss_core::{
    check_transition, new_id, string_enum, utcnow, Checker, Contract, InvalidStateTransition,
    TransitionTable, UtcDateTime, Validate, SCHEMA_VERSION,
};
use uuid::Uuid;

use super::{plain_contract, JsonMap};

string_enum! {
    #[derive(Default)]
    pub enum TaskStatus {
        #[default]
        Queued = "QUEUED",
        Running = "RUNNING",
        WaitingApproval = "WAITING_APPROVAL",
        Completed = "COMPLETED",
        Failed = "FAILED",
        Cancelled = "CANCELLED",
    }
}

/// `TASK_TRANSITIONS`.
pub const TASK_TRANSITIONS: &TransitionTable<TaskStatus> = &[
    (
        TaskStatus::Queued,
        &[TaskStatus::Running, TaskStatus::Cancelled],
    ),
    (
        TaskStatus::Running,
        &[
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::WaitingApproval,
            TaskStatus::Cancelled,
        ],
    ),
    (
        TaskStatus::WaitingApproval,
        &[
            TaskStatus::Running,
            TaskStatus::Cancelled,
            TaskStatus::Failed,
        ],
    ),
    // retry
    (TaskStatus::Failed, &[TaskStatus::Queued]),
    (TaskStatus::Completed, &[]),
    (TaskStatus::Cancelled, &[]),
];

string_enum! {
    pub enum TaskType {
        ConfirmRequirements = "confirm_requirements",
        SearchProperties = "search_properties",
        ResolveIdentity = "resolve_identity",
        IngestPermits = "ingest_permits",
        AnalyzeProperty = "analyze_property",
        EvaluateInfrastructure = "evaluate_infrastructure",
        CalculateCosts = "calculate_costs",
        RankProperties = "rank_properties",
        GenerateLayout = "generate_layout",
        ReviseLayout = "revise_layout",
        GenerateBom = "generate_bom",
        Outreach = "outreach",
        PlaceCall = "place_call",
        SupervisorChat = "supervisor_chat",
    }
}

string_enum! {
    pub enum Subsystem {
        AgentsFinance = "agents-finance",
        PropertySpatial = "property-spatial",
        FrontendProduct = "frontend-product",
        Communications = "communications",
        Foundation = "foundation",
    }
}

fn default_task_status() -> TaskStatus {
    TaskStatus::Queued
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRecord {
    #[serde(default = "new_id")]
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub task_type: TaskType,
    #[serde(default = "default_task_status")]
    pub status: TaskStatus,
    pub assigned_subsystem: Subsystem,
    #[serde(default)]
    pub input_reference: JsonMap,
    #[serde(default)]
    pub output_reference: JsonMap,
    #[serde(default)]
    pub dependencies: Vec<Uuid>,
    /// `>= 0`.
    #[serde(default)]
    pub attempt_count: i64,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default = "new_id")]
    pub correlation_id: Uuid,
    #[serde(default = "utcnow")]
    pub created_at: UtcDateTime,
    #[serde(default = "utcnow")]
    pub updated_at: UtcDateTime,
}

impl TaskRecord {
    pub fn new(
        project_id: Uuid,
        task_type: TaskType,
        assigned_subsystem: Subsystem,
        input_reference: JsonMap,
    ) -> Self {
        let now = utcnow();
        TaskRecord {
            task_id: new_id(),
            project_id,
            task_type,
            status: TaskStatus::Queued,
            assigned_subsystem,
            input_reference,
            output_reference: JsonMap::new(),
            dependencies: Vec::new(),
            attempt_count: 0,
            error_code: None,
            error_message: None,
            correlation_id: new_id(),
            created_at: now,
            updated_at: now,
        }
    }

    /// `TaskRecord.transition(target)`: also bumps `updated_at`.
    pub fn transition(&self, target: TaskStatus) -> Result<Self, InvalidStateTransition> {
        check_transition("task", TASK_TRANSITIONS, self.status, target)?;
        let mut next = self.clone();
        next.status = target;
        next.updated_at = utcnow();
        Ok(next)
    }
}

impl Validate for TaskRecord {
    fn check(&self, c: &mut Checker) {
        c.ge("attempt_count", self.attempt_count, 0);
    }
}

impl Contract for TaskRecord {
    const NAME: &'static str = "TaskRecord";
}

fn schema_version() -> String {
    SCHEMA_VERSION.to_owned()
}

/// Structured delegation from the supervisor to a specialist. Free text is NOT the contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTaskRequest {
    #[serde(default = "new_id")]
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub task_type: TaskType,
    #[serde(default)]
    pub source_record_ids: Vec<Uuid>,
    #[serde(default = "schema_version")]
    pub input_schema_version: String,
    /// Contract name, e.g. 'InfrastructureAssessment'.
    pub expected_output_schema: String,
    #[serde(default)]
    pub constraints: JsonMap,
    #[serde(default = "new_id")]
    pub correlation_id: Uuid,
}

string_enum! {
    pub enum AgentTaskStatus {
        Completed = "COMPLETED",
        Partial = "PARTIAL",
        Failed = "FAILED",
        NeedsApproval = "NEEDS_APPROVAL",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTaskResult {
    pub task_id: Uuid,
    pub status: AgentTaskStatus,
    #[serde(default)]
    pub output_record_ids: Vec<Uuid>,
    #[serde(default)]
    pub evidence_ids: Vec<Uuid>,
    #[serde(default)]
    pub unresolved_questions: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default = "utcnow")]
    pub completed_at: UtcDateTime,
}

plain_contract!(AgentTaskRequest, AgentTaskResult);
