//! Ports of `apps/api/tests/test_geometry.py`, `tests/test_ifc.py::test_mep_extraction_from_fixture`
//! and the framing half of `tests/test_sponsor_providers.py::test_floor_geometry_from_model_and_framing`,
//! plus layout-path tests that need no Python.

mod common;

use std::collections::BTreeSet;

use serde_json::Value;
use ss_core::{Contract, DataOrigin};
use ss_domain::contracts::geometry::{
    Coord, GeometrySource, LayoutPreferences, RoomPolygon, RoomRequirement, SiteGeometry,
};
use ss_domain::design::geometry::framing::{
    frame_for_solver, largest_inner_rectangle, normalize_plan, MIN_FRAME_AREA,
};
use ss_domain::design::geometry::solver::generate_candidates;
use ss_domain::geometry;
use ss_domain::spatial::validation::validate_layout;
use uuid::Uuid;

fn reqs() -> Vec<RoomRequirement> {
    vec![
        RoomRequirement {
            room_type: "treatment".into(),
            count: 3,
            min_area_sqm: 10.0,
            min_width_m: 2.8,
            needs_plumbing: true,
        },
        RoomRequirement {
            room_type: "reception".into(),
            count: 1,
            min_area_sqm: 12.0,
            min_width_m: 3.0,
            needs_plumbing: false,
        },
        RoomRequirement {
            room_type: "storage".into(),
            count: 1,
            min_area_sqm: 4.0,
            min_width_m: 1.5,
            needs_plumbing: false,
        },
    ]
}

fn points(v: &Value) -> Vec<Coord> {
    v.as_array()
        .expect("points")
        .iter()
        .map(|p| {
            let a = p.as_array().expect("pair");
            (a[0].as_f64().expect("x"), a[1].as_f64().expect("y"))
        })
        .collect()
}

/// `site()` of `test_geometry.py`: the Hollywood 302 synthetic fixture.
fn site() -> SiteGeometry {
    let d = common::read_json("fixtures/geometry/hollywood_302_synthetic.json");
    let doc = serde_json::json!({
        "property_id": Uuid::new_v4(),
        "source_id": d["source_id"],
        "geometry_source": "FIXTURE",
        "data_origin": "FIXTURE_SYNTHETIC",
        "floor_boundary": d["floor_boundary"],
        "fixed_obstacles": d["fixed_obstacles"],
        "entrances": d["entrances"],
        "exits": d["exits"],
        "fixture_locations": d["fixture_locations"],
    });
    SiteGeometry::from_json_value(doc).expect("fixture site validates")
}

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<Coord> {
    vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
}

#[test]
fn test_two_valid_non_overlapping_candidates() {
    let cands = generate_candidates(&site(), &reqs(), &LayoutPreferences::default(), 2).unwrap();
    assert_eq!(cands.len(), 2);
    for c in &cands {
        assert!(c.report.valid, "{:?}", c.report.issues);
        assert_eq!(c.rooms.len(), 5);
    }
    let set = |i: usize| -> BTreeSet<String> {
        cands[i]
            .rooms
            .iter()
            .map(|r| format!("{:?}", r.polygon))
            .collect()
    };
    assert_ne!(set(0), set(1));
}

#[test]
fn test_infeasible_program_returns_no_rooms() {
    let big = vec![RoomRequirement {
        room_type: "treatment".into(),
        count: 12,
        min_area_sqm: 12.0,
        min_width_m: 3.0,
        needs_plumbing: false,
    }];
    for c in generate_candidates(&site(), &big, &LayoutPreferences::default(), 2).unwrap() {
        assert!(!c.report.valid && c.rooms.is_empty() && c.report.issues[0].code == "INFEASIBLE");
    }
}

fn room(id: &str, rt: &str, polygon: Vec<Coord>, area: f64) -> RoomPolygon {
    RoomPolygon {
        room_id: id.into(),
        room_type: rt.into(),
        polygon,
        area_sqm: area,
    }
}

#[test]
fn test_validation_rejects_overlap_and_outside() {
    let s = site();
    let rooms = vec![
        room("a", "treatment", rect(0.0, 0.0, 4.0, 4.0), 16.0),
        room("b", "treatment", rect(3.0, 3.0, 7.0, 7.0), 16.0),
        room("c", "storage", rect(11.0, 10.0, 13.0, 12.0), 4.0),
    ];
    let codes: BTreeSet<String> = validate_layout(&s, &rooms, &reqs(), None)
        .issues
        .into_iter()
        .map(|i| i.code)
        .collect();
    for c in ["ROOM_OVERLAP", "OUTSIDE_BOUNDARY", "MISSING_ROOMS"] {
        assert!(codes.contains(c), "{codes:?}");
    }
}

#[test]
fn test_obstacles_reduce_usable_area() {
    let s = site();
    let rooms = vec![room("t", "treatment", rect(0.0, 0.0, 3.2, 3.2), 10.24)];
    let mut one = reqs()[0].clone();
    one.count = 1;
    let issues = validate_layout(&s, &rooms, &[one], None).issues;
    // 10.24 - 0.36 column < 10
    assert!(
        issues.iter().any(|i| i.code == "BELOW_MIN_AREA"),
        "{issues:?}"
    );
}

/// The framing half of `test_floor_geometry_from_model_and_framing`: the input is the document
/// Python's `floor_geometry_from_model(_l_shaped_model())` returns (recorded by the golden
/// generator; the Matterport client is not ported here).
#[test]
fn test_floor_geometry_framing() {
    let g = common::golden("domain/part3_framing.json");
    let doc = &g["l_shaped_model_doc"];
    assert_eq!(doc["data_origin"], "LIVE");
    assert_eq!(doc["geometry_source"], "MATTERPORT_MEASURED");
    let fb = points(&doc["floor_boundary"]);
    assert!((geometry::polygon_area(&fb) - 108.0).abs() <= 0.1);
    assert_eq!(doc["entrances"].as_array().map(Vec::len), Some(1));
    assert_eq!(doc["fixed_walls"].as_array().map(Vec::len), Some(1));
    let doc = normalize_plan(doc).unwrap();
    let site = SiteGeometry::from_json_value(serde_json::json!({
        "property_id": Uuid::new_v4(),
        "source_id": "TESTMODEL01",
        "geometry_source": "MATTERPORT_MEASURED",
        "data_origin": "LIVE",
        "floor_boundary": doc["floor_boundary"],
        "fixed_walls": doc["fixed_walls"],
        "entrances": doc["entrances"],
        "scale_verified": true,
    }))
    .unwrap();
    assert_eq!(site.geometry_source, GeometrySource::MatterportMeasured);
    assert_eq!(site.data_origin, DataOrigin::Live);
    let (x0, y0, x1, y1) = largest_inner_rectangle(&site.floor_boundary, 0.2).unwrap();
    assert!(((x1 - x0) * (y1 - y0) - 84.0).abs() <= 1.5); // 12 x 7 band
    let frame = frame_for_solver(&site, MIN_FRAME_AREA).unwrap();
    assert!(frame.notes.iter().any(|n| n.contains("partition")));
    let reqs = vec![
        RoomRequirement {
            room_type: "treatment".into(),
            count: 2,
            min_area_sqm: 9.0,
            min_width_m: 2.7,
            needs_plumbing: false,
        },
        RoomRequirement {
            room_type: "reception".into(),
            count: 1,
            min_area_sqm: 6.0,
            min_width_m: 2.0,
            needs_plumbing: false,
        },
    ];
    let cands = generate_candidates(&site, &reqs, &LayoutPreferences::default(), 2).unwrap();
    let valid: Vec<_> = cands.iter().filter(|c| c.report.valid).collect();
    assert!(
        !valid.is_empty(),
        "{:?}",
        cands.iter().map(|c| &c.report.issues).collect::<Vec<_>>()
    );
    for c in valid {
        for r in &c.rooms {
            // mapped back onto the real plan
            assert!(geometry::contains_with_tolerance(
                &site.floor_boundary,
                &r.polygon,
                0.01
            ));
        }
        assert!(c.report.disclaimer.contains("largest rectangular area"));
    }
}

fn many_kinds(n: usize, plumbing: bool) -> Vec<RoomRequirement> {
    // Distinct sizes, so no two rooms are interchangeable and the exact search's state space is
    // 2^n.
    (0..n)
        .map(|k| RoomRequirement {
            room_type: format!("kind{k}"),
            count: 1,
            min_area_sqm: 3.0 + k as f64 * 0.41,
            min_width_m: 0.3,
            needs_plumbing: plumbing,
        })
        .collect()
}

fn big_site() -> SiteGeometry {
    SiteGeometry::from_json_value(serde_json::json!({
        "property_id": Uuid::new_v4(),
        "source_id": "synthetic",
        "geometry_source": "FIXTURE",
        "data_origin": "FIXTURE_SYNTHETIC",
        "floor_boundary": [[0, 0], [24, 0], [24, 14], [0, 14]],
        "entrances": [{"kind": "entrance", "start": [11.4, 0], "end": [12.6, 0]}],
        "fixture_locations": [{"kind": "sink", "location": [20, 12]}],
    }))
    .unwrap()
}

#[test]
fn large_programs_use_the_constructive_method_within_the_model() {
    use ss_domain::design::geometry::solver::{
        best_strips, check_solution, expand, SolveStatus, Topology,
    };
    let s = big_site();
    let prefs = LayoutPreferences::default();
    for plumbing in [true, false] {
        let reqs = many_kinds(16, plumbing);
        let rooms = expand(&reqs).unwrap();
        let choice = best_strips(&s, Topology::Spine, &rooms, &prefs).unwrap();
        let rep = choice.solution.expect("a constructive solution");
        assert_eq!(rep.status, SolveStatus::Feasible, "plumbing {plumbing}");
        let obj = check_solution(&choice.strips, &rooms, &s, &prefs, &rep.placements).unwrap();
        assert_eq!(Some(obj), rep.objective);
        let cands = generate_candidates(&s, &reqs, &prefs, 2).unwrap();
        assert!(cands.iter().any(|c| c.rooms.len() == 16));
    }
}

#[test]
fn programs_above_the_room_limit_are_infeasible_not_searched() {
    use ss_domain::design::geometry::solver::MAX_ROOMS;
    let reqs = vec![RoomRequirement {
        room_type: "pod".into(),
        count: MAX_ROOMS as i64 + 1,
        min_area_sqm: 0.01,
        min_width_m: 0.05,
        needs_plumbing: false,
    }];
    let cands = generate_candidates(&site(), &reqs, &LayoutPreferences::default(), 2).unwrap();
    assert_eq!(cands.len(), 2);
    for c in cands {
        assert!(c.rooms.is_empty() && !c.report.valid);
        assert_eq!(c.report.issues[0].code, "INFEASIBLE");
        assert_eq!(
            c.report.issues[0].message,
            format!("No {} layout satisfies the room program", c.name)
        );
    }
}

#[test]
fn layouts_are_never_presented_as_approved_plans() {
    // Truthfulness: every report carries the "not an approved plan" disclaimer.
    for c in generate_candidates(&site(), &reqs(), &LayoutPreferences::default(), 2).unwrap() {
        assert!(c.report.disclaimer.starts_with(
            "Automated geometric check only. Not a code-compliance review or professionally approved plan."
        ));
    }
}

#[test]
fn huge_floors_are_bounded_not_scanned() {
    // 1 km x 1 km at 0.2 m would be 25 million cells; the scan is refused (no frame).
    let ring = vec![
        (0.0, 0.0),
        (1000.0, 0.0),
        (1000.0, 1000.0),
        (500.0, 1000.0),
        (500.0, 1001.0),
        (0.0, 1001.0),
    ];
    assert_eq!(largest_inner_rectangle(&ring, 0.2), None);
}
