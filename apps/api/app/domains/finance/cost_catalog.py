"""Illustrative renovation unit-cost ranges (integer cents). Planning values, NOT quotes.

Every work item generated from these requires a professional quote. Owner: agents-finance.
"""

from __future__ import annotations

COSTS = {
    # key: (unit, low, expected, high)
    "sink_extend_from_existing": ("sink", 250000, 450000, 800000),
    "sink_new_run_slab": ("sink", 800000, 1500000, 2500000),
    "partition_room_office_buildout": ("room", 200000, 400000, 700000),
    "partition_room_open_shell": ("room", 700000, 1100000, 1600000),
    "electrical_subpanel_allowance": ("allowance", 0, 600000, 1800000),
    "hvac_rezone_existing": ("room", 100000, 250000, 500000),
    "hvac_new_distribution": ("room", 500000, 900000, 1500000),
    "finishes": ("m2", 9000, 15000, 25000),
    "permits_fees": ("lot", 300000, 500000, 900000),
    "design_professional": ("lot", 500000, 900000, 1500000),
}
CONTINGENCY_RATE_BP = 1500  # 15.00% of renovation subtotal, in basis points
FF_E_PER_TREATMENT_ROOM = 450000  # furniture/fixtures/equipment allowance per treatment room
