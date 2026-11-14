//! Port of `api/routes/dev.py`: development-only helpers (dev tokens are disabled unless
//! `ALLOW_DEV_TOKENS=true` and `APP_ENV` is not `production`).

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use uuid::Uuid;

use crate::app::AppState;
use crate::contracts::DevTokenResponse;
use crate::error::{AppError, AppResult};
use crate::security;
use crate::store::repo::{users, User};
use crate::store::Tx;

pub const DEMO_EMAIL: &str = "demo-owner@second-site.invalid";

/// The facial-studio demo inputs (FIXTURE_SYNTHETIC, labelled so in `_meta`), embedded at build
/// time from `fixtures/projects/facial_studio.json`.
pub const DEMO_PROJECT_FIXTURE: &str =
    include_str!("../../../../../../../fixtures/projects/facial_studio.json");

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/dev/token", post(dev_token))
        .route("/api/v1/dev/fixtures/project", get(demo_project_fixture))
}

async fn demo_user(tx: &mut dyn Tx) -> AppResult<User> {
    if let Some(u) = users::find_by_email(tx, DEMO_EMAIL).await? {
        return Ok(u);
    }
    let user = User {
        id: Uuid::new_v4(),
        display_name: "Demo Owner".into(),
        email: Some(DEMO_EMAIL.into()),
        phone: None,
    };
    let sp = tx.savepoint().await?;
    match users::insert(tx, &user).await {
        Ok(()) => {
            tx.release_savepoint(sp).await?;
            Ok(user)
        }
        Err(e) if e.is_unique_violation("uq_users_email") => {
            // A concurrent first call created it.
            tx.rollback_to_savepoint(sp).await?;
            users::find_by_email(tx, DEMO_EMAIL)
                .await?
                .ok_or_else(|| AppError::internal("demo user vanished"))
        }
        Err(e) => {
            tx.rollback_to_savepoint(sp).await?;
            Err(e.into())
        }
    }
}

async fn dev_token(State(app): State<AppState>) -> AppResult<Json<DevTokenResponse>> {
    if !app.settings.allow_dev_tokens || app.settings.app_env == "production" {
        return Err(AppError::forbidden("dev tokens disabled"));
    }
    let user = crate::in_tx!(app, |tx| { demo_user(tx).await? });
    Ok(Json(DevTokenResponse {
        user_id: user.id,
        access_token: security::issue_token(app.settings.auth_secret.expose(), user.id),
        token_type: "bearer".into(),
    }))
}

/// Python returns `json.loads(file)` re-serialized compactly; this returns the same document
/// minified with its key order preserved.
async fn demo_project_fixture() -> Response {
    (
        [(CONTENT_TYPE, "application/json")],
        minify_json(DEMO_PROJECT_FIXTURE),
    )
        .into_response()
}

/// Remove insignificant whitespace from JSON text (keeps key order and number spelling).
pub fn minify_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_str = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_is_valid_json_and_labelled() {
        let min = minify_json(DEMO_PROJECT_FIXTURE);
        let v: serde_json::Value = serde_json::from_str(&min).unwrap();
        let orig: serde_json::Value = serde_json::from_str(DEMO_PROJECT_FIXTURE).unwrap();
        assert_eq!(v, orig);
        assert_eq!(v["_meta"]["data_origin"], "FIXTURE_SYNTHETIC");
        assert!(min.contains("\"project_name\":\"Facial studio — second location\""));
    }
}
