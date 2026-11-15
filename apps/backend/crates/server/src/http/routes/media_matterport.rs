//! `GET /api/v1/media/matterport/{asset_id}/…`: a Matterport scan's interior (mesh, photos,
//! model still, floor plan) served by this service. **A deliberate addition, not a port**: the
//! Python backend has no such route.
//!
//! - `manifest.json`: what the scan offers (`data_origin: LIVE`, `provider: matterport`).
//! - `mesh.obj` (`model/obj`, the `50k` OBJ unmodified), `photo/{n}.jpg` (1-based, the API's
//!   order, `web` resolution), `image.jpg` (the model still, `web` resolution), `floorplan.png`
//!   (the PNG at 100 px/m, the one flagged `alpha` preferred, else the first such PNG).
//!
//! No bearer token (a 3D loader and `<img>` tags load these). It is not an open proxy: the id
//! must be the id of a `provider: "matterport"` interior asset on some stored property, checked
//! with one bounded query (the newest [`GUARD_SCAN_LIMIT`] properties) whose positive answer is
//! remembered for [`GUARD_TTL`]. Matterport's links are signed and live about five minutes, so
//! the service fetches each file once, with no credentials, into
//! `OBJECT_STORAGE_LOCAL_DIR/matterport/<asset_id>/` (written to a temporary name, then
//! renamed) and serves the stored copy afterwards. No credential, signed link or query string
//! leaves the service, and nothing redirects to the CDN.
//!
//! The route does not say whose space a scan shows (the listed suite or another space in the
//! building): the caller knows that from the property's `matches_listing_unit`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};

use crate::app::AppState;
use crate::error::{AppError, AppResult};
use crate::http::extract::PathParams;
use crate::integrations::blocking;
use crate::integrations::matterport::{safe_id, ModelMedia, MEDIA_TOO_LARGE};
use crate::integrations::MatterportProvider;
use crate::store::repo::PROPERTIES;
use crate::store::Query;

const CACHE: &str = "private, max-age=3600";
/// The largest mesh stored (the 50k OBJs seen were about 1.3 MB).
pub const MAX_MESH_BYTES: usize = 40 * 1024 * 1024;
/// The largest photo, still or floor plan stored (a `web` photo is about 200 KB).
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// The stored-asset check reads at most this many properties, newest first.
pub const GUARD_SCAN_LIMIT: i64 = 5000;
/// How long a passed stored-asset check is remembered.
pub const GUARD_TTL: Duration = Duration::from_secs(300);
/// How long a stored manifest is served before one fresh assets query rebuilds it.
pub const MANIFEST_TTL: Duration = Duration::from_secs(3600);
/// Photo numbers above this are refused without any lookup.
const MAX_PHOTO: usize = 500;

pub fn router() -> Router<AppState> {
    let base = "/api/v1/media/matterport/{asset_id}";
    Router::new()
        .route(&format!("{base}/manifest.json"), get(manifest))
        .route(&format!("{base}/mesh.obj"), get(mesh))
        .route(&format!("{base}/image.jpg"), get(image))
        .route(&format!("{base}/floorplan.png"), get(floorplan))
        .route(&format!("{base}/photo/{{file}}"), get(photo))
        .route(&format!("{base}/{{*rest}}"), get(no_such_file))
}

/// Any other file under a scan: 404 `MEDIA_NOT_FOUND` before any lookup (not the router's bare 404).
async fn no_such_file() -> AppResult<Response> {
    Err(media_not_found(
        "no such interior media file: manifest.json, mesh.obj, photo/{n}.jpg, image.jpg or floorplan.png",
    ))
}

fn media_not_found(msg: impl Into<String>) -> AppError {
    AppError::not_found(msg).code("MEDIA_NOT_FOUND")
}

fn geometry_unavailable() -> AppError {
    AppError::not_found("Matterport does not know this model, or this token cannot see it")
        .code("GEOMETRY_UNAVAILABLE")
}

/// The ortho route's mapping (400 `PROVIDER_UNAVAILABLE`, the kind in the message), with
/// `retryable` from the kind. The provider's own message is not passed on.
fn provider_unavailable(e: &ProviderError) -> AppError {
    AppError::bad_request(format!("interior media unavailable ({})", e.kind))
        .code("PROVIDER_UNAVAILABLE")
        .with_retryable(e.retryable())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Mesh,
    Image,
    Floorplan,
    Photo(usize),
}

impl Item {
    fn file(self) -> String {
        match self {
            Item::Mesh => "mesh.obj".into(),
            Item::Image => "image.jpg".into(),
            Item::Floorplan => "floorplan.png".into(),
            Item::Photo(n) => format!("photo-{n}.jpg"),
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Item::Mesh => "model/obj",
            Item::Floorplan => "image/png",
            Item::Image | Item::Photo(_) => "image/jpeg",
        }
    }

    fn limit(self) -> usize {
        match self {
            Item::Mesh => MAX_MESH_BYTES,
            _ => MAX_IMAGE_BYTES,
        }
    }

    fn what(self) -> &'static str {
        match self {
            Item::Mesh => "a 50k OBJ mesh",
            Item::Image => "a model still",
            Item::Floorplan => "a 100 px/m PNG floor plan",
            Item::Photo(_) => "a photo with that number",
        }
    }

    fn link(self, m: &ModelMedia) -> Option<String> {
        match self {
            Item::Mesh => m.mesh_url.clone(),
            Item::Image => m.image_url.clone(),
            Item::Floorplan => m.floorplan.as_ref().map(|f| f.url.clone()),
            Item::Photo(n) => m.photo_urls.get(n - 1).cloned(),
        }
    }
}

fn respond(data: Vec<u8>, media_type: &'static str) -> Response {
    let mut resp = Body::from(data).into_response();
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(media_type));
    h.insert(CACHE_CONTROL, HeaderValue::from_static(CACHE));
    resp
}

// ---- the guard ----------------------------------------------------------------------------------

/// Asset ids that passed the stored-asset check, and when.
#[derive(Default)]
struct GuardMemo(Mutex<HashMap<String, Instant>>);

fn memo(app: &AppState) -> Arc<GuardMemo> {
    if let Some(m) = app.extensions.get::<GuardMemo>() {
        return m;
    }
    app.extensions.insert(Arc::new(GuardMemo::default()));
    app.extensions.get::<GuardMemo>().unwrap_or_default()
}

/// 404 `MEDIA_NOT_FOUND` unless `id` is a `matterport` interior asset of a stored property.
async fn ensure_stored(app: &AppState, id: &str) -> AppResult<()> {
    let memo = memo(app);
    {
        let mut seen = memo.0.lock().unwrap_or_else(|e| e.into_inner());
        match seen.get(id) {
            Some(at) if at.elapsed() < GUARD_TTL => return Ok(()),
            Some(_) => {
                seen.remove(id);
            }
            None => {}
        }
    }
    let props = crate::in_tx!(app, |tx| {
        PROPERTIES
            .list(
                tx,
                Query::new()
                    .order_desc("created_at")
                    .limit(GUARD_SCAN_LIMIT),
            )
            .await?
    });
    let stored = props.iter().any(|p| {
        p.interior_assets
            .iter()
            .any(|a| a.provider == "matterport" && a.asset_id == id)
    });
    if !stored {
        return Err(media_not_found(
            "no stored property has a Matterport interior asset with this id",
        ));
    }
    memo.0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_owned(), Instant::now());
    Ok(())
}

// ---- files --------------------------------------------------------------------------------------

/// One lock per stored file path, so two first requests download once.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_for(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(path.to_path_buf())
        .or_default()
        .clone()
}

fn io_error(e: &std::io::Error) -> AppError {
    AppError::internal_kind("OSError", e.to_string())
}

/// Write to a temporary name in the same directory, then rename: a reader sees the old state
/// or the whole file, never part of it.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.part", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn read_if_present(path: &Path) -> AppResult<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(&e)),
    }
}

fn fresh_manifest(path: &Path) -> AppResult<Option<Vec<u8>>> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(None);
    };
    let age = meta
        .modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or(Duration::MAX);
    if age >= MANIFEST_TTL {
        return Ok(None);
    }
    read_if_present(path)
}

/// The scan's links, or 404 `GEOMETRY_UNAVAILABLE` when Matterport does not know it.
fn links(mp: &dyn MatterportProvider, id: &str, fresh: bool) -> AppResult<ModelMedia> {
    mp.model_media(id, fresh)
        .map_err(|e| provider_unavailable(&e))?
        .ok_or_else(geometry_unavailable)
}

/// A signed link that stopped working before it was fetched answers 401/403/404 from the CDN.
fn link_expired(e: &ProviderError) -> bool {
    matches!(
        e.kind,
        ProviderErrorKind::Auth | ProviderErrorKind::NotFound
    )
}

/// Fetch one item (blocking): the cached links, the download, and on an expired link one fresh
/// assets query and a second download. Then the size check and the atomic write.
fn fetch(mp: &dyn MatterportProvider, id: &str, item: Item, path: &Path) -> AppResult<Vec<u8>> {
    let media = links(mp, id, false)?;
    let not_offered = || media_not_found(format!("this scan has no {}", item.what()));
    let url = item.link(&media).ok_or_else(not_offered)?;
    let oversize = || {
        AppError::bad_request(format!(
            "interior media unavailable (BAD_RESPONSE): the file is larger than the {} MB limit",
            item.limit() / (1024 * 1024)
        ))
        .code("PROVIDER_UNAVAILABLE")
        .with_retryable(false)
    };
    let failed = |e: &ProviderError| {
        if e.message == MEDIA_TOO_LARGE {
            oversize()
        } else {
            provider_unavailable(e)
        }
    };
    let data = match mp.download_media(&url, item.limit()) {
        Ok(d) => d,
        Err(e) if link_expired(&e) => {
            let media = links(mp, id, true)?;
            let url = item.link(&media).ok_or_else(not_offered)?;
            mp.download_media(&url, item.limit())
                .map_err(|e| failed(&e))?
        }
        Err(e) => return Err(failed(&e)),
    };
    if data.len() > item.limit() {
        return Err(oversize());
    }
    write_atomic(path, &data).map_err(|e| io_error(&e))?;
    Ok(data)
}

fn manifest_body(id: &str, m: &ModelMedia) -> Value {
    let fp = m.floorplan.as_ref();
    json!({
        "asset_id": id,
        "provider": "matterport",
        "data_origin": "LIVE",
        "name": m.name,
        "mesh": {"available": m.mesh_url.is_some(), "format": "obj", "resolution": "50k", "units": "m", "up_axis": "z"},
        "photos": {"count": m.photo_urls.len()},
        "image": {"available": m.image_url.is_some()},
        "floorplan": {
            "available": fp.is_some(),
            "resolution_px_per_m": fp.and_then(|f| f.resolution_px_per_m),
            "origin": fp.and_then(|f| f.origin).map(|(x, y)| json!({"x": x, "y": y})),
            "width_px": fp.and_then(|f| f.width_px),
            "height_px": fp.and_then(|f| f.height_px),
        },
        "retrieved_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

struct Ctx {
    id: String,
    dir: PathBuf,
    mp: Arc<dyn MatterportProvider>,
}

/// The checks every path makes before any provider request: the id syntax, live mode, the
/// stored-asset guard.
async fn prepare(app: &AppState, params: &PathParams) -> AppResult<Ctx> {
    let raw = params.raw("asset_id").unwrap_or_default();
    let id = safe_id(&raw)
        .map_err(|_| media_not_found("not a Matterport model id"))?
        .to_owned();
    let mp = app.integrations().matterport.clone();
    if !mp.media_enabled() {
        return Err(media_not_found(
            "live Matterport is not switched on (MATTERPORT_PROVIDER_MODE is not live, or the API credentials are missing)",
        ));
    }
    ensure_stored(app, &id).await?;
    let dir = Path::new(&app.settings.object_storage_local_dir)
        .join("matterport")
        .join(&id);
    Ok(Ctx { id, dir, mp })
}

async fn serve_item(app: AppState, params: PathParams, item: Item) -> AppResult<Response> {
    let ctx = prepare(&app, &params).await?;
    let path = ctx.dir.join(item.file());
    if let Item::Photo(n) = item {
        // A stored manifest already says how many photos there are.
        let mpath = ctx.dir.join("manifest.json");
        let known = blocking(move || fresh_manifest(&mpath)).await??;
        let count = known
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["photos"]["count"].as_u64());
        if count.is_some_and(|c| n as u64 > c) {
            return Err(media_not_found(format!("this scan has no {}", item.what())));
        }
    }
    let p = path.clone();
    if let Some(data) = blocking(move || read_if_present(&p)).await?? {
        return Ok(respond(data, item.content_type()));
    }
    let lock = lock_for(&path);
    let _guard = lock.lock().await;
    let Ctx { id, mp, .. } = ctx;
    let data = blocking(move || match read_if_present(&path)? {
        Some(d) => Ok(d),
        None => fetch(mp.as_ref(), &id, item, &path),
    })
    .await??;
    Ok(respond(data, item.content_type()))
}

async fn manifest(State(app): State<AppState>, params: PathParams) -> AppResult<Response> {
    let ctx = prepare(&app, &params).await?;
    let path = ctx.dir.join("manifest.json");
    let p = path.clone();
    if let Some(data) = blocking(move || fresh_manifest(&p)).await?? {
        return Ok(respond(data, "application/json"));
    }
    let lock = lock_for(&path);
    let _guard = lock.lock().await;
    let Ctx { id, mp, .. } = ctx;
    let data = blocking(move || -> AppResult<Vec<u8>> {
        if let Some(d) = fresh_manifest(&path)? {
            return Ok(d);
        }
        let media = links(mp.as_ref(), &id, false)?;
        let body = serde_json::to_vec(&manifest_body(&id, &media))
            .map_err(|e| AppError::internal(format!("manifest: {e}")))?;
        write_atomic(&path, &body).map_err(|e| io_error(&e))?;
        Ok(body)
    })
    .await??;
    Ok(respond(data, "application/json"))
}

async fn mesh(State(app): State<AppState>, params: PathParams) -> AppResult<Response> {
    serve_item(app, params, Item::Mesh).await
}

async fn image(State(app): State<AppState>, params: PathParams) -> AppResult<Response> {
    serve_item(app, params, Item::Image).await
}

async fn floorplan(State(app): State<AppState>, params: PathParams) -> AppResult<Response> {
    serve_item(app, params, Item::Floorplan).await
}

/// `photo/{n}.jpg`, `n` from 1; anything else is 404 `MEDIA_NOT_FOUND` before any lookup.
async fn photo(State(app): State<AppState>, params: PathParams) -> AppResult<Response> {
    let file = params.raw("file").unwrap_or_default();
    let n = file
        .strip_suffix(".jpg")
        .filter(|d| !d.is_empty() && d.len() <= 4 && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse::<usize>().ok())
        .filter(|n| (1..=MAX_PHOTO).contains(n))
        .ok_or_else(|| media_not_found("photo numbers start at 1: photo/{n}.jpg"))?;
    serve_item(app, params, Item::Photo(n)).await
}
