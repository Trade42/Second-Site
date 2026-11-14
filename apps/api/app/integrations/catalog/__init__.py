"""Product catalog (procurement sourcing). STATUS: MOCK. No real products/prices/inventory.

Fixture offers are explicitly FIXTURE_SYNTHETIC with illustrative prices.
"""

from __future__ import annotations

from typing import Protocol

from app.contracts import DataOrigin, IntegrationStatus, ProviderStatusEntry, SupplierOffer
from app.integrations.base import status_entry

ILLUSTRATIVE_UNIT_PRICES = {  # cents; illustrative planning values only
    "treatment_bed": 120000,
    "storage_cabinet": 35000,
    "task_lighting": 18000,
    "reception_desk": 150000,
    "shelving_unit": 22000,
    "flooring_lvp_sqm": 4500,
    "wall_paint_sqm": 800,
    "hand_sink": 60000,
}


class ProductCatalogProvider(Protocol):
    mode: str

    def offers_for(self, bom_item_id, item_type: str) -> list[SupplierOffer]: ...

    def status(self) -> ProviderStatusEntry: ...


class FixtureCatalogProvider:
    mode = "fixture"

    def offers_for(self, bom_item_id, item_type):
        price = ILLUSTRATIVE_UNIT_PRICES.get(item_type)
        if price is None:
            return []
        return [
            SupplierOffer(
                bom_item_id=bom_item_id,
                supplier="Illustrative Supplier (fixture)",
                product_identifier=f"fixture-{item_type}",
                product_name=f"Illustrative {item_type}",
                price=price,
                data_origin=DataOrigin.FIXTURE_SYNTHETIC,
            )
        ]

    def status(self):
        return status_entry(
            "product_catalog",
            self.mode,
            IntegrationStatus.MOCK,
            False,
            "Illustrative prices only; no supplier integration.",
        )
