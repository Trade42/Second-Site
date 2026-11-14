"""Matterport spatial assets.

Live: Matterport Model API (GraphQL at https://api.matterport.com/api/models/graph, HTTP Basic auth with
token id + secret). Every field queried here was verified against the live schema by introspection
(Model.name/dimensions/floors/rooms, ModelFloor.sequence/vertices/edges, Edge.type/thickness/vertices/openings,
Opening.type/width/relativeCenter, Dimension.areaFloor/width/depth, Units.metric).
Geometry comes back in meters in the model's local frame. `floor_geometry_from_model` turns one floor's
wall graph into the `SiteGeometry` shape the layout solver uses.
Fixture: synthetic rectangle geometry for offline work and CI.
"""

from __future__ import annotations

import json
import re
from typing import Any, Protocol

import httpx
from shapely.geometry import LineString, MultiPolygon, Polygon
from shapely.ops import polygonize, unary_union

from app.contracts import IntegrationStatus, ProviderStatusEntry
from app.integrations.base import (
    FIXTURES_DIR,
    ProviderError,
    ProviderErrorKind,
    RetryPolicy,
    status_entry,
    with_retries,
)

PROVIDER = "matterport"


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

    def model_summaries(self, model_ids: list[str]) -> dict[str, dict[str, Any]]:
        return {}

    def status(self):
        return status_entry(
            "matterport",
            self.mode,
            IntegrationStatus.MOCK,
            False,
            "Synthetic rectangle geometry. Set MATTERPORT_PROVIDER_MODE=live with Model API token.",
        )


SUMMARY_FIELDS = "id name dimensions(units: metric) { areaFloor } floors { id }"
GEOMETRY_QUERY = """
query ModelGeometry($id: ID!) {
  model(id: $id) {
    id
    name
    dimensions(units: metric) { areaFloor }
    floors {
      id
      sequence
      label
      dimensions(units: metric) { areaFloor }
      vertices { id position { x y } }
      edges { id type thickness vertices { id } openings { type width relativeCenter } }
    }
    rooms { id label floor { id } classifications { label confidence } dimensions(units: metric) { areaFloor width depth } }
  }
}
"""


class LiveMatterportProvider:
    mode = "live"

    def __init__(
        self,
        graph_url: str,
        token_id: str | None,
        token_secret: str | None,
        policy: RetryPolicy | None = None,
        client: httpx.Client | None = None,
    ):
        self.graph_url = graph_url
        self.configured = bool(token_id and token_secret)
        self.policy = policy or RetryPolicy(attempts=2, timeout_s=40.0)
        self.client = client or httpx.Client(
            timeout=self.policy.timeout_s, auth=(token_id or "", token_secret or "")
        )
        self._geometry_cache: dict[str, dict | None] = {}

    def _gql(self, query: str, variables: dict[str, Any] | None = None) -> dict[str, Any]:
        if not self.configured:
            raise ProviderError(
                PROVIDER, ProviderErrorKind.NOT_CONFIGURED, "MATTERPORT_API_KEY/TOKEN not set"
            )

        def call() -> dict[str, Any]:
            r = self.client.post(self.graph_url, json={"query": query, "variables": variables or {}})
            r.raise_for_status()
            return r.json()

        body = with_retries(PROVIDER, self.policy, call)
        errors = body.get("errors") or []
        if errors and not body.get("data"):
            msg = "; ".join(str(e.get("message")) for e in errors)[:300]
            kind = ProviderErrorKind.AUTH if "auth" in msg.lower() else ProviderErrorKind.BAD_RESPONSE
            raise ProviderError(PROVIDER, kind, msg)
        return body.get("data") or {}

    def model_summaries(self, model_ids: list[str]) -> dict[str, dict[str, Any]]:
        """Name + measured area for many models, batched with GraphQL aliases. Inaccessible models are skipped."""
        out: dict[str, dict[str, Any]] = {}
        for i in range(0, len(model_ids), 20):
            chunk = model_ids[i : i + 20]
            query = (
                "query {"
                + " ".join(
                    f'm{j}: model(id: "{_safe_id(mid)}") {{ {SUMMARY_FIELDS} }}'
                    for j, mid in enumerate(chunk)
                )
                + "}"
            )
            data = self._gql(query)
            for j, mid in enumerate(chunk):
                m = data.get(f"m{j}")
                if m:
                    out[mid] = {
                        "name": (m.get("name") or "").strip() or None,
                        "area_sqm": (m.get("dimensions") or {}).get("areaFloor"),
                        "floor_count": len(m.get("floors") or []),
                    }
        return out

    def get_floor_geometry(self, asset_id: str) -> dict | None:
        if asset_id in self._geometry_cache:
            return self._geometry_cache[asset_id]
        data = self._gql(GEOMETRY_QUERY, {"id": _safe_id(asset_id)})
        model = data.get("model")
        doc = floor_geometry_from_model(model) if model else None
        self._geometry_cache[asset_id] = doc
        return doc

    def status(self):
        return status_entry(
            "matterport",
            self.mode,
            IntegrationStatus.LIVE if self.configured else IntegrationStatus.BLOCKED,
            self.configured,
            "Matterport Model API (GraphQL): model names, measured areas, wall graph and openings."
            if self.configured
            else "MATTERPORT_API_KEY / MATTERPORT_API_TOKEN missing.",
        )


def _safe_id(mid: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9_-]{4,64}", mid):
        raise ProviderError(PROVIDER, ProviderErrorKind.BAD_RESPONSE, f"invalid model id {mid!r}")
    return mid


def suite_matches(model_name: str | None, suite: str | None) -> bool:
    """True when the model title names the listed suite, e.g. 'Suite 787' for suite '787'."""
    if not model_name or not suite:
        return False
    token = re.sub(r"[^A-Za-z0-9]", "", re.sub(r"(?i)^(suite|ste|unit|#)\s*", "", suite.strip())).upper()
    if not token or not any(ch.isdigit() for ch in token):
        return False
    name = re.sub(r"[^A-Z0-9#]+", " ", model_name.upper()).strip()
    if name.replace(" ", "") == token:
        return True
    # Require an explicit unit prefix so "2" never matches "2BR, 3BA - 2,280 SF".
    return re.search(rf"(?:\b(?:SUITE|STE|UNIT)|#)\s*{re.escape(token)}\b", name) is not None


# ---- geometry conversion ---------------------------------------------------------------------


def _segment(a: tuple[float, float], b: tuple[float, float], center: float, width: float):
    """Sub-segment of wall a-b of the given width centred at `center` (fraction along the wall)."""
    ax, ay = a
    bx, by = b
    length = ((bx - ax) ** 2 + (by - ay) ** 2) ** 0.5
    if length <= 0:
        return None
    half = min(width / 2 / length, 0.5)
    t0, t1 = max(center - half, 0.0), min(center + half, 1.0)
    return ((ax + (bx - ax) * t0, ay + (by - ay) * t0), (ax + (bx - ax) * t1, ay + (by - ay) * t1))


def floor_geometry_from_model(model: dict[str, Any]) -> dict | None:
    """Pick the largest floor and convert its wall graph into SiteGeometry fields (meters, local frame)."""
    floors = [f for f in model.get("floors") or [] if f.get("vertices") and f.get("edges")]
    if not floors:
        return None
    floor = max(floors, key=lambda f: ((f.get("dimensions") or {}).get("areaFloor") or 0, len(f["edges"])))
    pos = {v["id"]: (v["position"]["x"], v["position"]["y"]) for v in floor["vertices"]}
    segments = []
    for e in floor["edges"]:
        ids = [v["id"] for v in e.get("vertices") or []]
        if len(ids) == 2 and ids[0] in pos and ids[1] in pos and pos[ids[0]] != pos[ids[1]]:
            segments.append((e, pos[ids[0]], pos[ids[1]]))
    faces = list(polygonize(unary_union([LineString([a, b]) for _, a, b in segments])))
    if not faces:
        return None
    footprint = unary_union(faces)
    if isinstance(footprint, MultiPolygon):
        footprint = max(footprint.geoms, key=lambda g: g.area)
    footprint = Polygon(footprint.exterior).simplify(0.05, preserve_topology=True)
    if footprint.area < 4:
        return None
    boundary = footprint.exterior
    fixed_walls, entrances, windows, interior_doors = [], [], [], []
    for e, a, b in segments:
        on_boundary = LineString([a, b]).buffer(0.05).within(boundary.buffer(0.15))
        for op in e.get("openings") or []:
            seg = _segment(a, b, op.get("relativeCenter") or 0.5, op.get("width") or 0.9)
            if seg is None:
                continue
            kind = op.get("type")
            if kind == "window" and on_boundary:
                windows.append({"kind": "window", "start": seg[0], "end": seg[1]})
            elif kind in {"doorway", "opening"} and on_boundary:
                entrances.append({"kind": "entrance", "start": seg[0], "end": seg[1]})
            elif kind in {"doorway", "opening"}:
                interior_doors.append({"kind": "interior_door", "start": seg[0], "end": seg[1]})
        if e.get("type") == "wall" and not on_boundary:
            fixed_walls.append((a, b))
    coords = [(round(x, 3), round(y, 3)) for x, y in list(boundary.coords)[:-1]]
    rooms = [
        {
            "label": r.get("label") or ((r.get("classifications") or [{}])[0] or {}).get("label"),
            "area_sqm": (r.get("dimensions") or {}).get("areaFloor"),
        }
        for r in model.get("rooms") or []
        if (r.get("floor") or {}).get("id") == floor["id"]
    ]
    return {
        "source_id": model.get("id"),
        "model_name": model.get("name"),
        "floor_label": floor.get("label"),
        "floor_count": len(model.get("floors") or []),
        "measured_area_sqm": (floor.get("dimensions") or {}).get("areaFloor"),
        "geometry_source": "MATTERPORT_MEASURED",
        "data_origin": "LIVE",
        "floor_boundary": coords,
        "fixed_walls": [[[round(x, 3), round(y, 3)] for x, y in w] for w in fixed_walls],
        "entrances": entrances,
        "windows": windows,
        "interior_doors": interior_doors,
        "rooms": rooms,
        "scale_verified": True,
        "measurement_uncertainty_m": None,
    }
