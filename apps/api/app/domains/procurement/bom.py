"""Room-linked BOM generator (owner: frontend-product). Prices are ILLUSTRATIVE fixtures."""

from __future__ import annotations

from decimal import Decimal

from app.contracts import BillOfMaterials, BomItem, DataOrigin, DesignVersion
from app.integrations.catalog import ILLUSTRATIVE_UNIT_PRICES

ROOM_ITEMS = {
    "treatment": [
        ("treatment_bed", 1, "ea"),
        ("storage_cabinet", 1, "ea"),
        ("task_lighting", 1, "ea"),
        ("hand_sink", 1, "ea"),
    ],
    "reception": [("reception_desk", 1, "ea")],
    "storage": [("shelving_unit", 2, "ea")],
}
NOTE = "Illustrative planning price (fixture), not a supplier quote."


def generate_bom(dv: DesignVersion) -> BillOfMaterials:
    items: list[BomItem] = []
    for room in dv.room_polygons:
        for item_type, qty, unit in ROOM_ITEMS.get(room.room_type, []):
            items.append(
                BomItem(
                    item_type=item_type,
                    description=f"{item_type.replace('_', ' ')} for {room.room_id}",
                    quantity=Decimal(qty),
                    unit=unit,
                    room_id=room.room_id,
                    design_element_ref=f"room:{room.room_id}",
                    unit_price_estimate=ILLUSTRATIVE_UNIT_PRICES.get(item_type),
                    price_origin=DataOrigin.FIXTURE_SYNTHETIC,
                    price_note=NOTE,
                )
            )
    for f in dv.finish_specifications:
        items.append(
            BomItem(
                item_type="flooring_lvp_sqm",
                description=f"{f.material} ({f.surface})",
                quantity=Decimal(str(round(f.area_sqm * 1.1, 1))),
                unit="m2",
                room_id=f.room_id,
                design_element_ref=f"finish:{f.room_id}:{f.surface}",
                unit_price_estimate=ILLUSTRATIVE_UNIT_PRICES["flooring_lvp_sqm"],
                price_origin=DataOrigin.FIXTURE_SYNTHETIC,
                price_note=NOTE + " Includes 10% waste.",
            )
        )
    total = sum(int(i.quantity * (i.unit_price_estimate or 0)) for i in items)
    return BillOfMaterials(
        project_id=dv.project_id,
        property_id=dv.property_id,
        design_version_id=dv.design_version_id,
        items=items,
        estimated_total=total,
    )
