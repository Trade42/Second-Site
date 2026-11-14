"""Vectors for the pure parts of the design service port (`services/design.rs`).

- `room_requirements_from_profile(rooms, plumbing_rooms)` from `domains/design/service.py`,
  dumped with `model_dump(mode="json")`;
- the advisory lock keys the Rust service takes (version numbering per property, first bill of
  materials per design version), computed with the formula a Python writer would use to take the
  same locks: `int.from_bytes(sha256(f"second-site:{ns}:{id}").digest()[:8], "big", signed=True)`.

Pure functions, no database, no network. Deterministic: it regenerates byte-identically.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_design_service.py)
Output: design_service.json
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
sys.path.insert(0, str(REPO / "apps" / "api"))

from app.domains.design.service import room_requirements_from_profile  # noqa: E402

ROOMS = [
    ({"reception": 1, "storage": 1, "treatment": 3}, None),
    ({"treatment": 3, "reception": 1, "storage": 1}, None),
    ({"treatment": 2, "office": 1, "reception": 0, "storage": -1}, None),
    ({"treatment": 2, "laundry": 1, "staff_room": 1}, ["laundry", "staff_room"]),
    ({"treatment": 2, "reception": 1}, []),
    ({}, None),
    ({"unknown_room": 4}, ["unknown_room"]),
]

LOCKS = [
    ("design_versions", "6a8f3a52-1d3b-4f6e-9c4e-0b7c1f2a3d4e"),
    ("design_versions", "00000000-0000-4000-8000-000000000000"),
    ("bills_of_materials", "6a8f3a52-1d3b-4f6e-9c4e-0b7c1f2a3d4e"),
    ("activity_events", "6a8f3a52-1d3b-4f6e-9c4e-0b7c1f2a3d4e"),
]


def lock_key(ns: str, ident: str) -> int:
    return int.from_bytes(hashlib.sha256(f"second-site:{ns}:{ident}".encode()).digest()[:8], "big", signed=True)


def main() -> None:
    out = {
        "room_requirements_from_profile": [
            {
                # Pairs, in dict order (the order is part of the result).
                "rooms": list(rooms.items()),
                "plumbing_rooms": plumbing,
                "result": [
                    r.model_dump(mode="json")
                    for r in room_requirements_from_profile(rooms, set(plumbing) if plumbing is not None else None)
                ],
            }
            for rooms, plumbing in ROOMS
        ],
        "lock_keys": [{"namespace": ns, "id": i, "key": lock_key(ns, i)} for ns, i in LOCKS],
    }
    (HERE / "design_service.json").write_text(json.dumps(out, indent=1) + "\n")
    print("wrote design_service.json:", len(out["room_requirements_from_profile"]), "profiles,",
          len(out["lock_keys"]), "lock keys")


if __name__ == "__main__":
    main()
