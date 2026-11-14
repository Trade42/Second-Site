//! Port of `api/routes/tasks.py`.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::app::AppState;
use crate::contracts::TaskRecord;
use crate::error::AppResult;
use crate::http::extract::{AuthUser, PathParams};
use crate::store::repo::TASKS;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/v1/projects/{project_id}/tasks/{task_id}",
        get(get_task),
    )
}

async fn get_task(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<TaskRecord>> {
    let project_id = params.uuid("project_id")?;
    let task = crate::in_tx!(app, |tx| {
        let project = app.authz().project_for_user(tx, project_id, user).await?;
        // FastAPI validates the endpoint's own `task_id` only after the `owned_project`
        // dependency ran: another owner's project is a 404 even when `task_id` is malformed.
        let task_id = params.uuid("task_id")?;
        TASKS
            .require_in_project(tx, task_id, project.project_id, "task not found")
            .await?
    });
    Ok(Json(task))
}
