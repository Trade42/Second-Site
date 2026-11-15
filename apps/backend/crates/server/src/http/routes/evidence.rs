//! Evidence routes for the Python supervisor's tools (new; Python has no HTTP route for these,
//! its tools `get_property_evidence` and `validate_evidence` read the database directly):
//!
//! - `GET /api/v1/projects/{project_id}/properties/{property_id}/evidence`: the property's
//!   `SourceEvidence` records, in `EvidenceService::for_property` order (`created_at`, the order
//!   feasibility reads them in);
//! - `GET .../evidence/validation`: `{"problems": [string]}`, what the tool computes:
//!   `validate_property(prop, evidence, prop.infrastructure_assessments)`.
//!
//! Same authentication, check order and project-scoped lookups as the other property routes:
//! bearer token (401), the owned project (404), the property id (422), and the property must
//! belong to the project (404 `property not found`; Python's tool loaded any property by id).

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use ss_domain::contracts::SourceEvidence;
use ss_domain::evidence::validate_property;

use crate::app::AppState;
use crate::error::AppResult;
use crate::http::extract::{AuthUser, PathParams};
use crate::http::routes::properties::property_in_project;
use crate::services::evidence::EvidenceService;

const PROPERTY: &str = "/api/v1/projects/{project_id}/properties/{property_id}";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(&format!("{PROPERTY}/evidence"), get(evidence))
        .route(&format!("{PROPERTY}/evidence/validation"), get(validation))
}

/// The answer of the validation route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceValidation {
    pub problems: Vec<String>,
}

async fn evidence(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<Vec<SourceEvidence>>> {
    let project_id = params.uuid("project_id")?;
    let list = crate::in_tx!(app, |tx| {
        let project_id = app
            .authz()
            .project_for_user(tx, project_id, user)
            .await?
            .project_id;
        let property_id = params.uuid("property_id")?;
        let prop = property_in_project(tx, project_id, property_id).await?;
        EvidenceService.for_property(tx, prop.property_id).await?
    });
    Ok(Json(list))
}

async fn validation(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<EvidenceValidation>> {
    let project_id = params.uuid("project_id")?;
    let problems = crate::in_tx!(app, |tx| {
        let project_id = app
            .authz()
            .project_for_user(tx, project_id, user)
            .await?
            .project_id;
        let property_id = params.uuid("property_id")?;
        let prop = property_in_project(tx, project_id, property_id).await?;
        let evidence = EvidenceService.for_property(tx, prop.property_id).await?;
        validate_property(&prop, &evidence, &prop.infrastructure_assessments)
    });
    Ok(Json(EvidenceValidation { problems }))
}
