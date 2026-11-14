//! Contract tests: JSON Schema conformance of every public type of this crate against
//! `packages/contracts/schemas/`, enum values identical to Python, and the state-machine and
//! event-payload rules of `apps/api/tests/test_contracts.py`.

mod common;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{json, Value};
use ss_core::{Contract, IntegrationStatus, Market, ProjectContext, UtcDateTime};
use ss_server::contracts::*;
use ss_server::error::{AppError, ProblemDetail};
use uuid::Uuid;

use common::*;

fn v<T: Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap()
}

fn obj(v: Value) -> JsonMap {
    match v {
        Value::Object(m) => m,
        _ => panic!(),
    }
}

fn u() -> Uuid {
    Uuid::new_v4()
}

fn approval() -> ApprovalRequest {
    ApprovalRequest::new(NewApproval {
        project_id: u(),
        requested_by_agent: "t".into(),
        action_type: ActionType::Purchase,
        action_payload: obj(json!({"sku": "x", "qty": 1})),
        risk_category: RiskCategory::Consequential,
        summary: "s".into(),
        expires_at: UtcDateTime::from(chrono::Utc::now() + chrono::Duration::hours(1)),
        authorized_owner_id: u(),
        idempotency_key: "k".into(),
    })
}

fn task() -> TaskRecord {
    TaskRecord::new(
        u(),
        TaskType::AnalyzeProperty,
        Subsystem::AgentsFinance,
        JsonMap::new(),
    )
}

fn sample_payload(et: EventType) -> Value {
    let id = || Value::String(u().to_string());
    match et.payload_contract() {
        "ProfileConfirmedPayload" => json!({"profile_id": id()}),
        "PropertySearchCompletedPayload" => {
            json!({"property_ids": [id()], "provider": "fixture", "data_origin": "FIXTURE_SYNTHETIC"})
        }
        "PropertyRefPayload" => json!({"property_id": id()}),
        "PermitsIngestedPayload" => {
            json!({"property_id": id(), "permit_record_ids": [id()], "evidence_ids": [], "source_dataset_ids": ["ysqd-apz7"]})
        }
        "AnalysisCompletedPayload" => json!({"property_id": id(), "assessment_ids": [id()]}),
        "UnknownDetectedPayload" => {
            json!({"property_id": id(), "requirement_ids": ["r"], "verification_questions": ["q?"]})
        }
        "EvidenceUpdatedPayload" => {
            json!({"property_id": id(), "evidence_ids": [id()], "reason": "r"})
        }
        "ScenarioUpdatedPayload" => {
            json!({"property_id": id(), "scenario_id": id(), "long_term_cost": 123456, "comparison_horizon_months": 36})
        }
        "RankingUpdatedPayload" => {
            json!({"ranking_id": id(), "ranking_version": 2, "top_property_id": null})
        }
        "OutreachPayload" => json!({"communication_id": id(), "approval_id": id()}),
        "CallPayload" => json!({"call_id": id(), "state": "DIALING"}),
        "HandoffRequestedPayload" => {
            json!({"call_id": id(), "handoff_request_id": id(), "reason": "FINANCIAL_DETAILS"})
        }
        "FindingPayload" => json!({"call_id": id(), "claim_type": "electrical"}),
        "MessageReceivedPayload" => {
            json!({"channel": "imessage", "provider_message_id": "m1", "handled_as": "chat"})
        }
        "OwnerNotifiedPayload" => {
            json!({"communication_id": id(), "delivery": "recorded", "purpose": "handoff_request"})
        }
        "LayoutPayload" => {
            json!({"design_version_id": null, "property_id": id(), "valid": false, "issue_codes": ["TOO_SMALL"]})
        }
        "BomPayload" => json!({"bom_id": id(), "design_version_id": id(), "estimated_total": 1000}),
        "OfferPayload" => json!({"offer_id": id(), "bom_item_id": id()}),
        "TimelinePayload" => json!({"milestone": "m"}),
        "ApprovalPayload" => {
            json!({"approval_id": id(), "action_type": "PURCHASE", "approval_status": "PENDING"})
        }
        "TaskPayload" => json!({"task_id": id(), "task_type": "x", "status": "RUNNING"}),
        other => panic!("{other}"),
    }
}

fn event(et: EventType, payload: Value) -> Result<ActivityEvent, ss_core::ValidationError> {
    ActivityEvent::new(NewEvent {
        event_type: et,
        project_id: u(),
        correlation_id: u(),
        causation_id: None,
        idempotency_key: "k".into(),
        actor_type: ActorType::System,
        actor_id: "x".into(),
        payload: obj(payload),
    })
}

#[test]
fn every_public_type_conforms_to_its_schema() {
    let a = approval();
    assert_conforms("ApprovalRequest", &v(&a));
    let mut decided = a.transition(ApprovalStatus::Approved).unwrap();
    decided.decision_timestamp = Some(UtcDateTime::now());
    decided.decided_by = Some(u());
    assert_conforms("ApprovalRequest", &v(&decided));
    assert_conforms(
        "ApprovalDecision",
        &v(&ApprovalDecision {
            decision: ApprovalStatus::Approved,
            action_payload_hash: "h".into(),
            note: Some("n".into()),
        }),
    );
    let mut t = task();
    assert_conforms("TaskRecord", &v(&t));
    t.error_code = Some("NotFoundError".into());
    t.dependencies = vec![u()];
    assert_conforms("TaskRecord", &v(&t));
    assert_conforms(
        "TaskAccepted",
        &v(&TaskAccepted {
            task: t.clone(),
            status_url: "/api/v1/projects/x/tasks/y".into(),
        }),
    );
    assert_conforms(
        "AgentTaskRequest",
        &v(&AgentTaskRequest {
            task_id: u(),
            project_id: u(),
            task_type: TaskType::GenerateLayout,
            source_record_ids: vec![u()],
            input_schema_version: "1.0".into(),
            expected_output_schema: "DesignVersion".into(),
            constraints: obj(json!({"max": 3})),
            correlation_id: u(),
        }),
    );
    assert_conforms(
        "AgentTaskResult",
        &v(&AgentTaskResult {
            task_id: u(),
            status: AgentTaskStatus::NeedsApproval,
            output_record_ids: vec![],
            evidence_ids: vec![u()],
            unresolved_questions: vec!["q".into()],
            errors: vec![],
            completed_at: UtcDateTime::now(),
        }),
    );
    for et in EventType::ALL {
        let ev = event(*et, sample_payload(*et)).unwrap();
        assert_conforms("ActivityEvent", &v(&ev));
        assert_conforms(et.payload_contract(), &Value::Object(ev.payload.clone()));
    }
    let ev = event(
        EventType::TaskStarted,
        sample_payload(EventType::TaskStarted),
    )
    .unwrap();
    assert_conforms_openapi(
        "EventPage",
        &v(&EventPage {
            events: vec![ev],
            next_cursor: 7,
        }),
    );
    for e in [
        AppError::not_found("x"),
        AppError::internal("y"),
        AppError::conflict("z")
            .code("STALE_VERSION")
            .details(json!({"expected": 1})),
    ] {
        let p: ProblemDetail = e.to_problem_detail();
        assert_conforms("ProblemDetail", &v(&p));
    }
    assert_conforms(
        "CreateProjectRequest",
        &v(&CreateProjectRequest {
            project_name: "x".into(),
            market: Market::new("LA", "CA"),
            business_profile: Some(obj(json!({"business_type": "spa"}))),
        }),
    );
    let project = ProjectContext::new(u(), "p", Market::new("LA", "CA"));
    assert_conforms(
        "ProjectEnvelope",
        &v(&ProjectEnvelope {
            project,
            business_profile: None,
        }),
    );
    assert_conforms(
        "ChatRequest",
        &v(&ChatRequest {
            message: "hi".into(),
        }),
    );
    assert_conforms(
        "ChatResponse",
        &v(&ChatResponse {
            reply: "r".into(),
            task_ids: vec![u()],
            mode: "deterministic".into(),
            grounded_record_ids: vec![],
        }),
    );
    let entry = ProviderStatusEntry {
        provider: "costar".into(),
        mode: "fixture".into(),
        status: IntegrationStatus::NotStarted,
        credentials_present: false,
        note: "n".into(),
    };
    assert_conforms("ProviderStatusEntry", &v(&entry));
    assert_conforms(
        "HealthResponse",
        &v(&HealthResponse {
            status: "ok".into(),
            version: "0.1.0".into(),
        }),
    );
    let mut checks = IndexMap::new();
    checks.insert("database".to_owned(), "ok".to_owned());
    assert_conforms(
        "ReadyResponse",
        &v(&ReadyResponse {
            status: "ready".into(),
            checks,
            providers: vec![entry],
        }),
    );
    assert_conforms(
        "DevTokenResponse",
        &v(&DevTokenResponse {
            user_id: u(),
            access_token: "t".into(),
            token_type: "bearer".into(),
        }),
    );
    // The check has teeth.
    let mut bad = v(&approval());
    bad["approval_status"] = json!("MAYBE");
    assert_rejected("ApprovalRequest", &bad);
}

fn schema_enum(schema: &str, def: &str) -> Vec<String> {
    read_json(&format!("packages/contracts/schemas/{schema}.json"))["$defs"][def]["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("{schema}.{def}"))
        .iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

fn names<T: Copy + AsRef<str>>(all: &[T]) -> Vec<String> {
    all.iter().map(|x| x.as_ref().to_owned()).collect()
}

#[test]
fn enum_values_are_identical_to_python() {
    assert_eq!(
        names(EventType::ALL),
        schema_enum("ActivityEvent", "EventType")
    );
    assert_eq!(
        names(ActorType::ALL),
        schema_enum("ActivityEvent", "ActorType")
    );
    assert_eq!(
        names(ActionType::ALL),
        schema_enum("ApprovalRequest", "ActionType")
    );
    assert_eq!(
        names(ApprovalStatus::ALL),
        schema_enum("ApprovalRequest", "ApprovalStatus")
    );
    assert_eq!(
        names(RiskCategory::ALL),
        schema_enum("ApprovalRequest", "RiskCategory")
    );
    assert_eq!(
        names(TaskStatus::ALL),
        schema_enum("TaskRecord", "TaskStatus")
    );
    assert_eq!(names(TaskType::ALL), schema_enum("TaskRecord", "TaskType"));
    assert_eq!(
        names(Subsystem::ALL),
        schema_enum("TaskRecord", "Subsystem")
    );
    assert_eq!(
        names(AgentTaskStatus::ALL),
        schema_enum("AgentTaskResult", "AgentTaskStatus")
    );
}

#[test]
fn every_event_type_has_a_payload_model() {
    assert_eq!(EventType::ALL.len(), 34);
    for et in EventType::ALL {
        let name = et.payload_contract();
        assert!(
            repo_root()
                .join(format!("packages/contracts/schemas/{name}.json"))
                .exists(),
            "{name}"
        );
        assert!(
            parse_payload(*et, &obj(sample_payload(*et))).is_ok(),
            "{et}"
        );
    }
}

#[test]
fn test_approval_state_machine_and_payload_binding() {
    let a = approval();
    assert_eq!(a.action_payload_hash.len(), 64);
    assert_eq!(a.action_payload_hash, payload_hash(&a.action_payload));
    let b = a
        .transition(ApprovalStatus::Approved)
        .unwrap()
        .transition(ApprovalStatus::Executed)
        .unwrap();
    assert_eq!(b.approval_status, ApprovalStatus::Executed);
    assert!(b.transition(ApprovalStatus::Approved).is_err());
    let e = a
        .transition(ApprovalStatus::Rejected)
        .unwrap()
        .transition(ApprovalStatus::Approved)
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "approval: transition REJECTED -> APPROVED is not allowed"
    );
    for terminal in [
        ApprovalStatus::Rejected,
        ApprovalStatus::Expired,
        ApprovalStatus::Cancelled,
        ApprovalStatus::Executed,
    ] {
        for target in ApprovalStatus::ALL {
            assert!(APPROVAL_TRANSITIONS
                .iter()
                .any(|(s, to)| *s == terminal && to.is_empty()));
            let mut x = a.clone();
            x.approval_status = terminal;
            assert!(x.transition(*target).is_err());
        }
    }
    // Expiry is allowed from PENDING and APPROVED only.
    assert!(a.transition(ApprovalStatus::Expired).is_ok());
    assert!(a
        .transition(ApprovalStatus::Approved)
        .unwrap()
        .transition(ApprovalStatus::Expired)
        .is_ok());
    assert!(
        a.transition(ApprovalStatus::Executed).is_err(),
        "cannot execute without approval"
    );
}

#[test]
fn test_task_state_machine() {
    let t = task();
    let e = t.transition(TaskStatus::Completed).unwrap_err();
    assert_eq!(
        e.to_string(),
        "task: transition QUEUED -> COMPLETED is not allowed"
    );
    let back = t
        .transition(TaskStatus::Running)
        .unwrap()
        .transition(TaskStatus::Failed)
        .unwrap()
        .transition(TaskStatus::Queued)
        .unwrap();
    assert_eq!(back.status, TaskStatus::Queued);
    let running = t.transition(TaskStatus::Running).unwrap();
    assert!(running.updated_at >= t.updated_at);
    assert!(running
        .transition(TaskStatus::WaitingApproval)
        .unwrap()
        .transition(TaskStatus::Running)
        .is_ok());
    assert!(running
        .transition(TaskStatus::Completed)
        .unwrap()
        .transition(TaskStatus::Queued)
        .is_err());
    let mut bad = v(&t);
    bad["attempt_count"] = json!(-1);
    let err = TaskRecord::from_json_value(bad).unwrap_err();
    assert_eq!(err.errors[0].kind, "greater_than_equal");
}

#[test]
fn test_event_envelope_validates_payload_and_forbids_secrets() {
    let ev = event(
        EventType::TaskStarted,
        json!({"task_id": u().to_string(), "task_type": "x", "status": "RUNNING"}),
    )
    .unwrap();
    assert_eq!(ev.schema_version, "1.0");
    let round = ActivityEvent::from_json_str(&serde_json::to_string(&ev).unwrap()).unwrap();
    assert_eq!(round.event_id, ev.event_id);
    assert_eq!(round, ev);
    assert!(event(EventType::TaskStarted, json!({"wrong": 1})).is_err());
    let e = event(EventType::CallOwnerHandoffRequested, json!({"call_id": u().to_string(), "handoff_request_id": u().to_string(), "reason": "X", "join_token": "t"})).unwrap_err();
    assert!(e.mentions("tokens or secrets"), "{e}");
    for key in ["token", "api_key", "secret", "password"] {
        let mut p = sample_payload(EventType::LogisticsTimelineUpdated);
        p[key] = json!("x");
        assert!(event(EventType::LogisticsTimelineUpdated, p)
            .unwrap_err()
            .mentions("tokens or secrets"));
    }
    // The same rules apply when an envelope is read back.
    let mut stored = v(&ev);
    stored["payload"]["secret"] = json!("s");
    let err = ActivityEvent::from_json_value(stored).unwrap_err();
    assert!(err.mentions("tokens or secrets"), "{err}");
    let mut stored = v(&ev);
    stored["payload"] = json!({"wrong": 1});
    assert!(ActivityEvent::from_json_value(stored).is_err());
    // Normalization: defaults filled in, ids normalized like Pydantic's dump.
    let id = u();
    let ev = event(
        EventType::PropertyIdentityResolved,
        json!({"property_id": id.simple().to_string()}),
    )
    .unwrap();
    assert_eq!(
        Value::Object(ev.payload.clone()),
        json!({"property_id": id.to_string(), "summary": null})
    );
    // `X | None` without a default is required (present, may be null).
    assert!(event(
        EventType::PropertyRankingUpdated,
        json!({"ranking_id": u().to_string(), "ranking_version": 1})
    )
    .is_err());
    assert!(event(
        EventType::LayoutApproved,
        json!({"property_id": u().to_string(), "valid": true})
    )
    .is_err());
    // Key length limits.
    let mut long = NewEvent {
        event_type: EventType::LogisticsTimelineUpdated,
        project_id: u(),
        correlation_id: u(),
        causation_id: None,
        idempotency_key: "k".repeat(256),
        actor_type: ActorType::System,
        actor_id: "x".into(),
        payload: obj(json!({"milestone": "m"})),
    };
    assert!(ActivityEvent::new(long.clone()).is_err());
    long.idempotency_key = String::new();
    assert!(ActivityEvent::new(long.clone()).is_err());
    long.idempotency_key = "é".repeat(255);
    assert!(ActivityEvent::new(long).is_ok(), "length counts characters");
    // The typed payload is available to subscribers.
    let ev = event(
        EventType::FinancialScenarioUpdated,
        sample_payload(EventType::FinancialScenarioUpdated),
    )
    .unwrap();
    match ev.typed_payload().unwrap() {
        EventPayload::ScenarioUpdated(p) => assert_eq!(p.long_term_cost, 123456),
        other => panic!("{other:?}"),
    }
}

#[test]
fn api_request_contracts_validate_like_pydantic() {
    let ok = CreateProjectRequest::from_json_value(
        json!({"project_name": "x", "market": {"city": "LA", "state": "CA"}}),
    )
    .unwrap();
    assert!(ok.business_profile.is_none());
    for bad in [
        json!({"project_name": "", "market": {"city": "LA", "state": "CA"}}),
        json!({"project_name": "x".repeat(201), "market": {"city": "LA", "state": "CA"}}),
        json!({"project_name": "x", "market": {"city": "LA", "state": "C"}}),
        json!({"project_name": "x", "market": {"city": "LA", "state": "CA"}, "extra": 1}),
    ] {
        assert!(CreateProjectRequest::from_json_value(bad).is_err());
    }
    assert!(ApprovalDecision::from_json_value(json!({"decision": "APPROVED"})).is_err());
    assert!(ChatRequest::from_json_value(json!({"message": ""})).is_err());
    let d =
        DevTokenResponse::from_json_value(json!({"user_id": u().to_string(), "access_token": "t"}))
            .unwrap();
    assert_eq!(d.token_type, "bearer");
}
