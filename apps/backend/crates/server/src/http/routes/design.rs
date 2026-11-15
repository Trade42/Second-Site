//! Port of `api/routes/design.py`: layout generation, the stored floor geometry, the layout list,
//! revisions and the bill of materials.
//!
//! Every record loaded by id belongs to the URL project or the answer is 404: a layout
//! (`layout not found`), a geometry through its property (`geometry not found`), and, fixing
//! Python, the property of a generation request (`property not found`; Python only checked that
//! the property exists, in any project).
//!
//! Request checks run in FastAPI's order: a body that is not valid JSON is rejected first, then
//! the bearer token, the path, the owned project, the endpoint's own path parameters, the body
//! model, and finally the records the handler loads.
//!
//! Generation and revision answer 202 with the task, run inline as in Python. Anything the work
//! raises is recorded in the task (`FAILED`, `error_code` the Python exception class): a
//! property without measured geometry (the service's 404 `GEOMETRY_UNAVAILABLE`) and a stale or
//! unstructured revision (the service's 409 `STALE_DESIGN_VERSION` / `REVISION_NOT_STRUCTURED`)
//! are therefore 202 with a FAILED task (`NotFoundError` / `ConflictError`), exactly as Python
//! answers them.
//!
//! The request checks run in their own short transaction; the work then runs in the design
//! service's three phases (`DesignService::run_layout_task`), so the provider fetch and the
//! solve hold no transaction, pool connection or lock.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use ss_core::Contract;
use ss_domain::contracts::{
    BillOfMaterials, DesignRevisionRequest, DesignVersion, LayoutGenerationRequest, RevisionStatus,
    SiteGeometry,
};
use uuid::Uuid;

use crate::app::{App, AppState};
use crate::contracts::{JsonMap, RevisionBody, Subsystem, TaskAccepted, TaskRecord, TaskType};
use crate::error::{AppError, AppResult};
use crate::http::extract::{
    bearer_user, body_errors_with_input, early_json_body, AuthUser, PathParams,
};
use crate::http::routes::properties::property_in_project;
use crate::services::design::{DesignService, LayoutJob};
use crate::store::repo::{DESIGNS, GEOMETRIES, PROPERTIES};
use crate::store::{Query, Tx};

const PREFIX: &str = "/api/v1/projects/{project_id}/layouts";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(&format!("{PREFIX}/generate"), post(generate))
        .route(
            &format!("{PREFIX}/geometry/{{geometry_id}}"),
            get(get_geometry),
        )
        .route(PREFIX, get(list_layouts))
        .route(&format!("{PREFIX}/{{layout_id}}/revisions"), post(revise))
        .route(&format!("{PREFIX}/{{layout_id}}/bom"), get(bom))
}

/// `_design`: the layout if it belongs to the project, else 404 `layout not found`.
pub async fn design_in_project(
    tx: &mut dyn Tx,
    project_id: Uuid,
    layout_id: Uuid,
) -> AppResult<DesignVersion> {
    DESIGNS
        .require_in_project(tx, layout_id, project_id, "layout not found")
        .await
}

fn accepted(task: TaskRecord) -> TaskAccepted {
    TaskAccepted {
        status_url: format!(
            "/api/v1/projects/{}/tasks/{}",
            task.project_id, task.task_id
        ),
        task,
    }
}

async fn owned(app: &App, tx: &mut dyn Tx, project_id: Uuid, user: Uuid) -> AppResult<Uuid> {
    Ok(app
        .authz()
        .project_for_user(tx, project_id, user)
        .await?
        .project_id)
}

fn object_of<T: serde::Serialize>(v: &T) -> AppResult<JsonMap> {
    match serde_json::to_value(v) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(AppError::internal("request did not serialize to an object")),
        Err(e) => Err(AppError::internal(format!("serialize request: {e}"))),
    }
}

async fn generate(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<(StatusCode, Json<TaskAccepted>)> {
    let decoded = early_json_body(&headers, &body)?;
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let (project_id, req) = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        let obj = decoded.into_object()?;
        let req = LayoutGenerationRequest::from_json_value(Value::Object(obj.clone()))
            .map_err(|e| body_errors_with_input(e, &obj))?;
        // Python: `repos.properties.get(db, body.property_id) is None` (any project).
        property_in_project(tx, project_id, req.property_id).await?;
        (project_id, req)
    });
    let task = TaskRecord::new(
        project_id,
        TaskType::GenerateLayout,
        Subsystem::PropertySpatial,
        object_of(&req)?,
    );
    let task = DesignService::new(&app)
        .run_layout_task(task, &LayoutJob::Generate(req))
        .await?;
    Ok((StatusCode::ACCEPTED, Json(accepted(task))))
}

/// The measured (or fixture) floor a layout was generated on: outline, partitions, openings.
async fn get_geometry(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<SiteGeometry>> {
    let project_id = params.uuid("project_id")?;
    let geo = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        let geometry_id = params.uuid("geometry_id")?;
        let not_found = || AppError::not_found("geometry not found");
        let geo = GEOMETRIES
            .get(tx, geometry_id)
            .await?
            .ok_or_else(not_found)?;
        PROPERTIES
            .get_in_project(tx, geo.property_id, project_id)
            .await?
            .ok_or_else(not_found)?;
        geo
    });
    Ok(Json(geo))
}

async fn list_layouts(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<Vec<DesignVersion>>> {
    let project_id = params.uuid("project_id")?;
    let list = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        DESIGNS
            .list(tx, Query::new().eq("project_id", project_id))
            .await?
    });
    Ok(Json(list))
}

async fn revise(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<(StatusCode, Json<TaskAccepted>)> {
    let decoded = early_json_body(&headers, &body)?;
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let rev = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        let layout_id = params.uuid("layout_id")?;
        let obj = decoded.into_object()?;
        let body = RevisionBody::from_json_value(Value::Object(obj.clone()))
            .map_err(|e| body_errors_with_input(e, &obj))?;
        let base = design_in_project(tx, project_id, layout_id).await?;
        let rev = DesignRevisionRequest {
            revision_request_id: ss_core::new_id(),
            project_id,
            property_id: base.property_id,
            base_design_version_id: base.design_version_id,
            requested_changes: body.requested_changes,
            room_requirement_overrides: body.room_requirement_overrides,
            preserved_constraints: body.preserved_constraints,
            requested_by: user,
            source_channel: body.source_channel,
            status: RevisionStatus::default(),
        };
        rev.validate()?;
        rev
    });
    let mut input = JsonMap::new();
    input.insert(
        "revision_request_id".into(),
        json!(rev.revision_request_id.to_string()),
    );
    let task = TaskRecord::new(
        rev.project_id,
        TaskType::ReviseLayout,
        Subsystem::PropertySpatial,
        input,
    );
    let task = DesignService::new(&app)
        .run_layout_task(task, &LayoutJob::Revise(rev))
        .await?;
    Ok((StatusCode::ACCEPTED, Json(accepted(task))))
}

async fn bom(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<BillOfMaterials>> {
    let project_id = params.uuid("project_id")?;
    let b = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        let layout_id = params.uuid("layout_id")?;
        let d = design_in_project(tx, project_id, layout_id).await?;
        DesignService::new(&app)
            .bill_of_materials(tx, project_id, &d)
            .await?
    });
    Ok(Json(b))
}
