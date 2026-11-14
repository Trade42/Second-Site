//! Port of `apps/api/app/domains/procurement/bom.py`: the room-linked bill of materials. Prices
//! are ILLUSTRATIVE fixtures (`FIXTURE_SYNTHETIC`, with a note saying so), never supplier quotes.
//!
//! Money is integer cents. Quantities are Python `Decimal`s ([`PyDecimal`]); a line's total is
//! `int(quantity * unit_price)` (truncation toward zero of the exact product), as in Python.

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use ss_core::{new_id, Contract, CurrencyCode, DataOrigin};

use crate::contracts::geometry::DesignVersion;
use crate::contracts::procurement::{BillOfMaterials, BomItem, BomStatus};
use crate::contracts::values::PyDecimal;
use crate::integrations::catalog::illustrative_unit_price;
use crate::py;
use crate::DomainError;

/// One furniture or fixture line: `(item_type, quantity, unit)`.
pub type RoomItem = (&'static str, i64, &'static str);

/// `ROOM_ITEMS`: per room type, its lines.
pub const ROOM_ITEMS: &[(&str, &[RoomItem])] = &[
    (
        "treatment",
        &[
            ("treatment_bed", 1, "ea"),
            ("storage_cabinet", 1, "ea"),
            ("task_lighting", 1, "ea"),
            ("hand_sink", 1, "ea"),
        ],
    ),
    ("reception", &[("reception_desk", 1, "ea")]),
    ("storage", &[("shelving_unit", 2, "ea")]),
];

/// `NOTE`.
pub const NOTE: &str = "Illustrative planning price (fixture), not a supplier quote.";

fn item(fields: BomItem) -> Result<BomItem, DomainError> {
    // Python builds each `BomItem` with validation (`quantity > 0`).
    fields.validate()?;
    Ok(fields)
}

/// `generate_bom(dv)`: furniture and fixtures per room (by room type), then one flooring line
/// per finish specification (`round(area * 1.1, 1)` m2, 10% waste). `Err` where Python raises:
/// a flooring quantity that rounds to 0 fails `BomItem` validation.
pub fn generate_bom(dv: &DesignVersion) -> Result<BillOfMaterials, DomainError> {
    let mut items: Vec<BomItem> = Vec::new();
    for room in &dv.room_polygons {
        let list = ROOM_ITEMS
            .iter()
            .find(|(t, _)| *t == room.room_type)
            .map_or(&[][..], |(_, l)| *l);
        for (item_type, qty, unit) in list {
            items.push(item(BomItem {
                bom_item_id: new_id(),
                item_type: (*item_type).to_owned(),
                description: format!("{} for {}", item_type.replace('_', " "), room.room_id),
                quantity: PyDecimal::from(*qty),
                unit: (*unit).to_owned(),
                room_id: Some(room.room_id.clone()),
                design_element_ref: Some(format!("room:{}", room.room_id)),
                unit_price_estimate: illustrative_unit_price(item_type),
                price_origin: DataOrigin::FixtureSynthetic,
                price_note: Some(NOTE.to_owned()),
            })?);
        }
    }
    for f in &dv.finish_specifications {
        let q = py::decimal_of_f64(py::round(f.area_sqm * 1.1, 1)).ok_or_else(|| {
            DomainError::BadRecord(format!("finish area {} is not finite", f.area_sqm))
        })?;
        let room = f.room_id.as_deref().unwrap_or("None");
        items.push(item(BomItem {
            bom_item_id: new_id(),
            item_type: "flooring_lvp_sqm".into(),
            description: format!("{} ({})", f.material, f.surface),
            quantity: PyDecimal(q),
            unit: "m2".into(),
            room_id: f.room_id.clone(),
            design_element_ref: Some(format!("finish:{room}:{}", f.surface)),
            unit_price_estimate: illustrative_unit_price("flooring_lvp_sqm"),
            price_origin: DataOrigin::FixtureSynthetic,
            price_note: Some(format!("{NOTE} Includes 10% waste.")),
        })?);
    }
    let mut total: i64 = 0;
    for i in &items {
        let price = Decimal::from(i.unit_price_estimate.unwrap_or(0));
        let line = i
            .quantity
            .0
            .checked_mul(price)
            .and_then(|d| d.trunc().to_i64())
            .ok_or_else(|| DomainError::Overflow(format!("BOM line {}", i.item_type)))?;
        total = total
            .checked_add(line)
            .ok_or_else(|| DomainError::Overflow("BOM total".into()))?;
    }
    let bom = BillOfMaterials {
        bom_id: new_id(),
        project_id: dv.project_id,
        property_id: dv.property_id,
        design_version_id: dv.design_version_id,
        currency: CurrencyCode::usd(),
        items,
        estimated_total: total,
        approved_total: None,
        status: BomStatus::Draft,
    };
    bom.validate()?;
    Ok(bom)
}
