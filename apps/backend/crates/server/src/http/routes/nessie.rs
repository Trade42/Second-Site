//! Capital One Nessie sandbox routes (additive; no Python counterpart). Everything is mock money
//! and every answer says so: success bodies carry the `ss-nessie` label (`simulation`,
//! `disclaimer`) at the top level, and the bank-outcome errors carry it in `details`.
//!
//! | Route (under `/api/v1/projects/{project_id}`) | Answer |
//! |---|---|
//! | `POST /nessie/sandbox` `{opening_balance_minor?}` | 201 created, 200 already set up, 502 set-up stopped part way |
//! | `GET /nessie/sandbox` | 200, or 404 when none was set up or it is gone |
//! | `GET /properties/{property_id}/nessie/plan` | 200 `MoveInPaymentPlan` and the inputs it was built from |
//! | `GET /properties/{property_id}/nessie/affordability` | 200 `AffordabilityReport` for the sandbox account |
//! | `POST /properties/{property_id}/nessie/payments/{plan_item_id}/approval` | 201 new, 200 existing open approval |
//! | `POST /properties/{property_id}/nessie/payments/{plan_item_id}/execute` `{approval_id}` | 200 sent, 422 refused by the bank, 502 may exist |
//! | `POST /properties/{property_id}/nessie/payments/{plan_item_id}/refresh` | 200 the stored transaction read again |
//!
//! Request checks run in the existing order: a body that is not valid JSON first (routes with a
//! body), then the bearer token, the project path id, the owned project, the endpoint's own path
//! parameters, the body model, and finally the records (another project's property is 404).
//! The approval and refresh routes take no body (any body is ignored, as FastAPI ignores a body
//! an endpoint does not declare); refresh takes no transaction id from the client.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use ss_core::{Checker, Contract, ProjectContext, Validate};
use uuid::Uuid;

use crate::app::{App, AppState};
use crate::error::{AppError, AppResult};
use crate::http::extract::{
    bearer_user, body_errors_with_input, early_json_body, AuthUser, EarlyBody, PathParams,
};
use crate::http::routes::properties::property_in_project;
use crate::services::nessie::{self as svc, Executed, SetUp, MAX_OPENING_BALANCE_MINOR};
use crate::store::Tx;

const PROJECT: &str = "/api/v1/projects/{project_id}";

pub fn router() -> Router<AppState> {
    let payments = format!("{PROJECT}/properties/{{property_id}}/nessie/payments/{{plan_item_id}}");
    Router::new()
        .route(
            &format!("{PROJECT}/nessie/sandbox"),
            post(set_up_sandbox).get(read_sandbox),
        )
        .route(
            &format!("{PROJECT}/properties/{{property_id}}/nessie/plan"),
            get(plan),
        )
        .route(
            &format!("{PROJECT}/properties/{{property_id}}/nessie/affordability"),
            get(affordability),
        )
        .route(&format!("{payments}/approval"), post(approval))
        .route(&format!("{payments}/execute"), post(execute))
        .route(&format!("{payments}/refresh"), post(refresh))
}

/// `POST /nessie/sandbox` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSetupBody {
    /// Mock money to open the sandbox account with, US cents. Default: the business profile's
    /// `opening_capital`.
    #[serde(default)]
    pub opening_balance_minor: Option<i64>,
}

impl Validate for SandboxSetupBody {
    fn check(&self, c: &mut Checker) {
        if let Some(v) = self.opening_balance_minor {
            c.ge("opening_balance_minor", v, 0);
            c.le("opening_balance_minor", v, MAX_OPENING_BALANCE_MINOR);
        }
    }
}

impl Contract for SandboxSetupBody {
    const NAME: &'static str = "NessieSandboxSetup";
}

/// `POST .../execute` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteBody {
    pub approval_id: Uuid,
}

impl Validate for ExecuteBody {
    fn check(&self, _c: &mut Checker) {}
}

impl Contract for ExecuteBody {
    const NAME: &'static str = "NessieExecute";
}

async fn owned(
    app: &App,
    tx: &mut dyn Tx,
    project_id: Uuid,
    user: Uuid,
) -> AppResult<ProjectContext> {
    app.authz().project_for_user(tx, project_id, user).await
}

/// The `plan_item_id` path parameter: at most 64 printable ASCII characters without spaces.
fn plan_item(params: &PathParams) -> AppResult<String> {
    let raw = params.raw("plan_item_id").unwrap_or_default();
    if raw.is_empty()
        || raw.len() > ss_nessie::approval::PLAN_ITEM_ID_MAX_LEN
        || !raw.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(AppError::request_validation(
            "value_error",
            vec![json!("path"), json!("plan_item_id")],
            "Value error, plan_item_id must be 1 to 64 printable ASCII characters without spaces",
        ));
    }
    Ok(raw)
}

fn status_problem(status: StatusCode, e: AppError) -> Response {
    (status, Json(e.to_problem_detail())).into_response()
}

/// Owned project, `property_id` from the path, and the property loaded through the project.
async fn property_scope(
    app: &App,
    params: &PathParams,
    user: Uuid,
) -> AppResult<(ProjectContext, Uuid)> {
    let project_id = params.uuid("project_id")?;
    let out = crate::in_tx!(app, |tx| {
        let project = owned(app, tx, project_id, user).await?;
        let property_id = params.uuid("property_id")?;
        property_in_project(tx, project.project_id, property_id).await?;
        (project, property_id)
    });
    Ok(out)
}

async fn set_up_sandbox(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Response> {
    let decoded = early_json_body(&headers, &body)?;
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let requested = crate::in_tx!(app, |tx| {
        owned(&app, tx, project_id, user).await?;
        match decoded {
            EarlyBody::Empty => None,
            other => {
                let obj = other.into_object()?;
                SandboxSetupBody::from_json_value(Value::Object(obj.clone()))
                    .map_err(|e| body_errors_with_input(e, &obj))?
                    .opening_balance_minor
            }
        }
    });
    Ok(
        match svc::set_up_sandbox(&app, project_id, requested).await? {
            SetUp::Created(view) => (StatusCode::CREATED, Json(*view)).into_response(),
            SetUp::Existing(view) => (StatusCode::OK, Json(*view)).into_response(),
            SetUp::Incomplete(e) => status_problem(StatusCode::BAD_GATEWAY, e),
        },
    )
}

async fn read_sandbox(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<svc::SandboxView>> {
    let project_id = params.uuid("project_id")?;
    crate::in_tx!(app, |tx| {
        owned(&app, tx, project_id, user).await?;
    });
    Ok(Json(svc::read_sandbox(&app, project_id).await?))
}

async fn plan(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<svc::PlanView>> {
    let (project, property_id) = property_scope(&app, &params, user).await?;
    Ok(Json(
        svc::plan_view(&app, project.project_id, property_id).await?,
    ))
}

async fn affordability(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<svc::AffordabilityView>> {
    let (project, property_id) = property_scope(&app, &params, user).await?;
    Ok(Json(
        svc::affordability_view(&app, project.project_id, property_id).await?,
    ))
}

async fn approval(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Response> {
    let project_id = params.uuid("project_id")?;
    let (project, property_id, item) = crate::in_tx!(app, |tx| {
        let project = owned(&app, tx, project_id, user).await?;
        let property_id = params.uuid("property_id")?;
        let item = plan_item(&params)?;
        property_in_project(tx, project.project_id, property_id).await?;
        (project, property_id, item)
    });
    let view = svc::request_approval(
        &app,
        project.project_id,
        project.owner_id,
        property_id,
        &item,
    )
    .await?;
    let status = if view.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(view)).into_response())
}

async fn execute(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Response> {
    let decoded = early_json_body(&headers, &body)?;
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let (property_id, item, approval_id) = crate::in_tx!(app, |tx| {
        let project = owned(&app, tx, project_id, user).await?;
        let property_id = params.uuid("property_id")?;
        let item = plan_item(&params)?;
        let obj = decoded.into_object()?;
        let body = ExecuteBody::from_json_value(Value::Object(obj.clone()))
            .map_err(|e| body_errors_with_input(e, &obj))?;
        property_in_project(tx, project.project_id, property_id).await?;
        (property_id, item, body.approval_id)
    });
    Ok(
        match svc::execute_payment(&app, project_id, property_id, &item, approval_id).await? {
            Executed::Sent(view) => (StatusCode::OK, Json(*view)).into_response(),
            Executed::Refused(e) => status_problem(StatusCode::UNPROCESSABLE_ENTITY, e),
            Executed::MayExist(e) => status_problem(StatusCode::BAD_GATEWAY, e),
        },
    )
}

async fn refresh(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<svc::RefreshView>> {
    let project_id = params.uuid("project_id")?;
    let (property_id, item) = crate::in_tx!(app, |tx| {
        let project = owned(&app, tx, project_id, user).await?;
        let property_id = params.uuid("property_id")?;
        let item = plan_item(&params)?;
        property_in_project(tx, project.project_id, property_id).await?;
        (property_id, item)
    });
    Ok(Json(
        svc::refresh_payment(&app, project_id, property_id, &item).await?,
    ))
}
