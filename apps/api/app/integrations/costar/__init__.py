"""CoStar sponsor toolkit adapter.

STATUS: BLOCKED on sponsor access. The CoStar hackathon toolkit (SDK / MCP / API) has not been
inspected, so NO live method is implemented here. Do not invent endpoints. When access is granted,
implement `LiveCoStarProvider` against the documented toolkit and keep the `CoStarProvider` protocol.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Protocol

from app.contracts import DataOrigin, IntegrationStatus, PropertySearchRequest, ProviderStatusEntry
from app.integrations.base import FIXTURES_DIR, ProviderError, ProviderErrorKind, status_entry


@dataclass(frozen=True)
class RawListing:
    provider: str
    provider_listing_id: str
    data_origin: DataOrigin
    address: dict[str, Any]
    listing: dict[str, Any]
    interior_assets: list[dict[str, Any]] = field(default_factory=list)
    exterior_assets: list[dict[str, Any]] = field(default_factory=list)


class CoStarProvider(Protocol):
    mode: str

    def search(self, request: PropertySearchRequest) -> list[RawListing]: ...

    def status(self) -> ProviderStatusEntry: ...


class FixtureCoStarProvider:
    """Synthetic listings for real LA addresses. provider is 'fixture', never 'costar'."""

    mode = "fixture"
    SUPPORTED_MARKETS = {("LOS ANGELES", "CA"), ("HOUSTON", "TX")}

    def __init__(self, path=None):
        self.path = path or FIXTURES_DIR / "properties" / "la_candidates.json"

    def search(self, request: PropertySearchRequest) -> list[RawListing]:
        if (request.city.upper(), request.state.upper()) not in self.SUPPORTED_MARKETS:
            return []
        doc = json.loads(self.path.read_text())
        out = []
        for p in doc["properties"]:
            if p["city"].upper() != request.city.upper():
                continue  # Houston is supported by the adapter but has no fixture listings yet.
            lst = p["listing"]
            if request.max_monthly_rent is not None and lst["asking_rent_monthly"] > request.max_monthly_rent:
                continue
            if request.min_area_sqm is not None and lst["available_area_sqm"] < request.min_area_sqm:
                continue
            address = {
                k: p[k]
                for k in (
                    "street_address",
                    "unit_or_suite",
                    "city",
                    "state",
                    "postal_code",
                    "parcel_id",
                    "latitude",
                    "longitude",
                )
            }
            out.append(
                RawListing(
                    "fixture",
                    lst["provider_listing_id"],
                    DataOrigin.FIXTURE_SYNTHETIC,
                    address,
                    lst,
                    p.get("interior_assets", []),
                    p.get("exterior_assets", []),
                )
            )
        return out[: request.limit]

    def status(self):
        return status_entry(
            "costar",
            self.mode,
            IntegrationStatus.MOCK,
            False,
            "Synthetic listings; sponsor toolkit not yet inspected (BLOCKED for live).",
        )


class LiveCoStarProvider:
    mode = "live"

    def __init__(self, api_key: str | None, base_url: str | None):
        self.api_key, self.base_url = api_key, base_url

    def search(self, request: PropertySearchRequest) -> list[RawListing]:
        raise ProviderError(
            "costar",
            ProviderErrorKind.NOT_CONFIGURED,
            "Live CoStar adapter not implemented: sponsor toolkit contract not yet verified.",
        )

    def status(self):
        return status_entry(
            "costar",
            self.mode,
            IntegrationStatus.BLOCKED,
            bool(self.api_key),
            "Awaiting sponsor toolkit documentation/credentials.",
        )
