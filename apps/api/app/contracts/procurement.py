from __future__ import annotations

from decimal import Decimal
from enum import StrEnum
from uuid import UUID

from pydantic import Field

from .common import Contract, CurrencyCode, DataOrigin, NonNegativeMinor, UtcDatetime, new_id, utcnow


class BomStatus(StrEnum):
    DRAFT = "DRAFT"
    RECALCULATION_REQUIRED = "RECALCULATION_REQUIRED"
    PENDING_APPROVAL = "PENDING_APPROVAL"
    APPROVED = "APPROVED"


class BomItem(Contract):
    bom_item_id: UUID = Field(default_factory=new_id)
    item_type: str
    description: str
    quantity: Decimal = Field(gt=0)
    unit: str
    room_id: str | None = Field(description="Source room in the design version (or null for project-wide).")
    design_element_ref: str | None = None
    unit_price_estimate: NonNegativeMinor | None = None
    price_origin: DataOrigin = DataOrigin.FIXTURE_SYNTHETIC
    price_note: str | None = None


class BillOfMaterials(Contract):
    bom_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    property_id: UUID
    design_version_id: UUID
    currency: CurrencyCode = "USD"
    items: list[BomItem]
    estimated_total: NonNegativeMinor
    approved_total: NonNegativeMinor | None = None
    status: BomStatus = BomStatus.DRAFT


class AvailabilityStatus(StrEnum):
    IN_STOCK = "IN_STOCK"
    BACKORDERED = "BACKORDERED"
    OUT_OF_STOCK = "OUT_OF_STOCK"
    UNKNOWN = "UNKNOWN"


class SupplierOffer(Contract):
    offer_id: UUID = Field(default_factory=new_id)
    bom_item_id: UUID
    supplier: str
    product_identifier: str
    product_name: str
    currency: CurrencyCode = "USD"
    price: NonNegativeMinor
    dimensions_m: tuple[float, float, float] | None = Field(default=None, description="W x D x H meters")
    availability_status: AvailabilityStatus = AvailabilityStatus.UNKNOWN
    shipping_estimate_days: int | None = Field(default=None, ge=0)
    required_installation: bool = False
    product_url: str | None = None
    retrieved_at: UtcDatetime = Field(default_factory=utcnow)
    data_origin: DataOrigin
    verification_status: str = Field(default="UNVERIFIED", description="UNVERIFIED | PROVIDER_CONFIRMED")
