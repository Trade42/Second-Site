from __future__ import annotations

from fastapi import APIRouter, Depends, File, UploadFile

from app.api.deps import container, owned_project
from app.contracts import Contract, ProjectContext
from app.core.container import Container
from app.core.errors import AppError

router = APIRouter(prefix="/api/v1/projects/{project_id}", tags=["media"])
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
