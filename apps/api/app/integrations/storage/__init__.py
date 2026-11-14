"""Object storage for uploads and owner recordings. Local filesystem in dev (gitignored)."""

from __future__ import annotations

import hashlib
from pathlib import Path
from typing import Protocol


class ObjectStorageProvider(Protocol):
    def put(self, project_id: str, filename: str, data: bytes, content_type: str) -> str: ...

    def get(self, key: str) -> bytes: ...


class LocalObjectStorage:
    def __init__(self, root: str):
        self.root = Path(root)

    def put(self, project_id, filename, data, content_type):
        digest = hashlib.sha256(data).hexdigest()[:16]
        safe = "".join(c for c in filename if c.isalnum() or c in "._-")[:80] or "upload"
        key = f"{project_id}/{digest}-{safe}"
        path = self.root / key
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return key

    def get(self, key):
        path = (self.root / key).resolve()
        if self.root.resolve() not in path.parents:
            raise ValueError("invalid key")
        return path.read_bytes()
