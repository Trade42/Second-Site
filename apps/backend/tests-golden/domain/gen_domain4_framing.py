"""Generate the part 4 framing vectors (review item F3 of rust-domain-3): Python's
largest_inner_rectangle and frame_for_solver on floors with many vertices, which the Rust scan
must reproduce bit for bit while staying fast.

Run from the repository root (about 30 s; the 199 m floor alone takes Python about 12 s):
    (cd apps/api && .venv/bin/python -B ../backend/tests-golden/domain/gen_domain4_framing.py)

Pure functions only: no database, no network. Writes apps/backend/tests-golden/domain/part4_framing.json.

All outlines are synthetic test data (FIXTURE_SYNTHETIC): rectangles whose walls carry small
teeth (the review's "toothed floors": 0.05 m deep, so the outline has 4n + 4 vertices), teeth on
other walls, and a floor with a finely sampled curved wall.
"""

from __future__ import annotations

import json
import math
import sys
from pathlib import Path
from uuid import UUID

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
sys.path.insert(0, str(REPO / "apps" / "api"))

import random  # noqa: E402

from shapely.geometry import Polygon  # noqa: E402

import app.contracts.common as _common  # noqa: E402

# SiteGeometry.geometry_id comes from a seeded generator, so two runs are byte-identical.
_RNG = random.Random(0xF3)
_common.uuid4 = lambda: UUID(int=_RNG.getrandbits(128), version=4)

from app.contracts import DataOrigin, GeometrySource, Opening, SiteGeometry  # noqa: E402
from app.domains.design.geometry.framing import frame_for_solver, largest_inner_rectangle  # noqa: E402

PID = UUID("1afeb98f-0000-4000-8000-0000000000f3")


def toothed_bottom(w: float, h: float, n: int) -> list[tuple[float, float]]:
    """The review's `jagged(W, H, n)`: n teeth 0.05 m deep along the bottom wall."""
    pts = [(0.0, 0.0)]
    step = w / n
    for i in range(n):
        x = i * step
        pts += [(x + step * 0.25, 0.0), (x + step * 0.25, -0.05), (x + step * 0.75, -0.05), (x + step * 0.75, 0.0)]
    pts += [(w, 0.0), (w, h), (0.0, h)]
    return pts


def toothed_left(w: float, h: float, n: int) -> list[tuple[float, float]]:
    """Teeth 0.05 m deep along the left wall (x = 0), going down from the top-left corner."""
    pts = [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]
    step = h / n
    for i in range(n):
        y = h - i * step
        pts += [(0.0, y - step * 0.25), (-0.05, y - step * 0.25), (-0.05, y - step * 0.75), (0.0, y - step * 0.75)]
    return pts


def toothed_top_and_right(w: float, h: float, n: int) -> list[tuple[float, float]]:
    """Teeth on the right wall (0.04 m deep) and on the top wall (0.07 m deep)."""
    pts = [(0.0, 0.0), (w, 0.0)]
    step = h / n
    for i in range(n):
        y = i * step
        pts += [(w, y + step * 0.3), (w + 0.04, y + step * 0.3), (w + 0.04, y + step * 0.6), (w, y + step * 0.6)]
    pts.append((w, h))
    step = w / n
    for i in range(n):
        x = w - i * step
        pts += [(x - step * 0.2, h), (x - step * 0.2, h + 0.07), (x - step * 0.7, h + 0.07), (x - step * 0.7, h)]
    pts.append((0.0, h))
    return pts


def curved_wall(w: float, h: float, r: float, n: int) -> list[tuple[float, float]]:
    """A rectangle whose top-right corner is a quarter circle of radius r sampled at n points."""
    pts = [(0.0, 0.0), (w, 0.0)]
    cx, cy = w - r, h - r
    for k in range(n + 1):
        a = (math.pi / 2) * k / n
        pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    pts.append((0.0, h))
    return pts


def site(boundary, door):
    return SiteGeometry(
        property_id=PID,
        source_id="synthetic-f3",
        geometry_source=GeometrySource.FIXTURE,
        data_origin=DataOrigin.FIXTURE_SYNTHETIC,
        floor_boundary=boundary,
        entrances=[Opening(kind="entrance", start=door[0], end=door[1])],
    )


def frame_doc(frame):
    if frame is None:
        return None
    s = json.loads(frame.site.model_dump_json())
    probes = [(0.0, 0.0), (1.0, 0.5), (2.25, 3.75), (s["floor_boundary"][2][0], s["floor_boundary"][2][1])]
    return {"site": s, "notes": frame.notes, "to_site": [{"p": p, "q": frame.to_site(p)} for p in probes]}


CASES = [
    ("toothed_bottom_60m_404v", toothed_bottom(60.0, 60.0, 100), ((0.0, 5.0), (0.0, 6.0))),
    ("toothed_bottom_60m_2004v", toothed_bottom(60.0, 60.0, 500), ((0.0, 5.0), (0.0, 6.0))),
    ("toothed_bottom_199m_2004v", toothed_bottom(199.0, 199.0, 500), ((0.0, 5.0), (0.0, 6.0))),
    ("toothed_left_40x25m", toothed_left(40.0, 25.0, 300), ((10.0, 0.0), (11.2, 0.0))),
    ("toothed_top_right_30x20m", toothed_top_and_right(30.0, 20.0, 200), ((30.0, 8.0), (30.0, 9.0))),
    ("curved_wall_24x18m", curved_wall(24.0, 18.0, 6.0, 800), ((3.0, 0.0), (4.0, 0.0))),
]


def main() -> None:
    out = []
    for name, boundary, door in CASES:
        s = site(boundary, door)
        lir = largest_inner_rectangle(Polygon(boundary))
        out.append(
            {
                "name": name,
                "vertices": len(boundary),
                "site": json.loads(s.model_dump_json()),
                "largest_inner_rectangle": list(lir) if lir is not None else None,
                "frame": frame_doc(frame_for_solver(s)),
            }
        )
    doc = {
        "_meta": {
            "generator": "apps/backend/tests-golden/domain/gen_domain4_framing.py",
            "note": "Synthetic outlines (FIXTURE_SYNTHETIC) for review item F3; Python results are the specification.",
        },
        "cases": out,
    }
    (HERE / "part4_framing.json").write_text(json.dumps(doc, indent=1, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
