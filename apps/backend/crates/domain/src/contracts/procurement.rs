//! Port of `apps/api/app/contracts/procurement.py`. Money is integer minor units.

use serde::{Deserialize, Serialize};
use ss_core::{
    new_id, string_enum, utcnow, Checker, Contract, CurrencyCode, DataOrigin, NonNegativeMinor,
    UtcDateTime, Validate,
};
use uuid::Uuid;

use super::values::PyDecimal;

string_enum! {
    #[derive(Default)]
    pub enum BomStatus {
        #[default]
        Draft = "DRAFT",
        RecalculationRequired = "RECALCULATION_REQUIRED",
        PendingApproval = "PENDING_APPROVAL",
        Approved = "APPROVED",
    }
}

fn fixture_synthetic() -> DataOrigin {
    DataOrigin::FixtureSynthetic
}

/// Required on input but nullable (Pydantic `str | None` with no default).
fn required_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BomItem {
    #[serde(default = "new_id")]
    pub bom_item_id: Uuid,
    pub item_type: String,
    pub description: String,
    /// `> 0`.
    pub quantity: PyDecimal,
    pub unit: String,
    /// Source room in the design version (or null for project-wide). Required, may be null.
    #[serde(deserialize_with = "required_nullable")]
    pub room_id: Option<String>,
    #[serde(default)]
    pub design_element_ref: Option<String>,
    /// `>= 0` when present.
    #[serde(default)]
    pub unit_price_estimate: Option<NonNegativeMinor>,
    #[serde(default = "fixture_synthetic")]
    pub price_origin: DataOrigin,
    #[serde(default)]
    pub price_note: Option<String>,
}

impl Validate for BomItem {
    fn check(&self, c: &mut Checker) {
        c.gt("quantity", self.quantity, PyDecimal::ZERO);
        c.ge_opt("unit_price_estimate", self.unit_price_estimate, 0);
    }
}

impl Contract for BomItem {
    const NAME: &'static str = "BomItem";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillOfMaterials {
    #[serde(default = "new_id")]
    pub bom_id: Uuid,
    pub project_id: Uuid,
    pub property_id: Uuid,
    pub design_version_id: Uuid,
    #[serde(default)]
    pub currency: CurrencyCode,
    pub items: Vec<BomItem>,
    /// `>= 0`.
    pub estimated_total: NonNegativeMinor,
    /// `>= 0` when present.
    #[serde(default)]
    pub approved_total: Option<NonNegativeMinor>,
    #[serde(default)]
    pub status: BomStatus,
}

impl Validate for BillOfMaterials {
    fn check(&self, c: &mut Checker) {
        c.each("items", &self.items);
        c.ge("estimated_total", self.estimated_total, 0);
        c.ge_opt("approved_total", self.approved_total, 0);
    }
}

impl Contract for BillOfMaterials {
    const NAME: &'static str = "BillOfMaterials";
}

string_enum! {
    #[derive(Default)]
    pub enum AvailabilityStatus {
        InStock = "IN_STOCK",
        Backordered = "BACKORDERED",
        OutOfStock = "OUT_OF_STOCK",
        #[default]
        Unknown = "UNKNOWN",
    }
}

fn unverified() -> String {
    "UNVERIFIED".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplierOffer {
    #[serde(default = "new_id")]
    pub offer_id: Uuid,
    pub bom_item_id: Uuid,
    pub supplier: String,
    pub product_identifier: String,
    pub product_name: String,
    #[serde(default)]
    pub currency: CurrencyCode,
    /// `>= 0`.
    pub price: NonNegativeMinor,
    /// W x D x H meters.
    #[serde(default)]
    pub dimensions_m: Option<(f64, f64, f64)>,
    #[serde(default)]
    pub availability_status: AvailabilityStatus,
    /// `>= 0` when present.
    #[serde(default)]
    pub shipping_estimate_days: Option<i64>,
    #[serde(default)]
    pub required_installation: bool,
    #[serde(default)]
    pub product_url: Option<String>,
    #[serde(default = "utcnow")]
    pub retrieved_at: UtcDateTime,
    pub data_origin: DataOrigin,
    /// `UNVERIFIED | PROVIDER_CONFIRMED`.
    #[serde(default = "unverified")]
    pub verification_status: String,
}

impl Validate for SupplierOffer {
    fn check(&self, c: &mut Checker) {
        c.ge("price", self.price, 0);
        c.ge_opt("shipping_estimate_days", self.shipping_estimate_days, 0);
    }
}

impl Contract for SupplierOffer {
    const NAME: &'static str = "SupplierOffer";
}
