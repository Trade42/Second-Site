//! A bill of materials priced from an approximate sketch on the listed area: the move-in plan's
//! furnishing line says so (basis and one note), approved or not; the amount does not change; a
//! bill whose layout could not be read never reads as measured; and for a bill that is not on a
//! sketch the plan's JSON is byte for byte what it was before `BomTotals.floor` existed.

mod common;

use common::{golden_scenario, ALHAMBRA};
use serde_json::Value;
use ss_core::uuid::Uuid;
use ss_nessie::plan::{
    BomFloor, BASIS_BOM_FLOOR_UNKNOWN, BASIS_BOM_FLOOR_UNKNOWN_APPROVED, BASIS_BOM_SKETCH,
    BASIS_BOM_SKETCH_APPROVED, ITEM_FURNISHING, NOTE_BOM_FLOOR_UNKNOWN, NOTE_BOM_SKETCH,
};
use ss_nessie::{plan_move_in_payments, BomTotals, MoveInPaymentPlan, SandboxTargets, Simulation};

const OVERLAP_NOTE: &str = "The bill of materials may price items (for example flooring or hand sinks) that renovation work items also cover; check for overlap.";

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn plan(
    status: &str,
    approved: Option<i64>,
    floor: BomFloor,
    sim: Simulation,
) -> MoveInPaymentPlan {
    let s = golden_scenario("demo", ALHAMBRA, "36");
    let bom = BomTotals {
        bom_id: Uuid::from_u128(7),
        project_id: s.project_id,
        property_id: s.property_id,
        design_version_id: Uuid::from_u128(8),
        currency: "USD".into(),
        estimated_total_minor: 987_654,
        approved_total_minor: approved,
        status: status.into(),
        floor,
    };
    plan_move_in_payments(&s, Some(&bom), sim, None).unwrap()
}

/// Length and FNV-1a 64 of `serde_json::to_string(&plan)`, printed by this file's first version
/// against `plan.rs` as it was before the field (2026-11-15 08:42Z), and the furnishing line.
#[test]
fn a_bill_that_is_not_on_a_sketch_gives_the_plan_json_recorded_before_the_change() {
    let recorded: [(&str, Option<i64>, Simulation, usize, u64); 4] = [
        (
            "DRAFT",
            None,
            Simulation::FixtureBank,
            5596,
            0x99bc_9a7f_32c5_0cca,
        ),
        (
            "DRAFT",
            None,
            Simulation::NessieSandbox,
            5601,
            0xe7f0_c458_7697_56ff,
        ),
        (
            "APPROVED",
            Some(1_000_000),
            Simulation::FixtureBank,
            5562,
            0x8d35_3890_c4a2_ab85,
        ),
        (
            "APPROVED",
            Some(1_000_000),
            Simulation::NessieSandbox,
            5567,
            0x7b94_b209_65aa_4b64,
        ),
    ];
    for (status, approved, sim, len, fnv) in recorded {
        let p = plan(status, approved, BomFloor::NotAnEstimate, sim);
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(
            (json.len(), fnv1a(json.as_bytes())),
            (len, fnv),
            "{status} {sim:?}"
        );
    }
    let p = plan(
        "DRAFT",
        None,
        BomFloor::NotAnEstimate,
        Simulation::FixtureBank,
    );
    assert_eq!(
        serde_json::to_string(p.payment(ITEM_FURNISHING).unwrap()).unwrap(),
        r#"{"plan_item_id":"buildout:furnishing","sequence":5,"label":"Build-out: furnishing and equipment (simulated)","payee_role":"FURNISHING_SUPPLIER","operation":"PURCHASE","approval_action":"PURCHASE","moves_money":true,"amount_low_minor":987654,"amount_expected_minor":987654,"amount_high_minor":987654,"recurring_day":null,"depends_on":null,"source_line_item_ids":[],"basis":"bill of materials, estimated total (not approved; illustrative prices)","notes":["The bill of materials may price items (for example flooring or hand sinks) that renovation work items also cover; check for overlap."]}"#
    );
}

/// The plan as JSON with the furnishing line's basis and notes taken out (returned beside it).
fn split(p: &MoveInPaymentPlan) -> (Value, String, Vec<String>) {
    let mut v = serde_json::to_value(p).unwrap();
    let line = v["payments"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|l| l["plan_item_id"] == ITEM_FURNISHING)
        .unwrap();
    let basis = line["basis"].take().as_str().unwrap().to_owned();
    let notes = line["notes"]
        .take()
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_owned())
        .collect();
    (v, basis, notes)
}

#[test]
fn a_sketch_changes_only_the_furnishing_lines_basis_and_adds_one_note() {
    for (status, approved, want_basis) in [
        ("DRAFT", None, BASIS_BOM_SKETCH),
        ("PENDING_APPROVAL", None, BASIS_BOM_SKETCH),
        // APPROVED without an approved total is not approved.
        ("APPROVED", None, BASIS_BOM_SKETCH),
        ("APPROVED", Some(1_000_000), BASIS_BOM_SKETCH_APPROVED),
    ] {
        let sim = Simulation::NessieSandbox;
        let (measured, _, measured_notes) =
            split(&plan(status, approved, BomFloor::NotAnEstimate, sim));
        let sketch = plan(status, approved, BomFloor::ListedAreaEstimate, sim);
        let (rest, basis, notes) = split(&sketch);
        // Every amount, total, id and other line is the same.
        assert_eq!(rest, measured, "{status}");
        assert_eq!(basis, want_basis);
        assert_eq!(measured_notes, vec![OVERLAP_NOTE.to_owned()]);
        assert_eq!(
            notes,
            vec![OVERLAP_NOTE.to_owned(), NOTE_BOM_SKETCH.to_owned()]
        );
        assert!(basis.contains("approximate sketch on the listed area"));
        assert!(basis.contains("not measured"));
        let f = sketch.payment(ITEM_FURNISHING).unwrap();
        assert_eq!(f.amount_expected_minor, approved.unwrap_or(987_654));
    }
    assert_eq!(
        BASIS_BOM_SKETCH,
        "bill of materials from an approximate sketch on the listed area (not measured, not approved; illustrative prices)"
    );
    assert!(NOTE_BOM_SKETCH.contains("will change once the space is measured"));
}

#[test]
fn a_bill_whose_layout_could_not_be_read_never_reads_as_measured() {
    for (status, approved, want_basis) in [
        ("DRAFT", None, BASIS_BOM_FLOOR_UNKNOWN),
        (
            "APPROVED",
            Some(1_000_000),
            BASIS_BOM_FLOOR_UNKNOWN_APPROVED,
        ),
    ] {
        let sim = Simulation::FixtureBank;
        let (measured, measured_basis, _) =
            split(&plan(status, approved, BomFloor::NotAnEstimate, sim));
        let (rest, basis, notes) = split(&plan(status, approved, BomFloor::Unknown, sim));
        assert_eq!(rest, measured);
        assert_eq!(basis, want_basis);
        assert_ne!(basis, measured_basis);
        assert!(basis.contains("is unknown"), "{basis}");
        assert_eq!(
            notes,
            vec![OVERLAP_NOTE.to_owned(), NOTE_BOM_FLOOR_UNKNOWN.to_owned()]
        );
    }
}

/// What is sent to the bank for the furnishing purchase is the description, built from the plan
/// item, the attempt and the line's label; nothing built from the basis is sent, so a sketch
/// changes no byte of the approved payload.
#[test]
fn no_text_built_from_the_basis_is_sent_to_the_bank() {
    let targets = SandboxTargets {
        account_id: "acct_1".into(),
        landlord_merchant_id: Some("m_1".into()),
        contractor_merchant_id: Some("m_2".into()),
        supplier_merchant_id: Some("m_3".into()),
        occupancy_bill_id: None,
    };
    let payload = |floor| {
        plan("DRAFT", None, floor, Simulation::NessieSandbox)
            .intent_for(ITEM_FURNISHING, &targets, "0a1b2c3d")
            .unwrap()
            .approval_payload()
    };
    let measured = payload(BomFloor::NotAnEstimate);
    assert_eq!(payload(BomFloor::ListedAreaEstimate), measured);
    assert_eq!(payload(BomFloor::Unknown), measured);
    let text = serde_json::to_string(&measured).unwrap();
    assert!(!text.contains("bill of materials"), "{text}");
    assert!(
        text.contains("Build-out: furnishing and equipment (simulated)"),
        "{text}"
    );
}
