//! Port of `apps/api/app/integrations/catalog/__init__.py`: the product catalog for procurement
//! sourcing. STATUS: MOCK. No real products, prices or inventory: fixture offers are explicitly
//! `FIXTURE_SYNTHETIC` with illustrative prices.

use ss_core::{utcnow, CurrencyCode, DataOrigin, IntegrationStatus};
use uuid::Uuid;

use crate::contracts::procurement::{AvailabilityStatus, SupplierOffer};
use crate::contracts::ProviderStatusEntry;

use super::base::status_entry;

/// `ILLUSTRATIVE_UNIT_PRICES` (cents; illustrative planning values only), in Python's order.
pub const ILLUSTRATIVE_UNIT_PRICES: &[(&str, i64)] = &[
    ("treatment_bed", 120000),
    ("storage_cabinet", 35000),
    ("task_lighting", 18000),
    ("reception_desk", 150000),
    ("shelving_unit", 22000),
    ("flooring_lvp_sqm", 4500),
    ("wall_paint_sqm", 800),
    ("hand_sink", 60000),
];

/// Illustrative unit prices (cents; planning values only, never a supplier quote) of the items
/// that furnish the kinds of business other than `treatment`
/// (`procurement::bom::FurnishKind`). Not in Python, whose catalog has only the eight above;
/// they are a second table, looked up after it, so that [`ILLUSTRATIVE_UNIT_PRICES`] stays
/// Python's.
pub const KIND_ILLUSTRATIVE_UNIT_PRICES: &[(&str, i64)] = &[
    ("dental_chair", 650000),
    ("dentist_stool", 35000),
    ("instrument_cabinet", 180000),
    ("exam_table", 140000),
    ("physician_stool", 25000),
    ("cafe_table", 22000),
    ("cafe_chair", 9000),
    ("service_counter", 420000),
    ("espresso_machine", 850000),
    ("display_case", 260000),
    ("refrigerator", 240000),
    ("pilates_reformer", 380000),
    ("exercise_mat", 6000),
    ("wall_mirror", 45000),
    ("prop_shelf", 30000),
    ("styling_chair", 55000),
    ("mirror_station", 70000),
    ("shampoo_station", 110000),
    ("display_table", 60000),
    ("clothing_rack", 25000),
    ("wall_shelving", 40000),
    ("checkout_counter", 190000),
];

/// `ILLUSTRATIVE_UNIT_PRICES.get(item_type)`, then [`KIND_ILLUSTRATIVE_UNIT_PRICES`].
pub fn illustrative_unit_price(item_type: &str) -> Option<i64> {
    ILLUSTRATIVE_UNIT_PRICES
        .iter()
        .chain(KIND_ILLUSTRATIVE_UNIT_PRICES)
        .find(|(k, _)| *k == item_type)
        .map(|(_, v)| *v)
}

/// `ProductCatalogProvider`.
pub trait ProductCatalogProvider {
    fn mode(&self) -> &str;
    fn offers_for(&self, bom_item_id: Uuid, item_type: &str) -> Vec<SupplierOffer>;
    fn status(&self) -> ProviderStatusEntry;
}

/// `FixtureCatalogProvider`: one illustrative offer per priced item type.
#[derive(Debug, Clone, Copy, Default)]
pub struct FixtureCatalogProvider;

impl ProductCatalogProvider for FixtureCatalogProvider {
    fn mode(&self) -> &str {
        "fixture"
    }

    fn offers_for(&self, bom_item_id: Uuid, item_type: &str) -> Vec<SupplierOffer> {
        let Some(price) = illustrative_unit_price(item_type) else {
            return vec![];
        };
        vec![SupplierOffer {
            offer_id: ss_core::new_id(),
            bom_item_id,
            supplier: "Illustrative Supplier (fixture)".into(),
            product_identifier: format!("fixture-{item_type}"),
            product_name: format!("Illustrative {item_type}"),
            currency: CurrencyCode::usd(),
            price,
            dimensions_m: None,
            availability_status: AvailabilityStatus::Unknown,
            shipping_estimate_days: None,
            required_installation: false,
            product_url: None,
            retrieved_at: utcnow(),
            data_origin: DataOrigin::FixtureSynthetic,
            verification_status: "UNVERIFIED".into(),
        }]
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            "product_catalog",
            self.mode(),
            IntegrationStatus::Mock,
            false,
            "Illustrative prices only; no supplier integration.",
        )
    }
}
