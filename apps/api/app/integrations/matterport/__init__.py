"""Matterport spatial assets. STATUS: BLOCKED (sponsor-provided models not yet accessed).

Interface only + fixture geometry. The live implementation must use Matterport's documented
Model API (GraphQL) once a token and model IDs are available. Do not invent queries here.
"""

from __future__ import annotations

import json
from typing import Protocol

from app.contracts import IntegrationStatus, ProviderStatusEntry
from app.integrations.base import FIXTURES_DIR, status_entry


class MatterportProvider(Protocol):
    mode: str

    def get_floor_geometry(self, asset_id: str) -> dict | None: ...

    def status(self) -> ProviderStatusEntry: ...


class FixtureMatterportProvider:
    mode = "fixture"

    def get_floor_geometry(self, asset_id: str) -> dict | None:
        for f in (FIXTURES_DIR / "geometry").glob("*.json"):
            doc = json.loads(f.read_text())
            if doc.get("source_id") == asset_id:
                return doc
        return None

    def status(self):
        return status_entry(
            "matterport",
            self.mode,
            IntegrationStatus.MOCK,
            False,
            "Synthetic rectangle geometry; real model not yet accessed.",
        )
