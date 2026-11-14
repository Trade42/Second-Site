//! Port of `api/routes/media.py`.
//!
//! - `POST /api/v1/projects/{project_id}/media`: owner uploads (photos, genuine intro
//!   recordings, plans), multipart field `file`, stored privately; same allowed content types,
//!   15 MiB limit and error codes as Python. 201 `MediaStored`.
//! - `GET /api/v1/media/vexcel/ortho/{property_id}`: aerial orthomosaic around a stored property.
//!   No bearer auth (an `<img>` tag loads it); only properties already in the database with
//!   coordinates can be requested, so it is not an open proxy; the Vexcel key stays on the server.
//! - `GET /api/v1/media/public/{token}`: a short-lived signed link to one stored object (the
//!   owner's recorded intro the carrier fetches). `Cache-Control: no-store`.
//!
//! Differences from Python on the upload: authentication and the project are checked before the
//! multipart body is read (Python parses the whole form first, so a malformed body without a
//! token is a 400 there and a 401 here); a malformed multipart body is a `ProblemDetail` 400
//! (Python: Starlette's `{"detail": ...}`); a `file` field that is not a file is a 422 (Python
//! answers 500 because its validation handler cannot serialize the error). On the signed link, a
//! key that resolves to a directory is a 404 (Python: 500).

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::AppState;
use crate::contracts::plain_contract;
use crate::error::{AppError, AppResult};
use crate::http::extract::{AuthUser, PathParams};
use crate::integrations::storage::StorageError;
use crate::integrations::vexcel::{ORTHO_PIXELS, ORTHO_SIZE_M};
use crate::integrations::blocking;
use crate::security;
use crate::store::repo::PROPERTIES;

pub const MAX_BYTES: usize = 15 * 1024 * 1024;
pub const ALLOWED: [&str; 7] = [
    "image/jpeg",
    "image/png",
    "image/heic",
    "audio/mpeg",
    "audio/wav",
    "audio/mp4",
    "application/pdf",
];

/// Multipart bodies larger than this are refused as too large without being read further.
const MAX_REQUEST_BYTES: usize = 2 * MAX_BYTES + 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaStored {
    pub object_key: String,
    pub content_type: String,
    pub size_bytes: i64,
}

plain_contract!(MediaStored);

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/projects/{project_id}/media",
            post(upload).layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES)),
        )
        .route(
            "/api/v1/media/vexcel/ortho/{property_id}",
            get(vexcel_ortho),
        )
        .route("/api/v1/media/public/{token}", get(signed_object))
}

fn missing_file() -> AppError {
    AppError::request_validation("missing", vec![json!("body"), json!("file")], "Field required")
}

fn bad_multipart(e: &axum::extract::multipart::MultipartError) -> AppError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        AppError::bad_request("file too large").code("FILE_TOO_LARGE")
    } else {
        AppError::bad_request("Invalid multipart data.")
    }
}

/// One `file` part as Starlette hands it over.
struct FilePart {
    filename: Option<String>,
    content_type: Option<String>,
    data: Vec<u8>,
    too_large: bool,
}

async fn upload(
    State(app): State<AppState>,
    AuthUser(user): AuthUser,
    params: PathParams,
    request: Request,
) -> AppResult<(StatusCode, Json<MediaStored>)> {
    let project_id = params.uuid("project_id")?;
    let project_id = crate::in_tx!(app, |tx| {
        app.authz()
            .project_for_user(tx, project_id, user)
            .await?
            .project_id
    });
    let ct = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !ct.starts_with("multipart/form-data") {
        // Starlette parses any other body as an empty form.
        return Err(missing_file());
    }
    if !ct.contains("boundary=") {
        return Err(AppError::bad_request("Missing boundary in multipart."));
    }
    let mut form = Multipart::from_request(request, &app)
        .await
        .map_err(|_| AppError::bad_request("Missing boundary in multipart."))?;
    // `form.get("file")`: the last part named `file` wins.
    let mut last: Option<Result<FilePart, String>> = None;
    loop {
        let field = match form.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(bad_multipart(&e)),
        };
        if field.name() != Some("file") {
            continue;
        }
        let filename = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        let is_file = filename.is_some();
        let mut part = FilePart {
            filename,
            content_type,
            data: Vec::new(),
            too_large: false,
        };
        let mut field = field;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if part.too_large || part.data.len() + chunk.len() > MAX_BYTES {
                        part.too_large = true;
                        part.data.clear();
                    } else {
                        part.data.extend_from_slice(&chunk);
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(bad_multipart(&e)),
            }
        }
        last = Some(if is_file {
            Ok(part)
        } else {
            Err(String::from_utf8_lossy(&part.data).into_owned())
        });
    }
    let part = match last {
        None => return Err(missing_file()),
        Some(Err(text)) => {
            return Err(AppError::RequestValidation(vec![json!({
                "type": "value_error",
                "loc": ["body", "file"],
                "msg": "Value error, Expected UploadFile, received: <class 'str'>",
                "input": text,
            })]))
        }
        Some(Ok(p)) => p,
    };
    let content_type = match part.content_type.as_deref() {
        Some(c) if ALLOWED.contains(&c) => c.to_owned(),
        other => {
            return Err(AppError::bad_request(format!(
                "unsupported content type {}",
                other.unwrap_or("None")
            ))
            .code("UNSUPPORTED_MEDIA"))
        }
    };
    if part.too_large {
        return Err(AppError::bad_request("file too large").code("FILE_TOO_LARGE"));
    }
    let storage = app.integrations().storage.clone();
    let filename = part
        .filename
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "upload".to_owned());
    let size = part.data.len() as i64;
    let ct = content_type.clone();
    let key = blocking(move || {
        storage.put(&project_id.to_string(), &filename, &part.data, &ct)
    })
    .await?
    .map_err(|e| AppError::internal_kind("OSError", e.to_string()))?;
    Ok((
        StatusCode::CREATED,
        Json(MediaStored {
            object_key: key,
            content_type,
            size_bytes: size,
        }),
    ))
}

fn binary(data: Vec<u8>, media_type: &'static str, cache: &'static str) -> Response {
    let mut resp = Body::from(data).into_response();
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(media_type));
    h.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}

async fn vexcel_ortho(State(app): State<AppState>, params: PathParams) -> AppResult<Response> {
    let property_id = params.uuid("property_id")?;
    let prop = crate::in_tx!(app, |tx| { PROPERTIES.get(tx, property_id).await? });
    let coords = prop.and_then(|p| {
        let i = p.property_identity;
        i.latitude.zip(i.longitude)
    });
    let Some((lat, lon)) = coords else {
        return Err(AppError::not_found("property not found").code("NOT_FOUND"));
    };
    let vexcel = app.integrations().vexcel.clone();
    let img = blocking(move || vexcel.ortho_image(lat, lon, ORTHO_SIZE_M, ORTHO_PIXELS))
        .await?
        .map_err(|e| {
            AppError::bad_request(format!("imagery unavailable ({})", e.kind))
                .code("PROVIDER_UNAVAILABLE")
        })?;
    match img.filter(|b| !b.is_empty()) {
        Some(bytes) => Ok(binary(bytes, "image/jpeg", "private, max-age=86400")),
        None => Err(AppError::not_found("no imagery in this mode").code("NOT_FOUND")),
    }
}

async fn signed_object(State(app): State<AppState>, params: PathParams) -> AppResult<Response> {
    let token = params.raw("token").unwrap_or_default();
    let key = security::verify_media_key(app.settings.auth_secret.expose(), &token)?;
    let storage = app.integrations().storage.clone();
    let k = key.clone();
    let data = blocking(move || storage.get(&k)).await?.map_err(|e| match e {
        StorageError::NotFound => AppError::not_found("object not found"),
        StorageError::Io(m) => AppError::internal_kind("OSError", m),
    })?;
    let media = if key.ends_with(".mp3") {
        "audio/mpeg"
    } else if key.ends_with(".wav") {
        "audio/wav"
    } else {
        "application/octet-stream"
    };
    Ok(binary(data, media, "no-store"))
}
