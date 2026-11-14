"""Opening timeline (owner: frontend-product). Deterministic milestone offsets — placeholders until
procurement lead times and permit durations are integrated."""

from __future__ import annotations

from datetime import date, timedelta

MILESTONES = [
    ("lease_signed", 0),
    ("permits_submitted", 14),
    ("permits_issued", 56),
    ("construction_complete", 98),
    ("ffe_delivered", 105),
    ("opening", 120),
]


def opening_timeline(start: date) -> list[dict]:
    return [
        {
            "milestone": k,
            "target_date": (start + timedelta(days=d)).isoformat(),
            "basis": "placeholder offset",
        }
        for k, d in MILESTONES
    ]
