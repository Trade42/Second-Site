//! Port of `api/routes/projects.py` (without `/chat`, which stays with the Python supervisor).
//!
//! Fixes over Python on `PUT /business-profile`: a `profile_id` that belongs to another project is
//! refused with 404 (Python overwrote it), and the confirmed-event idempotency key uses a stable
//! SHA-256 digest of the profile (Python used the per-process randomized `hash()`). On create, a
//! client-chosen `profile_id` that already exists is refused with 404 too (Python: 500).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use ss_core::{BusinessExpansionProfile, ConfirmationStatus, Contract, ProjectContext};
use uuid::Uuid;

use crate::app::{App, AppState};
use crate::contracts::{ActorType, CreateProjectRequest, EventType, ProjectEnvelope};
use crate::error::{AppError, AppResult};
use crate::http::extract::{dict_body, json_body, py_truthy, AuthUser, PathParams};
use crate::pyjson;
use crate::services::events::Emit;
use crate::store::repo::{users, User, PROFILES, PROJECTS};
use crate::store::{Query, Tx};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/projects", post(create_project).get(list_projects))
        .route("/api/v1/projects/{project_id}", get(get_project))
        .route(
            "/api/v1/projects/{project_id}/business-profile",
            put(save_profile),
        )
}

/// `_ensure_user`: a `users` row for the token's user (FK of `projects.owner_id`).
async fn ensure_user(tx: &mut dyn Tx, user_id: Uuid) -> AppResult<()> {
    if users::get(tx, user_id).await?.is_some() {
        return Ok(());
    }
    let sp = tx.savepoint().await?;
    let user = User {
        id: user_id,
        display_name: "Owner".into(),
        email: None,
        phone: None,
    };
    match users::insert(tx, &user).await {
        Ok(()) => tx.release_savepoint(sp).await?,
        // A concurrent request created it first.
        Err(e) if e.is_unique_violation("pk_users") => tx.rollback_to_savepoint(sp).await?,
        Err(e) => {
            tx.rollback_to_savepoint(sp).await?;
            return Err(e.into());
        }
    }
    Ok(())
}

/// `{**fields, "project_id": ..., "profile_id": ...}` validated as `BusinessExpansionProfile`
/// (422 `CONTRACT_VALIDATION_ERROR` on failure, as Pydantic's `model_validate` in the handler).
fn profile_from(
    mut fields: serde_json::Map<String, Value>,
    project_id: Uuid,
    profile_id: Option<Uuid>,
) -> AppResult<BusinessExpansionProfile> {
    fields.insert("project_id".into(), Value::String(project_id.to_string()));
    if let Some(id) = profile_id {
        fields.insert("profile_id".into(), Value::String(id.to_string()));
    }
    Ok(BusinessExpansionProfile::from_json_value(Value::Object(
        fields,
    ))?)
}

fn not_yours() -> AppError {
    AppError::not_found("business profile not found")
}

/// The stable digest used in the profile-confirmed idempotency key.
pub fn profile_digest(profile: &BusinessExpansionProfile) -> AppResult<String> {
    let v = serde_json::to_value(profile)
        .map_err(|e| AppError::internal(format!("serialize profile: {e}")))?;
    Ok(hex::encode(Sha256::digest(
        pyjson::dumps_canonical(&v).as_bytes(),
    )))
}

async fn emit_confirmed(
    app: &App,
    tx: &mut dyn Tx,
    user: Uuid,
    project_id: Uuid,
    profile: &BusinessExpansionProfile,
    key: String,
) -> AppResult<()> {
    app.events()
        .emit(
            tx,
            Emit::new(
                EventType::ProjectProfileConfirmed,
                project_id,
                ActorType::User,
                user.to_string(),
                key,
                json!({"profile_id": profile.profile_id.to_string()}),
            ),
        )
        .await?;
    Ok(())
}

async fn create_project(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<(StatusCode, Json<ProjectEnvelope>)> {
    let req: CreateProjectRequest = json_body(&headers, &body)?;
    let envelope = crate::in_tx!(app, |tx| {
        ensure_user(tx, user).await?;
        let mut project = ProjectContext::new(user, req.project_name, req.market);
        let mut profile = None;
        if let Some(fields) = req.business_profile.filter(|m| !m.is_empty()) {
            let p = profile_from(fields, project.project_id, None)?;
            project.business_profile_id = Some(p.profile_id);
            profile = Some(p);
        }
        PROJECTS.add(tx, &project).await?;
        if let Some(p) = &profile {
            if PROFILES.get(tx, p.profile_id).await?.is_some() {
                return Err(not_yours());
            }
            PROFILES.add(tx, p).await?;
            if p.confirmation_status == ConfirmationStatus::Confirmed {
                let key = format!("profile:{}:confirmed", p.profile_id);
                emit_confirmed(&app, tx, user, project.project_id, p, key).await?;
            }
        }
        ProjectEnvelope {
            project,
            business_profile: profile,
        }
    });
    Ok((StatusCode::CREATED, Json(envelope)))
}

async fn list_projects(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
) -> AppResult<Json<Vec<ProjectContext>>> {
    let projects = crate::in_tx!(app, |tx| {
        PROJECTS
            .list(
                tx,
                Query::new().eq("owner_id", user).order_asc("created_at"),
            )
            .await?
    });
    Ok(Json(projects))
}

async fn get_project(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
) -> AppResult<Json<ProjectEnvelope>> {
    let project_id = params.uuid("project_id")?;
    let envelope = crate::in_tx!(app, |tx| {
        let project = app.authz().project_for_user(tx, project_id, user).await?;
        let mut profiles = PROFILES
            .list(tx, Query::new().eq("project_id", project.project_id))
            .await?;
        ProjectEnvelope {
            project,
            business_profile: profiles.pop(),
        }
    });
    Ok(Json(envelope))
}

async fn save_profile(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Json<BusinessExpansionProfile>> {
    let project_id = params.uuid("project_id")?;
    let profile = crate::in_tx!(app, |tx| {
        let project = app.authz().project_for_user(tx, project_id, user).await?;
        let fields = dict_body(&headers, &body)?;
        // `body.get("profile_id") or uuid4()`: a falsy value means "new profile".
        let supplied = fields.get("profile_id").filter(|v| py_truthy(v)).is_some();
        let profile = profile_from(
            fields,
            project.project_id,
            if supplied { None } else { Some(Uuid::new_v4()) },
        )?;
        if let Some(existing) = PROFILES.get(tx, profile.profile_id).await? {
            if existing.project_id != project.project_id {
                return Err(not_yours());
            }
        }
        // Lock the project row before the first write, and build the update from the locked
        // row (part 2 review, F2). A PUT that overlaps a search then waits for it and writes onto
        // its result instead of putting back the copy read before the search committed (which
        // reverted `project_status`), and it takes the search route's lock order: the project
        // row, then the project's event lock (first emit).
        let (mut updated, _) = PROJECTS
            .get_for_update(tx, project.project_id)
            .await?
            .ok_or_else(|| AppError::not_found("project not found"))?;
        PROFILES.upsert(tx, &profile).await?;
        updated.business_profile_id = Some(profile.profile_id);
        PROJECTS.upsert(tx, &updated).await?;
        if profile.confirmation_status == ConfirmationStatus::Confirmed {
            let key = format!(
                "profile:{}:confirmed:{}",
                profile.profile_id,
                profile_digest(&profile)?
            );
            emit_confirmed(&app, tx, user, project.project_id, &profile, key).await?;
        }
        profile
    });
    Ok(Json(profile))
}
