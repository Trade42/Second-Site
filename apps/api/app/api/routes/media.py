from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends, File, Response, UploadFile
from sqlalchemy.orm import Session

from app.api.deps import container, db_session, owned_project
from app.contracts import Contract, ProjectContext
from app.core.container import Container
from app.core.errors import AppError, NotFoundError
from app.integrations.base import ProviderError
from app.repositories import registry as repos

router = APIRouter(prefix="/api/v1/projects/{project_id}", tags=["media"])
imagery_router = APIRouter(prefix="/api/v1/media", tags=["media"])
MAX_BYTES = 15 * 1024 * 1024
ALLOWED = {"image/jpeg", "image/png", "image/heic", "audio/mpeg", "audio/wav", "audio/mp4", "application/pdf"}


class MediaStored(Contract):
    object_key: str
    content_type: str
    size_bytes: int


@router.post("/media", response_model=MediaStored, status_code=201)
async def upload(
    file: UploadFile = File(...),
    project: ProjectContext = Depends(owned_project),
    c: Container = Depends(container),
) -> MediaStored:
    """Owner uploads (photos, genuine intro recordings, plans). Stored privately; never committed."""
    if file.content_type not in ALLOWED:
        raise AppError(f"unsupported content type {file.content_type}", error_code="UNSUPPORTED_MEDIA")
    data = await file.read()
    if len(data) > MAX_BYTES:
        raise AppError("file too large", error_code="FILE_TOO_LARGE")
    key = c.storage.put(str(project.project_id), file.filename or "upload", data, file.content_type)
    return MediaStored(object_key=key, content_type=file.content_type, size_bytes=len(data))


@imagery_router.get(
    "/vexcel/ortho/{property_id}",
    response_class=Response,
    responses={200: {"content": {"image/jpeg": {}}}},
)
def vexcel_ortho(
    property_id: UUID, db: Session = Depends(db_session), c: Container = Depends(container)
) -> Response:
    """Aerial orthomosaic around a stored candidate property. Proxied so the Vexcel key never reaches the
    browser; only coordinates of properties already in the database can be requested (no open proxy)."""
    prop = repos.properties.get(db, property_id)
    ident = prop.property_identity if prop else None
    if ident is None or ident.latitude is None or ident.longitude is None:
        raise NotFoundError("property not found", error_code="NOT_FOUND")
    try:
        img = c.vexcel.ortho_image(ident.latitude, ident.longitude)
    except ProviderError as exc:
        raise AppError(f"imagery unavailable ({exc.kind})", error_code="PROVIDER_UNAVAILABLE") from exc
    if not img:
        raise NotFoundError("no imagery in this mode", error_code="NOT_FOUND")
    return Response(img, media_type="image/jpeg", headers={"Cache-Control": "private, max-age=86400"})
