//! The owner's recorded introduction (see [`crate::services::intro_recording`]).
//!
//! Bearer (the signed-in owner, only their own record):
//! - `POST /api/v1/owner/intro-recording`: multipart `file` (`audio/wav`, PCM 16-bit mono, 8 or
//!   16 kHz, 2 to 60 s, at most 2 MiB) and `affirmed=true`. 201 with the record. Without the
//!   affirmation 422; wrong type 400 `UNSUPPORTED_MEDIA`; bad WAV, too long or too short 400
//!   `INVALID_RECORDING`; too large 400 `FILE_TOO_LARGE`.
//! - `GET /api/v1/owner/intro-recording`: the active record and a 5-minute listen link; 404.
//! - `DELETE /api/v1/owner/intro-recording`: revoke; the audio is deleted, the record kept; 404.
//! - `GET /api/v1/owner/intro-recording/history`: replaced and revoked records (no audio).
//! - `POST /api/v1/owner/intro-recording/link`: a 30-minute, one-use recording link.
//!
//! Recording link (no sign-in; the token in the `X-Recording-Link` header, from the URL fragment):
//! - `GET /record`: the self-contained recording page.
//! - `POST /record/check`: `{"valid": bool}`.
//! - `POST /record/upload`: the same multipart body as the bearer upload; 201 `{"stored": true}`.
//!
//! Departure from the brief: the brief named `GET|POST /record/{token}`. The request logger
//! writes every path, so a token in the path would be in a log line; it is in the fragment
//! instead, which browsers never send.

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, REFERRER_POLICY};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::app::AppState;
use crate::error::{AppError, AppResult};
use crate::http::extract::AuthUser;
use crate::security;
use crate::services::intro_recording::{
    self as intro, IntroRecording, NewRecording, AFFIRMATION_BASIS, AFFIRMATION_STATEMENT,
    LINK_PURPOSE, LINK_TTL_S, LISTEN_TTL_S, MAX_BYTES,
};

const PAGE: &str = include_str!("recordings_page.html");
const LINK_HEADER: &str = "x-recording-link";
const MAX_REQUEST_BYTES: usize = MAX_BYTES + 64 * 1024;
pub const PAGE_CSP: &str = "default-src 'none'; script-src 'nonce-{N}'; style-src 'nonce-{N}'; \
media-src blob:; connect-src 'self'; img-src 'none'; form-action 'none'; base-uri 'none'; \
frame-ancestors 'none'";

pub fn router() -> Router<AppState> {
    let limit = DefaultBodyLimit::max(MAX_REQUEST_BYTES);
    Router::new()
        .route(
            "/api/v1/owner/intro-recording",
            post(upload)
                .get(current)
                .delete(revoke)
                .layer(limit),
        )
        .route("/api/v1/owner/intro-recording/history", get(history))
        .route("/api/v1/owner/intro-recording/link", post(new_link))
        .route("/record", get(page))
        .route("/record/check", post(check_link))
        .route("/record/upload", post(link_upload).layer(limit))
}

/// The record as a client sees it: never the storage key or a path.
fn view(r: &IntroRecording) -> Value {
    json!({
        "recording_id": r.recording_id,
        "status": r.status,
        "created_at": r.created_at,
        "content_type": r.content_type,
        "size_bytes": r.size_bytes,
        "duration_ms": r.duration_ms,
        "sample_rate_hz": r.sample_rate_hz,
        "source": r.source,
        "affirmation_statement": r.affirmation_statement,
        "affirmed_at": r.affirmed_at,
        "affirmation_basis": r.affirmation_basis,
        "voice_verified": false,
        "replaced_at": r.replaced_at,
        "revoked_at": r.revoked_at,
        "revoked_by": r.revoked_by,
        "audio_deleted": r.object_key.is_none(),
    })
}

struct Form {
    file: Option<(Option<String>, Vec<u8>)>,
    too_large: bool,
    affirmed: bool,
}

async fn read_form(app: &AppState, request: Request) -> AppResult<Form> {
    let ct = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !ct.starts_with("multipart/form-data") {
        return Err(AppError::request_validation(
            "missing",
            vec![json!("body"), json!("file")],
            "Field required",
        ));
    }
    let mut form = Multipart::from_request(request, app)
        .await
        .map_err(|_| AppError::bad_request("Invalid multipart data."))?;
    let mut out = Form {
        file: None,
        too_large: false,
        affirmed: false,
    };
    let too_large = || AppError::bad_request("file too large").code("FILE_TOO_LARGE");
    loop {
        let mut field = match form.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(too_large()),
            Err(_) => return Err(AppError::bad_request("Invalid multipart data.")),
        };
        let name = field.name().unwrap_or("").to_owned();
        let part_ct = field
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut data = Vec::new();
        let mut big = false;
        loop {
            match field.chunk().await {
                Ok(Some(c)) => {
                    if big || data.len() + c.len() > MAX_BYTES {
                        big = true;
                        data.clear();
                    } else {
                        data.extend_from_slice(&c);
                    }
                }
                Ok(None) => break,
                Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                    return Err(too_large())
                }
                Err(_) => return Err(AppError::bad_request("Invalid multipart data.")),
            }
        }
        match name.as_str() {
            "file" => {
                out.too_large = big;
                out.file = Some((part_ct, data));
            }
            "affirmed" => out.affirmed = data == b"true",
            _ => {}
        }
    }
    Ok(out)
}

async fn accept(
    app: &AppState,
    owner_id: Uuid,
    request: Request,
    source: &'static str,
    link_token: Option<String>,
) -> AppResult<IntroRecording> {
    let form = read_form(app, request).await?;
    let Some((ct, data)) = form.file else {
        return Err(AppError::request_validation(
            "missing",
            vec![json!("body"), json!("file")],
            "Field required",
        ));
    };
    if !form.affirmed {
        return Err(AppError::request_validation(
            "missing",
            vec![json!("body"), json!("affirmed")],
            format!("the owner must affirm: {AFFIRMATION_STATEMENT}"),
        ));
    }
    if ct.as_deref() != Some(intro::CONTENT_TYPE) {
        return Err(AppError::bad_request(format!(
            "unsupported content type {} (audio/wav only)",
            ct.as_deref().unwrap_or("None")
        ))
        .code("UNSUPPORTED_MEDIA"));
    }
    if form.too_large {
        return Err(AppError::bad_request("file too large").code("FILE_TOO_LARGE"));
    }
    let info =
        intro::check_wav(&data).map_err(|m| AppError::bad_request(m).code("INVALID_RECORDING"))?;
    intro::store(
        app,
        NewRecording {
            owner_id,
            data,
            source,
            link_token,
        },
        info,
    )
    .await
}

async fn upload(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    request: Request,
) -> AppResult<(StatusCode, Json<Value>)> {
    let rec = accept(&app, user, request, intro::SOURCE_UPLOAD, None).await?;
    Ok((StatusCode::CREATED, Json(view(&rec))))
}

fn none_found() -> AppError {
    AppError::not_found("no active intro recording").code("NOT_FOUND")
}

async fn current(State(app): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    let rec = crate::in_tx!(app, |tx| { intro::active(tx, user).await? });
    let rec = rec.ok_or_else(none_found)?;
    let mut v = view(&rec);
    if let Some(key) = rec.object_key.as_deref() {
        let token = security::sign_media_key(app.settings.auth_secret.expose(), key, LISTEN_TTL_S);
        v["listen_url"] = json!(format!(
            "{}/api/v1/media/public/{token}",
            app.settings.public_base_url
        ));
        v["listen_url_expires_in_s"] = json!(LISTEN_TTL_S);
    }
    Ok(Json(v))
}

async fn revoke(State(app): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    let rec = intro::revoke(&app, user).await?.ok_or_else(none_found)?;
    Ok(Json(view(&rec)))
}

async fn history(State(app): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    let recs = crate::in_tx!(app, |tx| { intro::history(tx, user).await? });
    Ok(Json(
        json!({"items": recs.iter().map(view).collect::<Vec<_>>()}),
    ))
}

async fn new_link(State(app): State<AppState>, AuthUser(user): AuthUser) -> AppResult<Json<Value>> {
    let token =
        security::issue_token_with(app.settings.auth_secret.expose(), user, LINK_TTL_S, LINK_PURPOSE);
    Ok(Json(json!({
        "url": format!("{}/record#{token}", app.settings.public_base_url),
        "expires_in_s": LINK_TTL_S,
        "single_use": true,
        "purpose": "upload this owner's recorded introduction, nothing else",
        "sent": false,
    })))
}

fn secure_headers(resp: &mut Response) {
    let h = resp.headers_mut();
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        "permissions-policy",
        HeaderValue::from_static("microphone=(self), camera=(), geolocation=()"),
    );
}

async fn page() -> Response {
    let nonce = security::opaque_reference();
    let nonce = nonce.trim_start_matches("ref_");
    let html = PAGE
        .replace("{NONCE}", nonce)
        .replace("{STATEMENT}", AFFIRMATION_STATEMENT)
        .replace("{EXAMPLE}", intro::EXAMPLE_WORDING)
        .replace("{BASIS}", AFFIRMATION_BASIS);
    let mut resp = html.into_response();
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    if let Ok(v) = HeaderValue::from_str(&PAGE_CSP.replace("{N}", nonce)) {
        resp.headers_mut().insert("content-security-policy", v);
    }
    secure_headers(&mut resp);
    resp
}

/// The owner a recording-link token speaks for, if it is valid and unused.
async fn link_owner(app: &AppState, headers: &HeaderMap) -> AppResult<Option<(Uuid, String)>> {
    let Some(token) = headers
        .get(LINK_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok(None);
    };
    let Ok(owner) = security::verify_token(app.settings.auth_secret.expose(), token, LINK_PURPOSE)
    else {
        return Ok(None);
    };
    let usable = crate::in_tx!(app, |tx| {
        crate::store::repo::users::get(tx, owner).await?.is_some()
            && !intro::link_used(tx, token).await?
    });
    Ok(usable.then(|| (owner, token.to_owned())))
}

fn invalid_link() -> AppError {
    AppError::unauthorized("This recording link is no longer valid.").code("LINK_INVALID")
}

async fn check_link(State(app): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let valid = link_owner(&app, &headers).await?.is_some();
    let mut resp = Json(json!({ "valid": valid })).into_response();
    secure_headers(&mut resp);
    Ok(resp)
}

async fn link_upload(State(app): State<AppState>, request: Request) -> AppResult<Response> {
    let (owner, token) = link_owner(&app, request.headers())
        .await?
        .ok_or_else(invalid_link)?;
    accept(&app, owner, request, intro::SOURCE_LINK, Some(token)).await?;
    let mut resp = (StatusCode::CREATED, Json(json!({ "stored": true }))).into_response();
    secure_headers(&mut resp);
    Ok(resp)
}
