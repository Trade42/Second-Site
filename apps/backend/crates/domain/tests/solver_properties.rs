//! Property tests of the layout solver: a deterministic pseudo-random generator with a fixed
//! seed builds 500 sites and room programs. For every case: no panic; every returned room lies
//! inside the floor and no two rooms overlap; every solution satisfies the integer model; the
//! work stays within the bound that selected the search; the output is identical when repeated.

use ss_domain::contracts::geometry::{
    Coord, Fixture, LayoutPreferences, Opening, RoomRequirement, SiteGeometry,
};
use ss_domain::design::geometry::framing::{frame_for_solver, MIN_FRAME_AREA};
use ss_domain::design::geometry::solver::{
    best_strips, check_solution, expand, generate_candidates, SolveStatus, Topology,
    ASSIGNMENT_STATE_LIMIT, MAX_ROOMS, POSITION_CELL_LIMIT,
};
use ss_domain::geometry;
use ss_domain::spatial::validation::EPS;

/// xorshift64*: deterministic, no dependencies.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn round(&mut self, lo: f64, hi: f64, digits: i32) -> f64 {
        let m = 10f64.powi(digits);
        (self.range(lo, hi) * m).round() / m
    }
}

fn site(rng: &mut Rng) -> SiteGeometry {
    let shape = rng.below(10);
    let (w, h) = (rng.round(3.0, 15.0, 1), rng.round(3.0, 13.0, 1));
    let (x0, y0) = if rng.below(4) == 0 {
        (rng.round(-5.0, 5.0, 2), rng.round(-5.0, 5.0, 2))
    } else {
        (0.0, 0.0)
    };
    let boundary: Vec<Coord> = match shape {
        // L shape
        0 | 1 => {
            let (nx, ny) = (rng.round(1.0, w - 1.0, 1), rng.round(1.0, h - 1.0, 1));
            vec![
                (x0, y0),
                (x0 + w, y0),
                (x0 + w, y0 + ny),
                (x0 + nx, y0 + ny),
                (x0 + nx, y0 + h),
                (x0, y0 + h),
            ]
        }
        // rotated rectangle
        2 => {
            let a = rng.range(-0.6, 0.6);
            let (c, s) = (a.cos(), a.sin());
            [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]
                .iter()
                .map(|(x, y)| {
                    (
                        ((x0 + x * c - y * s) * 1000.0).round() / 1000.0,
                        ((y0 + x * s + y * c) * 1000.0).round() / 1000.0,
                    )
                })
                .collect()
        }
        _ => vec![(x0, y0), (x0 + w, y0), (x0 + w, y0 + h), (x0, y0 + h)],
    };
    let mut entrances = Vec::new();
    match rng.below(6) {
        0 => {}
        1 => {
            // on the left wall
            let a = rng.round(0.0, h - 1.0, 2);
            entrances.push(Opening {
                kind: "entrance".into(),
                start: (x0, y0 + a),
                end: (x0, y0 + a + 1.0),
            });
        }
        _ => {
            let a = rng.round(0.0, (w - 0.8).max(0.1), 2);
            let len = rng.round(0.3, 2.5, 2);
            entrances.push(Opening {
                kind: "entrance".into(),
                start: (x0 + a, y0),
                end: (x0 + (a + len).min(w), y0),
            });
        }
    }
    let mut fixtures = Vec::new();
    if rng.below(3) > 0 {
        fixtures.push(Fixture {
            kind: "sink".into(),
            location: (
                x0 + rng.round(-2.0, w + 2.0, 2),
                y0 + rng.round(-2.0, h + 2.0, 2),
            ),
            evidence_ids: vec![],
        });
    }
    let mut obstacles = Vec::new();
    if rng.below(5) == 0 {
        let (ox, oy) = (
            x0 + rng.round(0.0, w - 0.5, 2),
            y0 + rng.round(0.0, h - 0.5, 2),
        );
        let (ow, oh) = (rng.round(0.2, 1.5, 2), rng.round(0.2, 1.5, 2));
        obstacles.push(vec![
            (ox, oy),
            (ox + ow, oy),
            (ox + ow, oy + oh),
            (ox, oy + oh),
        ]);
    }
    SiteGeometry {
        site_geometry_id: uuid::Uuid::nil(),
        property_id: uuid::Uuid::nil(),
        source_id: "property-test".into(),
        geometry_source: ss_domain::contracts::geometry::GeometrySource::Fixture,
        data_origin: ss_core::DataOrigin::FixtureSynthetic,
        units: "m".into(),
        coordinate_reference: "local_floor_origin".into(),
        floor_boundary: boundary,
        fixed_obstacles: obstacles,
        fixed_walls: vec![],
        entrances,
        exits: vec![],
        windows: vec![],
        fixture_locations: fixtures,
        restricted_areas: vec![],
        scale_verified: false,
        measurement_uncertainty_m: None,
    }
}

fn program(rng: &mut Rng) -> Vec<RoomRequirement> {
    if rng.below(15) == 0 {
        // Many distinct kinds: beyond the exact search's bounds, so the constructive method runs.
        let kinds = 12 + rng.below(5);
        let plumbing = rng.below(2) == 0;
        return (0..kinds)
            .map(|k| RoomRequirement {
                room_type: format!("kind{k}"),
                count: 1,
                min_area_sqm: 2.0 + k as f64 * 0.43,
                min_width_m: rng.round(0.2, 0.6, 1),
                needs_plumbing: plumbing,
            })
            .collect();
    }
    let kinds = rng.below(4) + 1;
    let big = rng.below(12) == 0;
    (0..kinds)
        .map(|k| RoomRequirement {
            room_type: ["treatment", "reception", "storage", "office"][k as usize].into(),
            count: if big {
                rng.below(6) as i64 + 3
            } else {
                rng.below(3) as i64 + 1
            },
            min_area_sqm: rng.round(1.0, 14.0, 1),
            min_width_m: rng.round(0.8, 3.6, 1),
            needs_plumbing: rng.below(2) == 0,
        })
        .collect()
}

#[test]
fn solver_properties_hold_on_500_seeded_cases() {
    let mut rng = Rng(0xDFF0_2230_5EED_0001);
    let (mut solved, mut rooms_placed, mut constructive, mut errors) = (0, 0, 0, 0);
    for case in 0..500 {
        let s = site(&mut rng);
        let reqs = program(&mut rng);
        let prefs = LayoutPreferences {
            style: None,
            prefer_rooms_near_plumbing: rng.below(4) > 0,
            circulation_width_m: [0.9, 1.0, 1.2, 1.2, 1.5][rng.below(5) as usize],
        };
        // The integer model, topology by topology.
        if let (Some(frame), Some(rooms)) = (frame_for_solver(&s, MIN_FRAME_AREA), expand(&reqs)) {
            assert!(rooms.len() <= MAX_ROOMS);
            for topology in Topology::ALL {
                let choice = best_strips(&frame.site, topology, &rooms, &prefs).unwrap();
                for (_, rep) in &choice.attempts {
                    let bound = match rep.status {
                        SolveStatus::Feasible | SolveStatus::Unknown => MAX_ROOMS * 8,
                        _ => POSITION_CELL_LIMIT.max(ASSIGNMENT_STATE_LIMIT),
                    };
                    assert!(
                        rep.work <= bound,
                        "case {case}: work {} > {bound}",
                        rep.work
                    );
                    if rep.status == SolveStatus::Feasible {
                        constructive += 1;
                    }
                }
                if let Some(rep) = &choice.solution {
                    solved += 1;
                    for placements in std::iter::once(&rep.placements).chain(&rep.alternatives) {
                        let obj =
                            check_solution(&choice.strips, &rooms, &frame.site, &prefs, placements)
                                .unwrap_or_else(|e| panic!("case {case} {topology:?}: {e}"));
                        assert_eq!(Some(obj), rep.objective, "case {case}");
                    }
                }
            }
        }
        // The full flow: rooms inside the measured floor, no overlaps, deterministic.
        let cands = match generate_candidates(&s, &reqs, &prefs, 2) {
            Ok(c) => c,
            Err(_) => {
                // Only a generated room failing its contract (sub-millimetre sizes) errs.
                errors += 1;
                continue;
            }
        };
        let again = generate_candidates(&s, &reqs, &prefs, 2).unwrap();
        assert_eq!(
            cands,
            again
                .iter()
                .map(|c| {
                    let mut c = c.clone();
                    c.report.checked_at = cands
                        .iter()
                        .find(|x| x.name == c.name)
                        .map_or(c.report.checked_at, |x| x.report.checked_at);
                    c
                })
                .collect::<Vec<_>>(),
            "case {case}: not deterministic"
        );
        for c in &cands {
            if c.rooms.is_empty() {
                assert!(!c.report.valid || reqs.iter().all(|r| r.count <= 0));
                continue;
            }
            rooms_placed += c.rooms.len();
            for (i, a) in c.rooms.iter().enumerate() {
                assert!(
                    geometry::contains_with_tolerance(&s.floor_boundary, &a.polygon, 1e-3),
                    "case {case} {}: {} outside the floor {:?}: {:?}",
                    c.name,
                    a.room_id,
                    s.floor_boundary,
                    a.polygon
                );
                for b in &c.rooms[i + 1..] {
                    let overlap = geometry::intersection_area(&a.polygon, &b.polygon);
                    assert!(
                        overlap <= EPS,
                        "case {case}: {} overlaps {} by {overlap}",
                        a.room_id,
                        b.room_id
                    );
                }
            }
        }
    }
    println!("solved topologies {solved}, rooms placed {rooms_placed}, constructive solves {constructive}, contract errors {errors}");
    assert!(
        solved > 200,
        "the generator should produce mostly solvable cases ({solved})"
    );
    assert!(
        constructive > 0,
        "the constructive method should be exercised"
    );
}

/// The exact search at its work bound: 10 rooms of 10 different kinds with plumbing terms on a
/// floor whose two spine strips total about 5,800 positions (1,024 class-count states), just
/// under `POSITION_CELL_LIMIT`. Run with `cargo test --release -- --ignored` to time it.
#[test]
#[ignore = "timing only; slow in a debug build"]
fn exact_search_at_its_bound_timing() {
    let site = SiteGeometry {
        floor_boundary: vec![(0.0, 0.0), (14.0, 0.0), (14.0, 29.0), (0.0, 29.0)],
        entrances: vec![Opening {
            kind: "entrance".into(),
            start: (6.4, 0.0),
            end: (7.6, 0.0),
        }],
        fixture_locations: vec![Fixture {
            kind: "sink".into(),
            location: (13.0, 20.0),
            evidence_ids: vec![],
        }],
        ..site(&mut Rng(1))
    };
    let reqs: Vec<RoomRequirement> = (0..10)
        .map(|k| RoomRequirement {
            room_type: format!("kind{k}"),
            count: 1,
            min_area_sqm: 8.0 + k as f64 * 0.7,
            // Small enough that every room's minimum length comes from its area: ten distinct
            // classes, 2^10 states.
            min_width_m: 0.5,
            needs_plumbing: true,
        })
        .collect();
    let prefs = LayoutPreferences::default();
    let rooms = expand(&reqs).unwrap();
    let t = std::time::Instant::now();
    let choice = best_strips(&site, Topology::Spine, &rooms, &prefs).unwrap();
    let rep = choice.solution.expect("solvable");
    println!(
        "status {:?}, work {} cells, {:?}",
        rep.status,
        rep.work,
        t.elapsed()
    );
    assert_eq!(rep.status, SolveStatus::Optimal);
    assert!(rep.work <= POSITION_CELL_LIMIT);
}
