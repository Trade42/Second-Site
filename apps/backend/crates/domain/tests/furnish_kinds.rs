//! The bill of materials follows the kind of business
//! (`orchestrator/prompts/reference/furnish-kinds-spec.md`): the kind of a business type, the
//! furniture per kind and room type, and an illustrative price for every item. Kind `treatment`
//! is the list every project had before: `generate_bom_for` then equals `generate_bom` (Python's
//! port, with its goldens) apart from the generated ids.

mod common;

use std::collections::BTreeSet;

use serde_json::Value;
use ss_core::{Contract, DataOrigin};
use ss_domain::contracts::geometry::DesignVersion;
use ss_domain::contracts::procurement::BillOfMaterials;
use ss_domain::integrations::catalog::{
    illustrative_unit_price, FixtureCatalogProvider, ProductCatalogProvider,
    ILLUSTRATIVE_UNIT_PRICES, KIND_ILLUSTRATIVE_UNIT_PRICES,
};
use ss_domain::procurement::bom::{NOTE, ROOM_ITEMS};
use ss_domain::procurement::{furnish_kind, generate_bom, generate_bom_for, FurnishKind};

/// The spec's section 1 examples and the strings the page sends, with their kind.
const KINDS: &[(&str, &str)] = &[
    // The page's strings (the brief's list).
    ("dental_office", "dental"),
    ("medical_clinic", "medical"),
    ("physical_therapy_clinic", "treatment"),
    ("hair_salon", "salon"),
    ("yoga_studio", "studio"),
    ("pilates_studio", "pilates"),
    ("retail_boutique", "retail"),
    ("caf", "cafe"),
    ("cafe", "cafe"),
    ("coffee_shop", "cafe"),
    ("facial_studio", "treatment"),
    ("med_spa", "treatment"),
    ("spa", "treatment"),
    ("salon", "salon"),
    ("retail", "retail"),
    ("restaurant", "cafe"),
    ("unspecified", "treatment"),
    ("", "treatment"),
    ("law_firm", "treatment"),
    // The spec's own examples.
    ("medical spa", "treatment"),
    ("coffee shop", "cafe"),
    // Case, `-` and `_` as spaces, accents.
    ("Dental-Office", "dental"),
    ("Café", "cafe"),
    ("MED-SPA", "treatment"),
    ("Urgent_Care", "medical"),
    // Order: an earlier rule wins.
    ("dental_clinic", "dental"),
    ("orthodontist", "dental"),
    ("dermatology_clinic", "treatment"),
    ("chiropractic clinic", "treatment"),
    ("pediatric dentist", "dental"),
    ("pediatric_office", "medical"),
    ("doctor", "medical"),
    ("coffee_shop_and_bakery", "cafe"),
    ("barbershop", "salon"),
    ("nail_salon", "salon"),
    ("beauty_supply_store", "salon"),
    ("bookstore", "retail"),
    ("gift shop", "retail"),
    ("martial_arts", "studio"),
    ("dance studio", "studio"),
    ("pilates and yoga", "pilates"),
    // Whole words only: `spa`, `tea`, `deli`.
    ("day spa", "treatment"),
    ("spa_and_salon", "treatment"),
    ("coworking_space", "treatment"),
    ("spare_parts", "treatment"),
    ("tea_house", "cafe"),
    ("deli", "cafe"),
    ("steakhouse", "treatment"),
    ("theater", "treatment"),
    ("delivery_hub", "treatment"),
    // Every keyword of the table once.
    ("dentist", "dental"),
    ("skin care", "treatment"),
    ("esthetician", "treatment"),
    ("massage", "treatment"),
    ("therapy", "treatment"),
    ("acupuncture", "treatment"),
    ("waxing", "treatment"),
    ("lash bar", "treatment"),
    ("physician", "medical"),
    ("espresso bar", "cafe"),
    ("juice bar", "cafe"),
    ("diner", "cafe"),
    ("fitness", "studio"),
    ("barre", "studio"),
    ("gym", "studio"),
    ("boutique", "retail"),
];

/// The spec's section 2: per kind, `(room type, [(item_type, quantity)])`.
type KindLines = &'static [(&'static str, &'static [(&'static str, i64)])];

const RECEPTION_DESK: &[(&str, i64)] = &[("reception_desk", 1)];
const SHELVING: &[(&str, i64)] = &[("shelving_unit", 2)];

/// One room type's lines, as the code holds them: `(room type, [(item_type, quantity, unit)])`.
type RoomLines = (&'static str, Vec<(&'static str, i64, &'static str)>);

fn spec_lines(kind: &str) -> KindLines {
    match kind {
        "treatment" => &[
            (
                "treatment",
                &[
                    ("treatment_bed", 1),
                    ("storage_cabinet", 1),
                    ("task_lighting", 1),
                    ("hand_sink", 1),
                ],
            ),
            ("reception", RECEPTION_DESK),
            ("storage", SHELVING),
        ],
        "dental" => &[
            (
                "treatment",
                &[
                    ("dental_chair", 1),
                    ("dentist_stool", 1),
                    ("instrument_cabinet", 1),
                    ("hand_sink", 1),
                ],
            ),
            ("reception", RECEPTION_DESK),
            ("storage", SHELVING),
        ],
        "medical" => &[
            (
                "treatment",
                &[
                    ("exam_table", 1),
                    ("physician_stool", 1),
                    ("storage_cabinet", 1),
                    ("hand_sink", 1),
                ],
            ),
            ("reception", RECEPTION_DESK),
            ("storage", SHELVING),
        ],
        "cafe" => &[
            ("treatment", &[("cafe_table", 2), ("cafe_chair", 4)]),
            (
                "reception",
                &[
                    ("service_counter", 1),
                    ("espresso_machine", 1),
                    ("display_case", 1),
                ],
            ),
            ("storage", &[("shelving_unit", 2), ("refrigerator", 1)]),
        ],
        "pilates" => &[
            (
                "treatment",
                &[
                    ("pilates_reformer", 2),
                    ("wall_mirror", 1),
                    ("prop_shelf", 1),
                ],
            ),
            ("reception", RECEPTION_DESK),
            ("storage", SHELVING),
        ],
        "studio" => &[
            (
                "treatment",
                &[("exercise_mat", 4), ("wall_mirror", 1), ("prop_shelf", 1)],
            ),
            ("reception", RECEPTION_DESK),
            ("storage", SHELVING),
        ],
        "salon" => &[
            (
                "treatment",
                &[
                    ("styling_chair", 2),
                    ("mirror_station", 2),
                    ("shampoo_station", 1),
                ],
            ),
            ("reception", RECEPTION_DESK),
            ("storage", SHELVING),
        ],
        "retail" => &[
            (
                "treatment",
                &[
                    ("display_table", 1),
                    ("clothing_rack", 2),
                    ("wall_shelving", 1),
                ],
            ),
            ("reception", &[("checkout_counter", 1)]),
            ("storage", SHELVING),
        ],
        other => panic!("no such kind: {other}"),
    }
}

/// The spec's price table for the new ids, in its order.
const SPEC_PRICES: &[(&str, i64)] = &[
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

/// The eight prices every bill had before kinds, in Python's order.
const EXISTING_PRICES: &[(&str, i64)] = &[
    ("treatment_bed", 120000),
    ("storage_cabinet", 35000),
    ("task_lighting", 18000),
    ("reception_desk", 150000),
    ("shelving_unit", 22000),
    ("flooring_lvp_sqm", 4500),
    ("wall_paint_sqm", 800),
    ("hand_sink", 60000),
];

/// One business type per kind, for the tests that walk the kinds.
const ONE_PER_KIND: &[(&str, &str)] = &[
    ("facial_studio", "treatment"),
    ("dental_office", "dental"),
    ("medical_clinic", "medical"),
    ("caf", "cafe"),
    ("pilates_studio", "pilates"),
    ("yoga_studio", "studio"),
    ("hair_salon", "salon"),
    ("retail_boutique", "retail"),
];

/// The designs of Python's bill-of-materials golden: 70 real layouts to furnish.
fn designs() -> Vec<(String, DesignVersion)> {
    let doc = common::golden("domain/part3_bom.json");
    doc["designs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let name = c["name"].as_str().unwrap().to_owned();
            let dv = DesignVersion::from_json_value(c["design"].clone()).expect(&name);
            (name, dv)
        })
        .collect()
}

/// The bill as JSON without the ids generated per call.
fn without_ids(bom: &BillOfMaterials) -> Value {
    let mut v = common::to_value(bom);
    v.as_object_mut().unwrap().remove("bom_id");
    for it in v["items"].as_array_mut().unwrap() {
        it.as_object_mut().unwrap().remove("bom_item_id");
    }
    v
}

#[test]
fn the_kind_of_a_business_type() {
    for (business_type, kind) in KINDS {
        assert_eq!(
            furnish_kind(business_type).as_str(),
            *kind,
            "{business_type:?}"
        );
    }
    // The names are the eight of the spec, `treatment` first.
    let names: Vec<&str> = FurnishKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "treatment",
            "dental",
            "medical",
            "cafe",
            "pilates",
            "studio",
            "salon",
            "retail"
        ]
    );
}

#[test]
fn each_kind_has_the_lines_of_the_spec() {
    for kind in FurnishKind::ALL {
        let ours: Vec<RoomLines> = kind
            .room_items()
            .iter()
            .map(|(room, items)| (*room, items.to_vec()))
            .collect();
        let spec: Vec<RoomLines> = spec_lines(kind.as_str())
            .iter()
            .map(|(room, items)| (*room, items.iter().map(|(t, q)| (*t, *q, "ea")).collect()))
            .collect();
        assert_eq!(ours, spec, "{}", kind.as_str());
    }
    // `treatment` is Python's one list itself.
    assert_eq!(FurnishKind::Treatment.room_items(), ROOM_ITEMS);
}

#[test]
fn every_item_of_the_spec_has_its_illustrative_price() {
    // The existing eight, unchanged and in their order; the new ones after them, in the spec's.
    assert_eq!(ILLUSTRATIVE_UNIT_PRICES, EXISTING_PRICES);
    assert_eq!(KIND_ILLUSTRATIVE_UNIT_PRICES, SPEC_PRICES);
    for (item_type, price) in EXISTING_PRICES.iter().chain(SPEC_PRICES) {
        assert_eq!(
            illustrative_unit_price(item_type),
            Some(*price),
            "{item_type}"
        );
    }
    // No id twice, so the lookup order cannot hide a price.
    let ids: BTreeSet<&str> = EXISTING_PRICES
        .iter()
        .chain(SPEC_PRICES)
        .map(|(k, _)| *k)
        .collect();
    assert_eq!(ids.len(), EXISTING_PRICES.len() + SPEC_PRICES.len());
    // Every id a kind can put on a bill is priced, and every new price is used by a kind.
    let mut used: BTreeSet<&str> = BTreeSet::new();
    for kind in FurnishKind::ALL {
        for (_, items) in kind.room_items() {
            for (item_type, quantity, _) in *items {
                assert!(
                    illustrative_unit_price(item_type).is_some(),
                    "{item_type} of kind {} has no illustrative price",
                    kind.as_str()
                );
                assert!(*quantity > 0, "{item_type}");
                used.insert(item_type);
            }
        }
    }
    for (item_type, _) in SPEC_PRICES {
        assert!(used.contains(item_type), "{item_type} is priced but unused");
    }
    assert_eq!(illustrative_unit_price("no_such_item"), None);
}

#[test]
fn the_fixture_catalog_offers_a_new_item_as_it_does_an_old_one() {
    let provider = FixtureCatalogProvider;
    let bom_item = ss_core::new_id();
    let shape = |item_type: &str| {
        let offers = provider.offers_for(bom_item, item_type);
        assert_eq!(offers.len(), 1, "{item_type}");
        let o = &offers[0];
        o.validate().unwrap();
        let v = common::to_value(o);
        common::assert_conforms("SupplierOffer", &v);
        assert_eq!(o.bom_item_id, bom_item);
        assert_eq!(o.price, illustrative_unit_price(item_type).unwrap());
        assert_eq!(o.data_origin, DataOrigin::FixtureSynthetic);
        assert_eq!(o.supplier, "Illustrative Supplier (fixture)");
        assert_eq!(o.product_identifier, format!("fixture-{item_type}"));
        assert_eq!(o.product_name, format!("Illustrative {item_type}"));
        assert_eq!(v["availability_status"], "UNKNOWN");
        assert_eq!(v["verification_status"], "UNVERIFIED");
        // No invented lead time, link or size.
        assert_eq!(v["shipping_estimate_days"], Value::Null);
        assert_eq!(v["product_url"], Value::Null);
        assert_eq!(v["dimensions_m"], Value::Null);
        // The same document as an old id's, apart from what names the item and its price.
        let mut v = v;
        for k in [
            "offer_id",
            "retrieved_at",
            "price",
            "product_identifier",
            "product_name",
        ] {
            v.as_object_mut().unwrap().remove(k);
        }
        v
    };
    let old = shape("treatment_bed");
    for (item_type, _) in SPEC_PRICES {
        assert_eq!(shape(item_type), old, "{item_type}");
    }
    assert!(provider.offers_for(bom_item, "no_such_item").is_empty());
}

#[test]
fn a_treatment_kind_bill_is_the_bill_as_it_was() {
    let designs = designs();
    assert!(designs.len() >= 40);
    let treatment: Vec<&str> = KINDS
        .iter()
        .filter(|(_, k)| *k == "treatment")
        .map(|(t, _)| *t)
        .collect();
    assert!(treatment.contains(&"facial_studio") && treatment.contains(&""));
    for (name, dv) in &designs {
        let before = without_ids(&generate_bom(dv).expect(name));
        for business_type in &treatment {
            let bom = generate_bom_for(dv, business_type).expect(name);
            assert_eq!(without_ids(&bom), before, "{name} as {business_type:?}");
        }
    }
}

#[test]
fn a_kind_furnishes_each_room_by_the_spec_and_keeps_everything_else() {
    let mut furnished_rooms = 0;
    for (name, dv) in &designs() {
        let before = generate_bom(dv).expect(name);
        let flooring = |b: &BillOfMaterials| -> Vec<Value> {
            b.items
                .iter()
                .filter(|i| i.item_type == "flooring_lvp_sqm")
                .map(|i| {
                    let mut v = common::to_value(i);
                    v.as_object_mut().unwrap().remove("bom_item_id");
                    v
                })
                .collect()
        };
        for (business_type, kind) in ONE_PER_KIND {
            let ctx = format!("{name} as {business_type}");
            let bom = generate_bom_for(dv, business_type).expect(&ctx);
            bom.validate().expect(&ctx);
            common::assert_conforms("BillOfMaterials", &common::to_value(&bom));
            // The furniture: per room in the layout's order, the kind's lines for its type.
            let mut expected: Vec<(String, String, i64)> = Vec::new();
            for room in &dv.room_polygons {
                let lines = spec_lines(kind)
                    .iter()
                    .find(|(t, _)| *t == room.room_type)
                    .map_or(&[][..], |(_, l)| *l);
                for (item_type, quantity) in lines {
                    expected.push((room.room_id.clone(), (*item_type).to_owned(), *quantity));
                    furnished_rooms += 1;
                }
            }
            let furniture: Vec<_> = bom
                .items
                .iter()
                .filter(|i| i.item_type != "flooring_lvp_sqm")
                .collect();
            assert_eq!(furniture.len(), expected.len(), "{ctx}");
            for (item, (room_id, item_type, quantity)) in furniture.iter().zip(&expected) {
                let v = common::to_value(*item);
                assert_eq!(item.item_type, *item_type, "{ctx}");
                assert_eq!(item.room_id.as_deref(), Some(room_id.as_str()), "{ctx}");
                assert_eq!(v["quantity"], quantity.to_string(), "{ctx} {item_type}");
                assert_eq!(item.unit, "ea");
                assert_eq!(
                    item.description,
                    format!("{} for {room_id}", item_type.replace('_', " ")),
                    "{ctx}"
                );
                assert_eq!(
                    item.design_element_ref.as_deref(),
                    Some(format!("room:{room_id}").as_str())
                );
                assert_eq!(
                    item.unit_price_estimate,
                    illustrative_unit_price(item_type),
                    "{ctx} {item_type}"
                );
                assert!(item.unit_price_estimate.is_some(), "{ctx} {item_type}");
                assert_eq!(item.price_origin, DataOrigin::FixtureSynthetic);
                assert_eq!(item.price_note.as_deref(), Some(NOTE));
            }
            // Flooring is as it was, and comes after the furniture.
            assert_eq!(flooring(&bom), flooring(&before), "{ctx}");
            let first_floor = bom
                .items
                .iter()
                .position(|i| i.item_type == "flooring_lvp_sqm")
                .unwrap_or(bom.items.len());
            assert_eq!(first_floor, furniture.len(), "{ctx}");
            // The total is the sum of the lines: furniture exactly, flooring as before.
            let furniture_total: i64 = expected
                .iter()
                .map(|(_, t, q)| q * illustrative_unit_price(t).unwrap())
                .sum();
            let before_furniture: i64 = before
                .items
                .iter()
                .filter(|i| i.item_type != "flooring_lvp_sqm")
                .map(|i| {
                    let q: i64 = common::to_value(i)["quantity"]
                        .as_str()
                        .unwrap()
                        .parse()
                        .unwrap();
                    q * i.unit_price_estimate.unwrap()
                })
                .sum();
            assert_eq!(
                bom.estimated_total,
                before.estimated_total - before_furniture + furniture_total,
                "{ctx}"
            );
            // Everything of the bill that is not a line is as it was.
            let head = |b: &BillOfMaterials| {
                let mut v = common::to_value(b);
                for k in ["bom_id", "items", "estimated_total"] {
                    v.as_object_mut().unwrap().remove(k);
                }
                v
            };
            assert_eq!(head(&bom), head(&before), "{ctx}");
        }
    }
    assert!(furnished_rooms > 1000, "{furnished_rooms} lines checked");
}
