//! Port of `api/routes/communications.py`, everything under `router` (the project routes):
//! outreach, communications, calls, the call simulator and owner handoffs. The `owner_router`
//! routes (`/api/v1/owner/channel*`) are Photon's and stay in the Python service.
//!
//! Paths, status codes and bodies are Python's. Request checks run in FastAPI's order: a body
//! that is not valid JSON first, then the bearer token, the path, the owned project, the
//! endpoint's own path ids, the body model, and the records.
//!
//! Fixed Python defects (shared plan): outreach loads its property, starting a call loads its
//! communication, and a handoff response loads its handoff through the URL project, with 404 for
//! a record of another project (same message as a missing one).
//!
//! Owner texts the call flow asks for are sent only after the request's transaction committed
//! ([`crate::services::notifier`]); the answer never waits for them and never fails because of
//! them.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use ss_core::Contract;
use ss_domain::communications::call_payload;
use ss_domain::contracts::{
    CallSession, Channel, CommunicationSession, CommunicationState, HandoffStatus, OutreachRequest,
    OwnerHandoffRequest,
};
use uuid::Uuid;

use crate::app::{App, AppState};
use crate::contracts::{
    plain_contract, ActionType, ActorType, ApprovalRequest, ApprovalStatus, EventType,
};
use crate::error::{AppError, AppResult};
use crate::http::extract::{bearer_user, body_errors_with_input, early_json_body, PathParams};
use crate::http::routes::properties::property_in_project;
use crate::services::approvals::ApprovalSpec;
use crate::services::call_orchestration::{lock_project, CallOrchestrator, StartCall};
use crate::services::call_simulator::CallSimulator;
use crate::services::events::Emit;
use crate::services::notifier::NotifyRequest;
use crate::store::comms::{channel_for_owner, CALLS, COMMUNICATIONS, HANDOFFS};
use crate::store::repo::events as event_rows;
use crate::store::{Query, Tx};

const PREFIX: &str = "/api/v1/projects/{project_id}";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(&format!("{PREFIX}/outreach"), post(outreach))
        .route(&format!("{PREFIX}/communications"), get(list_comms))
        .route(&format!("{PREFIX}/calls"), post(start_call).get(list_calls))
        .route(
            &format!("{PREFIX}/calls/{{call_id}}/simulate"),
            post(simulate_call),
        )
        .route(&format!("{PREFIX}/handoffs"), get(list_handoffs))
        .route(
            &format!("{PREFIX}/handoffs/{{handoff_id}}/response"),
            post(handoff_response),
        )
}

/// `OutreachAccepted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutreachAccepted {
    pub communication: CommunicationSession,
    pub approval: ApprovalRequest,
}

/// `StartCallBody`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartCallBody {
    pub communication_id: Uuid,
    #[serde(default)]
    pub simulate: Option<bool>,
}

fn accept() -> String {
    "accept".to_owned()
}

/// `SimulateBody`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimulateBody {
    #[serde(default = "accept")]
    pub owner_action: String,
    #[serde(default)]
    pub answers: IndexMap<String, String>,
}

/// `HandoffResponseBody`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffResponseBody {
    pub response: HandoffStatus,
}

plain_contract!(
    OutreachAccepted,
    StartCallBody,
    SimulateBody,
    HandoffResponseBody
);

/// Python's `str(dict)` of a `dict[str, str]`: `{'k': 'v', ...}` in insertion order.
pub fn py_dict_repr(m: &IndexMap<String, String>) -> String {
    let parts: Vec<String> = m
        .iter()
        .map(|(k, v)| {
            format!(
                "{}: {}",
                ss_domain::py::str_repr(k),
                ss_domain::py::str_repr(v)
            )
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

fn parse_body<T: Contract>(decoded: crate::http::extract::EarlyBody) -> AppResult<T> {
    let obj = decoded.into_object()?;
    T::from_json_value(Value::Object(obj.clone()))
        .map_err(|e| with_enum_ctx(body_errors_with_input(e, &obj)))
}

/// Pydantic's `ctx` of an `enum` error, `{"expected": "'A', 'B' or 'C'"}`, read back from the
/// message it formats from it (`Input should be 'A', 'B' or 'C'`).
fn with_enum_ctx(e: AppError) -> AppError {
    let AppError::RequestValidation(mut errors) = e else {
        return e;
    };
    for err in &mut errors {
        let Some(obj) = err.as_object_mut() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("enum") || obj.contains_key("ctx") {
            continue;
        }
        let expected = obj
            .get("msg")
            .and_then(Value::as_str)
            .and_then(|m| m.strip_prefix("Input should be "))
            .map(str::to_owned);
        if let Some(expected) = expected {
            obj.insert("ctx".into(), json!({ "expected": expected }));
        }
    }
    AppError::RequestValidation(errors)
}

async fn owned(
    app: &App,
    tx: &mut dyn Tx,
    project_id: Uuid,
    user: Uuid,
) -> AppResult<(Uuid, Uuid)> {
    let p = app.authz().project_for_user(tx, project_id, user).await?;
    Ok((p.project_id, p.owner_id))
}

fn get_default<'m>(m: &'m IndexMap<String, String>, k: &str, default: &'m str) -> &'m str {
    m.get(k).map_or(default, String::as_str)
}

/// The second key of an approval request text (see [`needs_linked_renotify`]).
pub fn linked_renotify_key(approval_id: Uuid) -> String {
    format!("approval-notify:{approval_id}:linked")
}

/// The `delivery` Python recorded for a notify key in this project (its
/// `communication.owner_notified` event `notify:<key>`), if any.
async fn recorded_delivery(
    tx: &mut dyn Tx,
    project_id: Uuid,
    key: &str,
) -> AppResult<Option<String>> {
    let rows = event_rows::select(
        tx,
        Query::new()
            .eq("idempotency_key", format!("notify:{key}"))
            .limit(1),
    )
    .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let ev = event_rows::parse(row)?;
    if ev.project_id != project_id || ev.event_type != EventType::CommunicationOwnerNotified {
        return Ok(None);
    }
    Ok(ev
        .payload
        .get("delivery")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// A repeated outreach reuses its PENDING approval and therefore its notify key
/// `approval-notify:<approval id>`. Python's answer for a key is final: when the owner had not
/// linked their channel the first time (`not_linked`), the repeat gets `not_linked` again and the
/// owner is never told. So, when the first answer was `not_linked`, the owner's channel now reads
/// LINKED (with the space and agent ids a send needs, as `notify_owner` checks), and the derived key
/// `approval-notify:<approval id>:linked` has no recorded answer yet, the approval request is
/// sent once more under that key. Never after `accepted`, `recorded` or `failed`; at most once
/// per approval (Python dedupes the derived key, and once it has an answer it is not sent again).
async fn needs_linked_renotify(
    tx: &mut dyn Tx,
    project_id: Uuid,
    owner_id: Uuid,
    approval_id: Uuid,
) -> AppResult<bool> {
    let first =
        recorded_delivery(tx, project_id, &format!("approval-notify:{approval_id}")).await?;
    if first.as_deref() != Some("not_linked") {
        return Ok(false);
    }
    let linked = channel_for_owner(tx, owner_id).await?.is_some_and(|c| {
        c.status == ss_domain::contracts::OwnerChannelStatus::Linked
            && c.space_id.as_deref().is_some_and(|s| !s.is_empty())
            && c.agent_user_id.as_deref().is_some_and(|s| !s.is_empty())
    });
    if !linked {
        return Ok(false);
    }
    // REVIEW FIX (rust-server-4-review F3): an earlier attempt whose outcome is unknown (timeout,
    // 5xx: Python may have texted) is not sent again, so the derived key texts at most once.
    let unreached = format!(
        "{}{}",
        crate::services::notifier::UNREACHED_KEY_PREFIX,
        linked_renotify_key(approval_id)
    );
    for row in
        event_rows::select(tx, Query::new().eq("idempotency_key", unreached).limit(1)).await?
    {
        let ev = event_rows::parse(&row)?;
        if ev.payload.get("delivery").and_then(Value::as_str) == Some("unknown") {
            return Ok(false);
        }
    }
    Ok(event_rows::select(
        tx,
        Query::new()
            .eq(
                "idempotency_key",
                format!("notify:{}", linked_renotify_key(approval_id)),
            )
            .limit(1),
    )
    .await?
    .is_empty())
}

/// `POST /projects/{id}/outreach` (202): a communication draft and a PENDING approval. Nothing
/// is sent until the owner approves and `POST .../calls` executes it.
async fn outreach(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<(StatusCode, Json<OutreachAccepted>)> {
    let decoded = early_json_body(&headers, &body)?;
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let (out, notices) = crate::in_tx!(app, |tx| {
        let (project_id, owner_id) = owned(&app, tx, project_id, user).await?;
        let mut req: OutreachRequest = parse_body(decoded)?;
        // The JSON value above holds object keys sorted; Python's dict keeps the body's order,
        // which the approval key (`str(recipient)`) shows. Read the recipient again from the raw
        // body, in document order (a repeated key keeps its first position and its last value,
        // as in Python).
        if let Ok(ordered) = serde_json::from_slice::<OutreachRequest>(&body) {
            if ordered.recipient == req.recipient {
                req.recipient = ordered.recipient;
            }
        }
        lock_project(tx, project_id).await?;
        // Python accepted any property id, even one of another project.
        property_in_project(tx, project_id, req.property_id).await?;
        let payload = call_payload(project_id, &req.recipient, Some(req.property_id));
        let action = if req.channel == Channel::Voice {
            ActionType::PlaceCall
        } else {
            ActionType::SendMessage
        };
        let summary = format!(
            "{} to {}: {}",
            req.channel.as_str(),
            get_default(&req.recipient, "name", "contact"),
            req.purpose
        );
        let key = format!(
            "outreach:{project_id}:{}:{}",
            req.property_id,
            py_dict_repr(&req.recipient)
        );
        let appr = app
            .approvals()
            .request(
                tx,
                ApprovalSpec::new(
                    project_id, owner_id, "outreach", action, payload, summary, key,
                ),
            )
            .await?;
        let comm = CommunicationSession {
            communication_id: Uuid::new_v4(),
            project_id,
            property_id: Some(req.property_id),
            channel: req.channel,
            external_participants: vec![req.recipient.clone()],
            authorized_owner_id: owner_id,
            purpose: req.purpose.clone(),
            state: CommunicationState::AwaitingApproval,
            recording_reference: req.intro_recording_reference.clone(),
            transcript_reference: None,
            structured_findings: Vec::new(),
            approval_id: Some(appr.approval_id),
            provider_message_ids: Vec::new(),
            created_at: ss_core::UtcDateTime::now(),
        };
        COMMUNICATIONS.add(tx, &comm).await?;
        let notice = NotifyRequest {
            owner_id,
            project_id,
            text: format!(
                "Your assistant wants to call {} about a property ({}). Review and approve in the \
                 app: {}/activity",
                get_default(&req.recipient, "name", "a broker"),
                req.purpose,
                app.settings.public_web_url
            ),
            purpose: "approval_request".into(),
            idempotency_key: format!("approval-notify:{}", appr.approval_id),
        };
        let mut notices = vec![notice.clone()];
        if appr.approval_status == ApprovalStatus::Pending
            && needs_linked_renotify(tx, project_id, owner_id, appr.approval_id).await?
        {
            notices.push(NotifyRequest {
                idempotency_key: linked_renotify_key(appr.approval_id),
                ..notice
            });
        }
        app.events()
            .emit(
                tx,
                Emit::new(
                    EventType::OutreachApprovalRequested,
                    project_id,
                    ActorType::Agent,
                    "outreach",
                    format!("outreach:{}:requested", comm.communication_id),
                    json!({
                        "communication_id": comm.communication_id.to_string(),
                        "property_id": req.property_id.to_string(),
                        "approval_id": appr.approval_id.to_string(),
                    }),
                ),
            )
            .await?;
        (
            OutreachAccepted {
                communication: comm,
                approval: appr,
            },
            notices,
        )
    });
    app.owner_notify().dispatch(app.clone(), notices);
    Ok((StatusCode::ACCEPTED, Json(out)))
}

async fn list_comms(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
) -> AppResult<Json<Vec<CommunicationSession>>> {
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let rows = crate::in_tx!(app, |tx| {
        let (project_id, _) = owned(&app, tx, project_id, user).await?;
        COMMUNICATIONS
            .list(tx, Query::new().eq("project_id", project_id))
            .await?
    });
    Ok(Json(rows))
}

/// `POST /projects/{id}/calls` (202): executes an approved outreach. With the fixture voice
/// provider no carrier exists, so the call is always a labelled simulation.
async fn start_call(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<(StatusCode, Json<CallSession>)> {
    let decoded = early_json_body(&headers, &body)?;
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let (call, notices) = crate::in_tx!(app, |tx| {
        let (project_id, owner_id) = owned(&app, tx, project_id, user).await?;
        let req: StartCallBody = parse_body(decoded)?;
        lock_project(tx, project_id).await?;
        // Python loaded the communication by id from any project.
        let comm = COMMUNICATIONS
            .require_in_project(
                tx,
                req.communication_id,
                project_id,
                &format!("CommunicationSession {} not found", req.communication_id),
            )
            .await?;
        let recipient = comm
            .external_participants
            .first()
            .cloned()
            .unwrap_or_default();
        // With the fixture provider every call is simulated. With the live provider a call is
        // always real, and Python's `simulate: true` there placed a real call labelled
        // simulated (its answers stored as fixture evidence, "not a real conversation"); that is
        // refused here with the simulator's own live-provider answer, before the approval is
        // spent and before any carrier request, and nothing is stored.
        let live = app.voice().mode() == "live";
        if live && req.simulate == Some(true) {
            return Err(AppError::conflict(
                "a simulated call needs the fixture voice provider; with the live provider \
                 this would place a real call. Start it without \"simulate\" to place the real \
                 call.",
            )
            .code("SIMULATOR_DISABLED"));
        }
        let simulated = !live;
        let mut orch = CallOrchestrator::new(&app);
        let call = orch
            .start(
                tx,
                StartCall {
                    project_id,
                    owner_id,
                    recipient,
                    property_id: comm.property_id,
                    approval_id: comm.approval_id,
                    intro_recording_reference: comm.recording_reference.clone(),
                    communication_id: Some(comm.communication_id),
                    simulated,
                },
            )
            .await?;
        (call, orch.take_notices())
    });
    app.owner_notify().dispatch(app.clone(), notices);
    Ok((StatusCode::ACCEPTED, Json(call)))
}

/// `POST /projects/{id}/calls/{call_id}/simulate`: fixture mode only; plays the broker side of a
/// simulated call through the real event handlers.
async fn simulate_call(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Json<CallSession>> {
    let decoded = early_json_body(&headers, &body)?;
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let (call, notices) = crate::in_tx!(app, |tx| {
        let (project_id, _) = owned(&app, tx, project_id, user).await?;
        let call_id = params.uuid("call_id")?;
        let req: SimulateBody = parse_body(decoded)?;
        let call = CALLS.require(tx, call_id).await?;
        if call.project_id != project_id {
            return Err(AppError::not_found("call not found"));
        }
        lock_project(tx, project_id).await?;
        let mut sim = CallSimulator::new(&app)?;
        let call = sim
            .run(tx, call_id, &req.owner_action, &req.answers, 1)
            .await?;
        (call, sim.orch.take_notices())
    });
    app.owner_notify().dispatch(app.clone(), notices);
    Ok(Json(call))
}

async fn list_handoffs(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
) -> AppResult<Json<Vec<OwnerHandoffRequest>>> {
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let rows = crate::in_tx!(app, |tx| {
        let (project_id, _) = owned(&app, tx, project_id, user).await?;
        HANDOFFS
            .list(tx, Query::new().eq("project_id", project_id))
            .await?
    });
    Ok(Json(rows))
}

async fn list_calls(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
) -> AppResult<Json<Vec<CallSession>>> {
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let rows = crate::in_tx!(app, |tx| {
        let (project_id, _) = owned(&app, tx, project_id, user).await?;
        CALLS
            .list(tx, Query::new().eq("project_id", project_id))
            .await?
    });
    Ok(Json(rows))
}

/// `POST /projects/{id}/handoffs/{handoff_id}/response`: the owner's answer from the web app.
async fn handoff_response(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Json<CallSession>> {
    let decoded = early_json_body(&headers, &body)?;
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let (call, notices) = crate::in_tx!(app, |tx| {
        let (project_id, _) = owned(&app, tx, project_id, user).await?;
        let handoff_id = params.uuid("handoff_id")?;
        let req: HandoffResponseBody = parse_body(decoded)?;
        lock_project(tx, project_id).await?;
        // Python loaded the handoff by id from any project.
        let h = HANDOFFS
            .require_in_project(
                tx,
                handoff_id,
                project_id,
                &format!("OwnerHandoffRequest {handoff_id} not found"),
            )
            .await?;
        let mut orch = CallOrchestrator::new(&app);
        let call = orch.respond(tx, h, user, req.response).await?;
        (call, orch.take_notices())
    });
    app.owner_notify().dispatch(app.clone(), notices);
    Ok(Json(call))
}
