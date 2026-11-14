//! Port of `api/routes/properties.py`.
//!
//! Every property loaded by id goes through the "belongs to this project or 404" check
//! (`property not found`). Request checks run in FastAPI's order: a body that is not valid JSON
//! is rejected first (FastAPI decodes the body before it resolves dependencies), then the bearer
//! token, the path, the owned project, the endpoint's own path parameters and finally the body
//! model.
//!
//! `POST .../properties/search` answers 202 but runs the slice inline, so the results are
//! readable as soon as it returns; a failure inside the slice is a FAILED task (still 202).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use ss_core::Contract;
use ss_domain::contracts::{
    CostScenario, InfrastructureAssessment, PermitRecord, PropertyDigitalProfile,
    PropertyRankingResult, PropertySearchRequest,
};
use uuid::Uuid;

use crate::app::{App, AppState};
use crate::contracts::{JsonMap, Subsystem, TaskAccepted, TaskRecord, TaskType};
use crate::error::AppResult;
use crate::http::extract::{bearer_user, body_errors, early_json_body, AuthUser, PathParams};
use crate::store::repo::{ASSESSMENTS, COST_SCENARIOS, PERMITS, PROPERTIES, RANKINGS};
use crate::store::{Query, Tx};
use crate::workflows::property_analysis::PropertyAnalysisWorkflow;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/projects/{project_id}/properties/search",
            post(search),
        )
        .route("/api/v1/projects/{project_id}/properties", get(list_properties))
        .route(
            "/api/v1/projects/{project_id}/properties/{property_id}",
            get(get_property),
        )
        .route(
            "/api/v1/projects/{project_id}/properties/{property_id}/analyze",
            post(analyze),
        )
        .route(
            "/api/v1/projects/{project_id}/properties/{property_id}/permits",
            get(permits),
        )
        .route(
            "/api/v1/projects/{project_id}/properties/{property_id}/feasibility",
            get(feasibility),
        )
        .route("/api/v1/projects/{project_id}/rankings", get(rankings))
        .route(
            "/api/v1/projects/{project_id}/cost-scenarios",
            get(cost_scenarios),
        )
}

/// `_prop`: the property if it belongs to the project, else 404 `property not found`.
pub async fn property_in_project(
    tx: &mut dyn Tx,
    project_id: Uuid,
    property_id: Uuid,
) -> AppResult<PropertyDigitalProfile> {
    PROPERTIES
        .require_in_project(tx, property_id, project_id, "property not found")
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

async fn search(
    State(app): State<AppState>,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<(StatusCode, Json<TaskAccepted>)> {
    let decoded = early_json_body(&headers, &body)?;
    let user = bearer_user(&headers, app.settings.auth_secret.expose())?;
    let project_id = params.uuid("project_id")?;
    let task = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        let obj = decoded.into_object()?;
        let req = PropertySearchRequest::from_json_value(Value::Object(obj)).map_err(body_errors)?;
        let input: JsonMap = match serde_json::to_value(&req) {
            Ok(Value::Object(m)) => m,
            _ => JsonMap::new(),
        };
        let task = app
            .tasks()
            .create(
                tx,
                project_id,
                TaskType::AnalyzeProperty,
                Subsystem::AgentsFinance,
                input,
            )
            .await?;
        let running = app.tasks().start(tx, task).await?;
        let correlation_id = running.task.correlation_id;
        let outcome = PropertyAnalysisWorkflow::new(&app)
            .run(tx, project_id, req, correlation_id)
            .await;
        app.tasks().finish(tx, running, outcome).await?
    });
    Ok((StatusCode::ACCEPTED, Json(accepted(task))))
}

async fn list_properties(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<Vec<PropertyDigitalProfile>>> {
    let project_id = params.uuid("project_id")?;
    let props = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        PROPERTIES
            .list(tx, Query::new().eq("project_id", project_id))
            .await?
    });
    Ok(Json(props))
}

async fn get_property(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<PropertyDigitalProfile>> {
    let project_id = params.uuid("project_id")?;
    let prop = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        let property_id = params.uuid("property_id")?;
        property_in_project(tx, project_id, property_id).await?
    });
    Ok(Json(prop))
}

async fn analyze(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<(StatusCode, Json<TaskAccepted>)> {
    let project_id = params.uuid("project_id")?;
    let task = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        let property_id = params.uuid("property_id")?;
        let prop = property_in_project(tx, project_id, property_id).await?;
        let mut input = JsonMap::new();
        input.insert("property_id".into(), json!(property_id.to_string()));
        let task = app
            .tasks()
            .create(
                tx,
                project_id,
                TaskType::EvaluateInfrastructure,
                Subsystem::AgentsFinance,
                input,
            )
            .await?;
        let running = app.tasks().start(tx, task).await?;
        let correlation_id = running.task.correlation_id;
        let wf = PropertyAnalysisWorkflow::new(&app);
        let outcome: AppResult<JsonMap> = async {
            let p = wf.ingest_permits(tx, prop, correlation_id).await?;
            let (assessments, scenario) = wf.evaluate(tx, p, correlation_id, None).await?;
            let ranking = wf.rank(tx, project_id, correlation_id, None).await?;
            let mut out = JsonMap::new();
            out.insert(
                "assessment_ids".into(),
                json!(assessments
                    .iter()
                    .map(|a| a.assessment_id.to_string())
                    .collect::<Vec<_>>()),
            );
            out.insert("scenario_id".into(), json!(scenario.scenario_id.to_string()));
            out.insert("ranking_id".into(), json!(ranking.ranking_id.to_string()));
            Ok(out)
        }
        .await;
        app.tasks().finish(tx, running, outcome).await?
    });
    Ok((StatusCode::ACCEPTED, Json(accepted(task))))
}

async fn permits(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<Vec<PermitRecord>>> {
    let project_id = params.uuid("project_id")?;
    let list = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        let property_id = params.uuid("property_id")?;
        property_in_project(tx, project_id, property_id).await?;
        PERMITS
            .list(tx, Query::new().eq("property_id", property_id))
            .await?
    });
    Ok(Json(list))
}

async fn feasibility(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<Vec<InfrastructureAssessment>>> {
    let project_id = params.uuid("project_id")?;
    let list = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        let property_id = params.uuid("property_id")?;
        property_in_project(tx, project_id, property_id).await?;
        ASSESSMENTS
            .list(
                tx,
                Query::new()
                    .eq("property_id", property_id)
                    .eq("superseded", false),
            )
            .await?
    });
    Ok(Json(list))
}

async fn rankings(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<Vec<PropertyRankingResult>>> {
    let project_id = params.uuid("project_id")?;
    let list = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        RANKINGS
            .list(
                tx,
                Query::new()
                    .eq("project_id", project_id)
                    .order_asc("ranking_version"),
            )
            .await?
    });
    Ok(Json(list))
}

async fn cost_scenarios(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<Vec<CostScenario>>> {
    let project_id = params.uuid("project_id")?;
    let list = crate::in_tx!(app, |tx| {
        let project_id = owned(&app, tx, project_id, user).await?;
        COST_SCENARIOS
            .list(
                tx,
                Query::new()
                    .eq("project_id", project_id)
                    .eq("superseded", false),
            )
            .await?
    });
    Ok(Json(list))
}
