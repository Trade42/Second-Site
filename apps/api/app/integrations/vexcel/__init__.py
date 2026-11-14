"""Vexcel aerial imagery. STATUS: BLOCKED (sponsor access not yet verified). Interface + fixture only."""

from __future__ import annotations

from typing import Protocol

from app.contracts import IntegrationStatus, ProviderStatusEntry
from app.integrations.base import status_entry


class VexcelProvider(Protocol):
    mode: str

    def list_assets(self, latitude: float, longitude: float) -> list[dict]: ...

    def status(self) -> ProviderStatusEntry: ...


class FixtureVexcelProvider:
    mode = "fixture"

    def list_assets(self, latitude: float, longitude: float) -> list[dict]:
        return []  # exterior assets come from the property fixture for now

    def status(self):
        return status_entry(
            "vexcel", self.mode, IntegrationStatus.MOCK, False, "No live imagery; placeholders only."
        )
