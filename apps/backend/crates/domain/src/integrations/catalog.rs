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

/// `ILLUSTRATIVE_UNIT_PRICES.get(item_type)`.
pub fn illustrative_unit_price(item_type: &str) -> Option<i64> {
    ILLUSTRATIVE_UNIT_PRICES
        .iter()
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
