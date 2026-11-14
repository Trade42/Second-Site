"""Interior finish specifications (owner: frontend-product). Deterministic style presets."""

from __future__ import annotations

from app.contracts import FinishSpecification, RoomPolygon

STYLE_PRESETS = {
    "warm_minimalist": {"floor": "light oak-look LVP", "wall": "warm white eggshell paint"},
    None: {"floor": "neutral LVP", "wall": "white eggshell paint"},
}


def finish_specifications(rooms: list[RoomPolygon], style: str | None) -> list[FinishSpecification]:
    preset = STYLE_PRESETS.get(style, STYLE_PRESETS[None])
    return [
        FinishSpecification(room_id=r.room_id, surface="floor", material=preset["floor"], area_sqm=r.area_sqm)
        for r in rooms
    ]
