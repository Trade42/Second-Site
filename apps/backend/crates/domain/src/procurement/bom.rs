//! Port of `apps/api/app/domains/procurement/bom.py`: the room-linked bill of materials. Prices
//! are ILLUSTRATIVE fixtures (`FIXTURE_SYNTHETIC`, with a note saying so), never supplier quotes.
//!
//! Not in Python: [`generate_bom_for`] furnishes by the kind of business ([`furnish_kind`]); kind
//! `treatment` is Python's one list, so [`generate_bom`] and its goldens are unchanged.
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

/// The kind of a business, for furnishing: which furniture its three room types get. The room
/// types themselves (`treatment`, `reception`, `storage`) do not change with the kind.
/// Not in Python, whose `bom.py` keeps the one list ([`ROOM_ITEMS`], kind `treatment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FurnishKind {
    Treatment,
    Dental,
    Medical,
    Cafe,
    Pilates,
    Studio,
    Salon,
    Retail,
}

impl FurnishKind {
    /// Every kind, `treatment` (the list every project had before kinds) first.
    pub const ALL: [FurnishKind; 8] = [
        FurnishKind::Treatment,
        FurnishKind::Dental,
        FurnishKind::Medical,
        FurnishKind::Cafe,
        FurnishKind::Pilates,
        FurnishKind::Studio,
        FurnishKind::Salon,
        FurnishKind::Retail,
    ];

    /// The kind's name, as the workspace page spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            FurnishKind::Treatment => "treatment",
            FurnishKind::Dental => "dental",
            FurnishKind::Medical => "medical",
            FurnishKind::Cafe => "cafe",
            FurnishKind::Pilates => "pilates",
            FurnishKind::Studio => "studio",
            FurnishKind::Salon => "salon",
            FurnishKind::Retail => "retail",
        }
    }

    /// Per room type, the kind's lines; `treatment` is [`ROOM_ITEMS`] itself.
    pub fn room_items(self) -> &'static [(&'static str, &'static [RoomItem])] {
        const RECEPTION_DESK: &[RoomItem] = &[("reception_desk", 1, "ea")];
        const SHELVING: &[RoomItem] = &[("shelving_unit", 2, "ea")];
        match self {
            FurnishKind::Treatment => ROOM_ITEMS,
            FurnishKind::Dental => &[
                (
                    "treatment",
                    &[
                        ("dental_chair", 1, "ea"),
                        ("dentist_stool", 1, "ea"),
                        ("instrument_cabinet", 1, "ea"),
                        ("hand_sink", 1, "ea"),
                    ],
                ),
                ("reception", RECEPTION_DESK),
                ("storage", SHELVING),
            ],
            FurnishKind::Medical => &[
                (
                    "treatment",
                    &[
                        ("exam_table", 1, "ea"),
                        ("physician_stool", 1, "ea"),
                        ("storage_cabinet", 1, "ea"),
                        ("hand_sink", 1, "ea"),
                    ],
                ),
                ("reception", RECEPTION_DESK),
                ("storage", SHELVING),
            ],
            FurnishKind::Cafe => &[
                (
                    "treatment",
                    &[("cafe_table", 2, "ea"), ("cafe_chair", 4, "ea")],
                ),
                (
                    "reception",
                    &[
                        ("service_counter", 1, "ea"),
                        ("espresso_machine", 1, "ea"),
                        ("display_case", 1, "ea"),
                    ],
                ),
                (
                    "storage",
                    &[("shelving_unit", 2, "ea"), ("refrigerator", 1, "ea")],
                ),
            ],
            FurnishKind::Pilates => &[
                (
                    "treatment",
                    &[
                        ("pilates_reformer", 2, "ea"),
                        ("wall_mirror", 1, "ea"),
                        ("prop_shelf", 1, "ea"),
                    ],
                ),
                ("reception", RECEPTION_DESK),
                ("storage", SHELVING),
            ],
            FurnishKind::Studio => &[
                (
                    "treatment",
                    &[
                        ("exercise_mat", 4, "ea"),
                        ("wall_mirror", 1, "ea"),
                        ("prop_shelf", 1, "ea"),
                    ],
                ),
                ("reception", RECEPTION_DESK),
                ("storage", SHELVING),
            ],
            FurnishKind::Salon => &[
                (
                    "treatment",
                    &[
                        ("styling_chair", 2, "ea"),
                        ("mirror_station", 2, "ea"),
                        ("shampoo_station", 1, "ea"),
                    ],
                ),
                ("reception", RECEPTION_DESK),
                ("storage", SHELVING),
            ],
            FurnishKind::Retail => &[
                (
                    "treatment",
                    &[
                        ("display_table", 1, "ea"),
                        ("clothing_rack", 2, "ea"),
                        ("wall_shelving", 1, "ea"),
                    ],
                ),
                ("reception", &[("checkout_counter", 1, "ea")]),
                ("storage", SHELVING),
            ],
        }
    }
}

/// One rule of [`furnish_kind`]: the kind, its substrings, and its whole words.
type KindRule = (
    FurnishKind,
    &'static [&'static str],
    &'static [&'static str],
);

/// The rules, in order: the first that matches gives the kind.
const KIND_RULES: &[KindRule] = &[
    (
        FurnishKind::Dental,
        &["dental", "dentist", "orthodont"],
        &[],
    ),
    (
        FurnishKind::Treatment,
        &[
            "facial", "skin", "esthetic", "massage", "therap", "chiro", "acupunct", "derm", "wax",
            "lash",
        ],
        &["spa"],
    ),
    (
        FurnishKind::Medical,
        &[
            "medical",
            "doctor",
            "physician",
            "pediatric",
            "urgent care",
            "clinic",
        ],
        &[],
    ),
    (
        FurnishKind::Cafe,
        &[
            "caf",
            "coffee",
            "espresso",
            "bakery",
            "restaurant",
            "juice",
            "diner",
        ],
        &["tea", "deli"],
    ),
    (FurnishKind::Pilates, &["pilates"], &[]),
    (
        FurnishKind::Studio,
        &["yoga", "fitness", "dance", "barre", "gym", "martial"],
        &[],
    ),
    (
        FurnishKind::Salon,
        &["salon", "hair", "barber", "nail", "beauty"],
        &[],
    ),
    (
        FurnishKind::Retail,
        &["retail", "boutique", "shop", "store"],
        &[],
    ),
];

/// The furnishing kind of a business type (`BusinessExpansionProfile.business_type`, for example
/// `dental_office`, `caf`, `yoga_studio`): lower-cased, `_` and `-` read as spaces, then the
/// first of [`KIND_RULES`] with a substring (or a whole word, where the rule marks one) in it.
/// Everything else, the empty string and `unspecified` are `treatment`: the list as it was.
pub fn furnish_kind(business_type: &str) -> FurnishKind {
    let text = business_type.to_lowercase().replace(['_', '-'], " ");
    let has_word = |w: &str| text.split(|c: char| !c.is_alphanumeric()).any(|t| t == w);
    KIND_RULES
        .iter()
        .find(|(_, parts, words)| {
            parts.iter().any(|p| text.contains(p)) || words.iter().any(|w| has_word(w))
        })
        .map_or(FurnishKind::Treatment, |(kind, _, _)| *kind)
}

fn item(fields: BomItem) -> Result<BomItem, DomainError> {
    // Python builds each `BomItem` with validation (`quantity > 0`).
    fields.validate()?;
    Ok(fields)
}

/// `generate_bom(dv)`: furniture and fixtures per room (by room type), then one flooring line
/// per finish specification (`round(area * 1.1, 1)` m2, 10% waste). `Err` where Python raises:
/// a flooring quantity that rounds to 0 fails `BomItem` validation.
pub fn generate_bom(dv: &DesignVersion) -> Result<BillOfMaterials, DomainError> {
    bom_with(dv, ROOM_ITEMS)
}

/// [`generate_bom`] with the furniture of the business's kind ([`furnish_kind`]) in each room;
/// flooring lines, prices' origin and note, and the total are made the same way. For a kind
/// `treatment` business type the bill is `generate_bom(dv)`'s, apart from the new ids. Not in
/// Python.
pub fn generate_bom_for(
    dv: &DesignVersion,
    business_type: &str,
) -> Result<BillOfMaterials, DomainError> {
    bom_with(dv, furnish_kind(business_type).room_items())
}

fn bom_with(
    dv: &DesignVersion,
    room_items: &[(&str, &[RoomItem])],
) -> Result<BillOfMaterials, DomainError> {
    let mut items: Vec<BomItem> = Vec::new();
    for room in &dv.room_polygons {
        let list = room_items
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
