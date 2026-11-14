//! Port of `apps/api/app/contracts/events.py`: the `ActivityEvent` envelope and one versioned
//! payload contract per event type.
//!
//! Constructing or deserializing an [`ActivityEvent`] applies Python's `_validate_payload`: the
//! payload may not carry a top-level `join_token` / `token` / `api_key` / `secret` / `password`
//! key, it must validate against the payload contract of its event type, and it is replaced by
//! that contract's normalized dump (defaults filled in, ids normalized).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ss_core::{
    new_id, string_enum, utcnow, Checker, Contract, UtcDateTime, Validate, ValidationError,
    SCHEMA_VERSION,
};
use uuid::Uuid;

use super::{nullable, plain_contract, JsonMap};

string_enum! {
    pub enum EventType {
        ProjectProfileConfirmed = "project.profile.confirmed",
        PropertySearchCompleted = "property.search.completed",
        PropertyIdentityResolved = "property.identity.resolved",
        PropertyPermitsIngested = "property.permits.ingested",
        PropertySpatialAnalysisCompleted = "property.spatial_analysis.completed",
        PropertyAnalysisCompleted = "property.analysis.completed",
        InfrastructureUnknownDetected = "infrastructure.unknown_detected",
        InfrastructureEvidenceUpdated = "infrastructure.evidence_updated",
        FinancialScenarioUpdated = "financial.scenario.updated",
        PropertyRankingUpdated = "property.ranking.updated",
        OutreachApprovalRequested = "outreach.approval_requested",
        OutreachApproved = "outreach.approved",
        OutreachCompleted = "outreach.completed",
        CallStarted = "call.started",
        CallOwnerHandoffRequested = "call.owner_handoff_requested",
        CallOwnerJoined = "call.owner_joined",
        CallCompleted = "call.completed",
        CallFailed = "call.failed",
        CallFindingRecorded = "call.finding_recorded",
        CommunicationMessageReceived = "communication.message_received",
        CommunicationOwnerNotified = "communication.owner_notified",
        LayoutVersionCreated = "layout.version_created",
        LayoutValidationFailed = "layout.validation_failed",
        LayoutApproved = "layout.approved",
        BomGenerated = "bom.generated",
        BomRecalculationRequired = "bom.recalculation_required",
        BomApproved = "bom.approved",
        ProcurementOfferUpdated = "procurement.offer_updated",
        LogisticsTimelineUpdated = "logistics.timeline_updated",
        ApprovalRequested = "approval.requested",
        ApprovalDecided = "approval.decided",
        TaskStarted = "task.started",
        TaskCompleted = "task.completed",
        TaskFailed = "task.failed",
    }
}

string_enum! {
    pub enum ActorType {
        Agent = "agent",
        User = "user",
        System = "system",
        Provider = "provider",
    }
}

// ---- payloads (v1.0) ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfirmedPayload {
    pub profile_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertySearchCompletedPayload {
    pub property_ids: Vec<Uuid>,
    pub provider: String,
    pub data_origin: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyRefPayload {
    pub property_id: Uuid,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitsIngestedPayload {
    pub property_id: Uuid,
    pub permit_record_ids: Vec<Uuid>,
    pub evidence_ids: Vec<Uuid>,
    pub source_dataset_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisCompletedPayload {
    pub property_id: Uuid,
    pub assessment_ids: Vec<Uuid>,
    #[serde(default)]
    pub unknown_requirement_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnknownDetectedPayload {
    pub property_id: Uuid,
    pub requirement_ids: Vec<String>,
    pub verification_questions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceUpdatedPayload {
    pub property_id: Uuid,
    pub evidence_ids: Vec<Uuid>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioUpdatedPayload {
    pub property_id: Uuid,
    pub scenario_id: Uuid,
    pub long_term_cost: i64,
    pub comparison_horizon_months: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankingUpdatedPayload {
    pub ranking_id: Uuid,
    pub ranking_version: i64,
    #[serde(deserialize_with = "nullable")]
    pub top_property_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachPayload {
    pub communication_id: Uuid,
    #[serde(default)]
    pub property_id: Option<Uuid>,
    #[serde(default)]
    pub approval_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallPayload {
    pub call_id: Uuid,
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Never include join tokens here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRequestedPayload {
    pub call_id: Uuid,
    pub handoff_request_id: Uuid,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindingPayload {
    pub call_id: Uuid,
    pub claim_type: String,
    #[serde(default)]
    pub evidence_id: Option<Uuid>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageReceivedPayload {
    pub channel: String,
    pub provider_message_id: String,
    /// 'linked' | 'command' | 'chat' | 'image' | 'ignored'.
    pub handled_as: String,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerNotifiedPayload {
    pub communication_id: Uuid,
    /// 'accepted' (provider took it) | 'recorded' (fixture) | 'not_linked'.
    pub delivery: String,
    pub purpose: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutPayload {
    #[serde(deserialize_with = "nullable")]
    pub design_version_id: Option<Uuid>,
    pub property_id: Uuid,
    pub valid: bool,
    #[serde(default)]
    pub issue_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BomPayload {
    pub bom_id: Uuid,
    pub design_version_id: Uuid,
    #[serde(default)]
    pub estimated_total: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfferPayload {
    pub offer_id: Uuid,
    pub bom_item_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimelinePayload {
    pub milestone: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalPayload {
    pub approval_id: Uuid,
    pub action_type: String,
    pub approval_status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPayload {
    pub task_id: Uuid,
    pub task_type: String,
    pub status: String,
    #[serde(default)]
    pub error_code: Option<String>,
}

plain_contract!(
    ProfileConfirmedPayload,
    PropertySearchCompletedPayload,
    PropertyRefPayload,
    PermitsIngestedPayload,
    AnalysisCompletedPayload,
    UnknownDetectedPayload,
    EvidenceUpdatedPayload,
    ScenarioUpdatedPayload,
    RankingUpdatedPayload,
    OutreachPayload,
    CallPayload,
    HandoffRequestedPayload,
    FindingPayload,
    MessageReceivedPayload,
    OwnerNotifiedPayload,
    LayoutPayload,
    BomPayload,
    OfferPayload,
    TimelinePayload,
    ApprovalPayload,
    TaskPayload,
);

/// A payload parsed into its contract (`ActivityEvent.typed_payload()`).
#[derive(Debug, Clone, PartialEq)]
pub enum EventPayload {
    ProfileConfirmed(ProfileConfirmedPayload),
    PropertySearchCompleted(PropertySearchCompletedPayload),
    PropertyRef(PropertyRefPayload),
    PermitsIngested(PermitsIngestedPayload),
    AnalysisCompleted(AnalysisCompletedPayload),
    UnknownDetected(UnknownDetectedPayload),
    EvidenceUpdated(EvidenceUpdatedPayload),
    ScenarioUpdated(ScenarioUpdatedPayload),
    RankingUpdated(RankingUpdatedPayload),
    Outreach(OutreachPayload),
    Call(CallPayload),
    HandoffRequested(HandoffRequestedPayload),
    Finding(FindingPayload),
    MessageReceived(MessageReceivedPayload),
    OwnerNotified(OwnerNotifiedPayload),
    Layout(LayoutPayload),
    Bom(BomPayload),
    Offer(OfferPayload),
    Timeline(TimelinePayload),
    Approval(ApprovalPayload),
    Task(TaskPayload),
}

impl EventType {
    /// The payload contract name (`EVENT_PAYLOADS[event_type].__name__`).
    pub fn payload_contract(&self) -> &'static str {
        use EventType::*;
        match self {
            ProjectProfileConfirmed => "ProfileConfirmedPayload",
            PropertySearchCompleted => "PropertySearchCompletedPayload",
            PropertyIdentityResolved | PropertySpatialAnalysisCompleted => "PropertyRefPayload",
            PropertyPermitsIngested => "PermitsIngestedPayload",
            PropertyAnalysisCompleted => "AnalysisCompletedPayload",
            InfrastructureUnknownDetected => "UnknownDetectedPayload",
            InfrastructureEvidenceUpdated => "EvidenceUpdatedPayload",
            FinancialScenarioUpdated => "ScenarioUpdatedPayload",
            PropertyRankingUpdated => "RankingUpdatedPayload",
            OutreachApprovalRequested | OutreachApproved | OutreachCompleted => "OutreachPayload",
            CallStarted | CallOwnerJoined | CallCompleted | CallFailed => "CallPayload",
            CallOwnerHandoffRequested => "HandoffRequestedPayload",
            CallFindingRecorded => "FindingPayload",
            CommunicationMessageReceived => "MessageReceivedPayload",
            CommunicationOwnerNotified => "OwnerNotifiedPayload",
            LayoutVersionCreated | LayoutValidationFailed | LayoutApproved => "LayoutPayload",
            BomGenerated | BomRecalculationRequired | BomApproved => "BomPayload",
            ProcurementOfferUpdated => "OfferPayload",
            LogisticsTimelineUpdated => "TimelinePayload",
            ApprovalRequested | ApprovalDecided => "ApprovalPayload",
            TaskStarted | TaskCompleted | TaskFailed => "TaskPayload",
        }
    }
}

fn parse<T: Contract>(v: &JsonMap) -> Result<T, ValidationError> {
    T::from_json_value(Value::Object(v.clone()))
}

/// Validate `payload` against the contract of `event_type`.
pub fn parse_payload(
    event_type: EventType,
    payload: &JsonMap,
) -> Result<EventPayload, ValidationError> {
    use EventType::*;
    Ok(match event_type {
        ProjectProfileConfirmed => EventPayload::ProfileConfirmed(parse(payload)?),
        PropertySearchCompleted => EventPayload::PropertySearchCompleted(parse(payload)?),
        PropertyIdentityResolved | PropertySpatialAnalysisCompleted => {
            EventPayload::PropertyRef(parse(payload)?)
        }
        PropertyPermitsIngested => EventPayload::PermitsIngested(parse(payload)?),
        PropertyAnalysisCompleted => EventPayload::AnalysisCompleted(parse(payload)?),
        InfrastructureUnknownDetected => EventPayload::UnknownDetected(parse(payload)?),
        InfrastructureEvidenceUpdated => EventPayload::EvidenceUpdated(parse(payload)?),
        FinancialScenarioUpdated => EventPayload::ScenarioUpdated(parse(payload)?),
        PropertyRankingUpdated => EventPayload::RankingUpdated(parse(payload)?),
        OutreachApprovalRequested | OutreachApproved | OutreachCompleted => {
            EventPayload::Outreach(parse(payload)?)
        }
        CallStarted | CallOwnerJoined | CallCompleted | CallFailed => {
            EventPayload::Call(parse(payload)?)
        }
        CallOwnerHandoffRequested => EventPayload::HandoffRequested(parse(payload)?),
        CallFindingRecorded => EventPayload::Finding(parse(payload)?),
        CommunicationMessageReceived => EventPayload::MessageReceived(parse(payload)?),
        CommunicationOwnerNotified => EventPayload::OwnerNotified(parse(payload)?),
        LayoutVersionCreated | LayoutValidationFailed | LayoutApproved => {
            EventPayload::Layout(parse(payload)?)
        }
        BomGenerated | BomRecalculationRequired | BomApproved => EventPayload::Bom(parse(payload)?),
        ProcurementOfferUpdated => EventPayload::Offer(parse(payload)?),
        LogisticsTimelineUpdated => EventPayload::Timeline(parse(payload)?),
        ApprovalRequested | ApprovalDecided => EventPayload::Approval(parse(payload)?),
        TaskStarted | TaskCompleted | TaskFailed => EventPayload::Task(parse(payload)?),
    })
}

impl EventPayload {
    /// `model_dump(mode="json")` of the payload contract.
    pub fn to_map(&self) -> JsonMap {
        fn dump<T: Serialize>(t: &T) -> JsonMap {
            match serde_json::to_value(t) {
                Ok(Value::Object(m)) => m,
                _ => JsonMap::new(),
            }
        }
        match self {
            EventPayload::ProfileConfirmed(p) => dump(p),
            EventPayload::PropertySearchCompleted(p) => dump(p),
            EventPayload::PropertyRef(p) => dump(p),
            EventPayload::PermitsIngested(p) => dump(p),
            EventPayload::AnalysisCompleted(p) => dump(p),
            EventPayload::UnknownDetected(p) => dump(p),
            EventPayload::EvidenceUpdated(p) => dump(p),
            EventPayload::ScenarioUpdated(p) => dump(p),
            EventPayload::RankingUpdated(p) => dump(p),
            EventPayload::Outreach(p) => dump(p),
            EventPayload::Call(p) => dump(p),
            EventPayload::HandoffRequested(p) => dump(p),
            EventPayload::Finding(p) => dump(p),
            EventPayload::MessageReceived(p) => dump(p),
            EventPayload::OwnerNotified(p) => dump(p),
            EventPayload::Layout(p) => dump(p),
            EventPayload::Bom(p) => dump(p),
            EventPayload::Offer(p) => dump(p),
            EventPayload::Timeline(p) => dump(p),
            EventPayload::Approval(p) => dump(p),
            EventPayload::Task(p) => dump(p),
        }
    }
}

/// `_FORBIDDEN_PAYLOAD_KEYS`.
pub const FORBIDDEN_PAYLOAD_KEYS: [&str; 5] =
    ["join_token", "token", "api_key", "secret", "password"];
pub const SECRET_PAYLOAD_MESSAGE: &str = "event payloads must not carry tokens or secrets";

/// `_validate_payload`: the normalized payload, or the model-level error Pydantic raises.
pub fn normalize_payload(event_type: EventType, payload: &JsonMap) -> Result<JsonMap, String> {
    if FORBIDDEN_PAYLOAD_KEYS
        .iter()
        .any(|k| payload.contains_key(*k))
    {
        return Err(SECRET_PAYLOAD_MESSAGE.to_owned());
    }
    parse_payload(event_type, payload)
        .map(|p| p.to_map())
        .map_err(|e| e.to_string())
}

fn schema_version() -> String {
    SCHEMA_VERSION.to_owned()
}

/// `ActivityEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ActivityEventWire")]
pub struct ActivityEvent {
    pub schema_version: String,
    pub event_id: Uuid,
    pub event_type: EventType,
    pub project_id: Uuid,
    pub correlation_id: Uuid,
    pub causation_id: Option<Uuid>,
    /// 1..=255 characters.
    pub idempotency_key: String,
    pub actor_type: ActorType,
    pub actor_id: String,
    pub occurred_at: UtcDateTime,
    pub payload: JsonMap,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityEventWire {
    #[serde(default = "schema_version")]
    schema_version: String,
    #[serde(default = "new_id")]
    event_id: Uuid,
    event_type: EventType,
    project_id: Uuid,
    correlation_id: Uuid,
    #[serde(default)]
    causation_id: Option<Uuid>,
    idempotency_key: String,
    actor_type: ActorType,
    actor_id: String,
    #[serde(default = "utcnow")]
    occurred_at: UtcDateTime,
    payload: JsonMap,
}

fn key_errors(key: &str) -> Option<String> {
    let n = key.chars().count();
    if n < 1 {
        Some("idempotency_key: String should have at least 1 character".into())
    } else if n > 255 {
        Some("idempotency_key: String should have at most 255 characters".into())
    } else {
        None
    }
}

impl TryFrom<ActivityEventWire> for ActivityEvent {
    type Error = String;

    fn try_from(w: ActivityEventWire) -> Result<Self, Self::Error> {
        if let Some(e) = key_errors(&w.idempotency_key) {
            return Err(e);
        }
        let payload =
            normalize_payload(w.event_type, &w.payload).map_err(|e| format!("Value error, {e}"))?;
        Ok(ActivityEvent {
            schema_version: w.schema_version,
            event_id: w.event_id,
            event_type: w.event_type,
            project_id: w.project_id,
            correlation_id: w.correlation_id,
            causation_id: w.causation_id,
            idempotency_key: w.idempotency_key,
            actor_type: w.actor_type,
            actor_id: w.actor_id,
            occurred_at: w.occurred_at,
            payload,
        })
    }
}

/// Fields of a new event; the rest take their Python defaults.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub event_type: EventType,
    pub project_id: Uuid,
    pub correlation_id: Uuid,
    pub causation_id: Option<Uuid>,
    pub idempotency_key: String,
    pub actor_type: ActorType,
    pub actor_id: String,
    pub payload: JsonMap,
}

impl ActivityEvent {
    /// Construct and validate (`ActivityEvent(...)`).
    pub fn new(n: NewEvent) -> Result<Self, ValidationError> {
        let mut c = Checker::new();
        c.min_len("idempotency_key", &n.idempotency_key, 1);
        c.max_len("idempotency_key", &n.idempotency_key, 255);
        c.finish("ActivityEvent")?;
        let payload = normalize_payload(n.event_type, &n.payload)
            .map_err(|e| ValidationError::value_error("ActivityEvent", e))?;
        Ok(ActivityEvent {
            schema_version: schema_version(),
            event_id: new_id(),
            event_type: n.event_type,
            project_id: n.project_id,
            correlation_id: n.correlation_id,
            causation_id: n.causation_id,
            idempotency_key: n.idempotency_key,
            actor_type: n.actor_type,
            actor_id: n.actor_id,
            occurred_at: utcnow(),
            payload,
        })
    }

    /// `ActivityEvent.typed_payload()`.
    pub fn typed_payload(&self) -> Result<EventPayload, ValidationError> {
        parse_payload(self.event_type, &self.payload)
    }
}

impl Validate for ActivityEvent {
    fn check(&self, c: &mut Checker) {
        let mark = c.error_count();
        c.min_len("idempotency_key", &self.idempotency_key, 1);
        c.max_len("idempotency_key", &self.idempotency_key, 255);
        c.model_rules(mark, |c| {
            if let Err(e) = normalize_payload(self.event_type, &self.payload) {
                c.value_error(e);
            }
        });
    }
}

impl Contract for ActivityEvent {
    const NAME: &'static str = "ActivityEvent";
}

/// `EventPage` (`apps/api/app/api/routes/events.py`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventPage {
    pub events: Vec<ActivityEvent>,
    pub next_cursor: i64,
}

plain_contract!(EventPage);
