"""Vexcel aerial imagery and property elements.

Live: Vexcel API 2.0 (https://api.vexcelgroup.com/v2, OpenAPI at /v2/api-docs), authenticated with the
`X-API-KEY` header. Endpoints used, verified against the published OpenAPI document and smoke-tested:
- GET /ortho/extract   (layer, wkt, bbox-dimensions, destination-dimensions, image-format) -> JPEG
- GET /ortho/collections (wkt, include) -> GeoJSON coverage with capture dates
- GET /property/extract (wkt, total-records) -> GeoJSON parcel + structure elements (roof, footprint)
Element values are model detections with scores; they are VISUALLY_OBSERVED evidence, never verified facts.
The API key stays server-side: the web app loads imagery through /api/v1/media/vexcel/ortho.
"""

from __future__ import annotations

from typing import Any, Protocol

import httpx

from app.contracts import IntegrationStatus, ProviderStatusEntry
from app.integrations.base import ProviderError, ProviderErrorKind, RetryPolicy, status_entry, with_retries

PROVIDER = "vexcel"


class VexcelProvider(Protocol):
    mode: str

    def status(self) -> ProviderStatusEntry: ...


def _wkt_point(lat: float, lon: float) -> str:
    if not (-90 <= lat <= 90 and -180 <= lon <= 180):
        raise ProviderError(PROVIDER, ProviderErrorKind.BAD_RESPONSE, "coordinates out of range")
    return f"POINT({lon:.7f} {lat:.7f})"


class FixtureVexcelProvider:
    mode = "fixture"

    def ortho_image(self, lat: float, lon: float, size_m: int = 80, pixels: int = 600) -> bytes | None:
        return None

    def capture_dates(self, lat: float, lon: float) -> list[dict[str, Any]]:
        return []

    def property_elements(self, lat: float, lon: float) -> dict[str, Any] | None:
        return None

    def status(self):
        return status_entry(
            "vexcel",
            self.mode,
            IntegrationStatus.MOCK,
            False,
            "No imagery in fixture mode. Set VEXCEL_PROVIDER_MODE=live with VEXCEL_API_KEY.",
        )


class LiveVexcelProvider:
    mode = "live"

    def __init__(
        self,
        base_url: str,
        api_key: str | None,
        policy: RetryPolicy | None = None,
        client: httpx.Client | None = None,
    ):
        self.base_url = base_url.rstrip("/")
        self.configured = bool(api_key)
        self.policy = policy or RetryPolicy(attempts=2, timeout_s=45.0)
        self.client = client or httpx.Client(
            timeout=self.policy.timeout_s, headers={"X-API-KEY": api_key or ""}
        )
        self._cache: dict[tuple, Any] = {}

    def _get(self, path: str, params: dict[str, Any]) -> httpx.Response:
        if not self.configured:
            raise ProviderError(PROVIDER, ProviderErrorKind.NOT_CONFIGURED, "VEXCEL_API_KEY not set")

        def call() -> httpx.Response:
            r = self.client.get(f"{self.base_url}{path}", params=params)
            r.raise_for_status()
            return r

        return with_retries(PROVIDER, self.policy, call)

    def ortho_image(self, lat: float, lon: float, size_m: int = 80, pixels: int = 600) -> bytes | None:
        key = ("ortho", round(lat, 6), round(lon, 6), size_m, pixels)
        if key not in self._cache:
            r = self._get(
                "/ortho/extract",
                {
                    "layer": "urban",
                    "wkt": _wkt_point(lat, lon),
                    "bbox-dimensions": f"{size_m},{size_m}",
                    "destination-dimensions": f"{pixels},{pixels}",
                    "image-format": "jpeg",
                },
            )
            if not r.headers.get("content-type", "").startswith("image/"):
                raise ProviderError(
                    PROVIDER, ProviderErrorKind.BAD_RESPONSE, "ortho/extract did not return an image"
                )
            self._cache[key] = r.content
        return self._cache[key]

    def capture_dates(self, lat: float, lon: float) -> list[dict[str, Any]]:
        r = self._get(
            "/ortho/collections",
            {"wkt": _wkt_point(lat, lon), "include": "layer,collection,first-capture-date,last-capture-date"},
        )
        return [f.get("properties") or {} for f in r.json().get("features") or []]

    def property_elements(self, lat: float, lon: float) -> dict[str, Any] | None:
        key = ("property", round(lat, 6), round(lon, 6))
        if key not in self._cache:
            r = self._get("/property/extract", {"wkt": _wkt_point(lat, lon), "total-records": 1})
            feats = r.json().get("features") or []
            self._cache[key] = summarize_property(feats[0].get("properties") or {}) if feats else None
        return self._cache[key]

    def status(self):
        return status_entry(
            "vexcel",
            self.mode,
            IntegrationStatus.LIVE if self.configured else IntegrationStatus.BLOCKED,
            self.configured,
            "Vexcel API 2.0: orthomosaic imagery, capture dates, roof/structure elements."
            if self.configured
            else "VEXCEL_API_KEY missing.",
        )


def summarize_property(props: dict[str, Any]) -> dict[str, Any]:
    """Flatten the primary structure's elements we use. Keys are Vexcel's dotted element names."""
    structures = props.get("structures") or []
    primary = next((s for s in structures if s.get("primary-structure")), structures[0] if structures else {})
    return {
        "footprint_area_sqm": primary.get("footprint.area"),
        "roof_condition": primary.get("roof-characteristics.condition"),
        "roof_material": primary.get("roof-characteristics.material.class"),
        "roof_material_score": primary.get("roof-characteristics.material.score"),
        "roof_shape": primary.get("roof-characteristics.shape.value"),
        "rooftop_ac_units": primary.get("roof-objects.ac-unit.count"),
        "solar": primary.get("roof-objects.solar.value"),
        "structural_damage_detected": primary.get("roof-defects.structural-damage.detected"),
        "water_pooling_detected": primary.get("roof-defects.discoloration.types.water-pooling.detected"),
        "building_type": primary.get("building-type.class"),
        "building_type_score": primary.get("building-type.score"),
        "structure_distance_m": primary.get("structure-distance"),
        "inside_parcel": props.get("request-information.inside-parcel"),
        "image_date": props.get("metadata.ortho-date-estimate"),
        "collection": props.get("metadata.collection"),
        "gsd_m": props.get("metadata.gsd"),
    }
