//! Port of `api/routes/approvals.py`.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::app::AppState;
use crate::contracts::{ApprovalDecision, ApprovalRequest};
use crate::error::AppResult;
use crate::http::extract::{json_body, AuthUser, PathParams};
use crate::store::repo::APPROVALS;
use crate::store::Query;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/approvals/{approval_id}/decision", post(decide))
        .route(
            "/api/v1/projects/{project_id}/approvals",
            get(list_approvals),
        )
}

async fn decide(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Json<ApprovalRequest>> {
    let approval_id = params.uuid("approval_id")?;
    let decision: ApprovalDecision = json_body(&headers, &body)?;
    let appr = crate::in_tx!(app, |tx| {
        app.approvals()
            .decide(
                tx,
                approval_id,
                user,
                decision.decision,
                &decision.action_payload_hash,
            )
            .await?
    });
    Ok(Json(appr))
}

async fn list_approvals(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<Vec<ApprovalRequest>>> {
    let project_id = params.uuid("project_id")?;
    let list = crate::in_tx!(app, |tx| {
        let project = app.authz().project_for_user(tx, project_id, user).await?;
        APPROVALS
            .list(tx, Query::new().eq("project_id", project.project_id))
            .await?
    });
    Ok(Json(list))
}
