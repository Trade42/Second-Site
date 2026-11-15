//! Review item F3 of rust-domain-3, timing half: the two toothed floors the review named (60 m
//! with 404 vertices, 199 m with 2,004 vertices; the pre-F3 scan took 5.5 s and 317 s for them in a
//! release build) are framed well under a second, even in a debug build. Their results are
//! checked against Python in `tests/framing_work.rs`.
//!
//! Finding R2 of the part 4 review (rust-domain-6): every outline the work bound accepts frames
//! within the budget (about 0.25 s in a release build, about 1 s in the workspace's debug
//! profile), and every outline it refuses is refused within it. For each adversarial family the
//! test finds the largest member the bound accepts (the family's worst accepted case), times it,
//! and times the refusal of a member beyond the bound.
//!
//! This file is its own test binary, so no other test of this crate runs beside it. The shared
//! machine can be loaded by other builds, so each outline is timed three times, the fastest run
//! counts, and the budget is checked with a factor of [`LOAD_FACTOR`].

mod common;

use std::f64::consts::TAU;
use std::time::{Duration, Instant};

use ss_core::Contract;
use ss_domain::contracts::geometry::{Coord, SiteGeometry};
use ss_domain::design::geometry::framing::{
    frame_for_solver, frame_work_estimate, largest_inner_rectangle_checked, FRAME_WORK_PER_CELL,
    FRAME_WORK_PER_SEGMENT, FRAME_WORK_PER_SEGMENT_ROW, FRAME_WORK_PER_SEGMENT_ROW_STEP,
    MAX_FRAME_WORK, MIN_FRAME_AREA,
};

/// The budget is about 0.25 s in a release build and about 1 s in the debug profile at low load;
/// timings here are checked against `LOAD_FACTOR` times that, so a loaded machine does not fail
/// the test (the measured worst cases, at low load, are in the rust-domain-6 report).
const LOAD_FACTOR: u32 = 3;

fn budget() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_millis(1000)
    } else {
        Duration::from_millis(250)
    }
}

fn fastest_of_3<T>(mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut best = Duration::MAX;
    let mut out = None;
    for _ in 0..3 {
        let t = Instant::now();
        let r = f();
        best = best.min(t.elapsed());
        out = Some(r);
    }
    (best, out.expect("ran"))
}

#[test]
fn toothed_floors_frame_well_under_a_second() {
    let doc = common::golden("domain/part4_framing.json");
    for name in ["toothed_bottom_60m_404v", "toothed_bottom_199m_2004v"] {
        let case = doc["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .expect(name);
        let site = SiteGeometry::from_json_value(case["site"].clone()).unwrap();
        let best = (0..3)
            .map(|_| {
                let t = Instant::now();
                assert!(frame_for_solver(&site, MIN_FRAME_AREA).is_some());
                t.elapsed()
            })
            .min()
            .unwrap();
        eprintln!("{name}: frame_for_solver in {best:?} (fastest of 3)");
        assert!(best < Duration::from_millis(1000), "{name} took {best:?}");
    }
}

// ---- adversarial families (valid polygons), each growing with n ------------------------------

/// The review's sawtooth: n deep triangular teeth hanging from the top of a 199.8 m floor (2n
/// nearly full-height segments, each near a cell of every row).
fn sawtooth(n: usize) -> Vec<Coord> {
    let (w, h) = (199.8, 199.8);
    let mut pts = vec![(0.0, 0.0), (w, 0.0), (w, h)];
    let step = w / n as f64;
    for i in (1..=n).rev() {
        let x = i as f64 * step;
        pts.push((x - step / 2.0, h * 0.02));
        pts.push((x - step, h));
    }
    pts
}

/// The review's comb: n rectangular teeth 0.6 of the height deep hanging from the top.
fn comb(n: usize) -> Vec<Coord> {
    let (w, h) = (199.8, 199.8);
    let mut pts = vec![(0.0, 0.0), (w, 0.0), (w, h)];
    let step = w / n as f64;
    for i in (1..=n).rev() {
        let x = i as f64 * step;
        pts.extend([
            (x - step * 0.25, h),
            (x - step * 0.25, h * 0.4),
            (x - step * 0.75, h * 0.4),
            (x - step * 0.75, h),
        ]);
    }
    pts.push((0.0, h));
    pts
}

/// The review's star: n vertices alternating between radius 99.9 m and 15 % of it.
fn star(n: usize) -> Vec<Coord> {
    let r = 99.9;
    (0..n)
        .map(|k| {
            let rr = if k % 2 == 0 { r } else { r * 0.15 };
            let a = TAU * k as f64 / n as f64;
            (rr * a.cos() + r, rr * a.sin() + r)
        })
        .collect()
}

/// n thin spikes in the right margin of a 199.95 m floor (beyond the last whole grid column):
/// per-row preparation only.
fn margin_spikes(n: usize) -> Vec<Coord> {
    let (w, h) = (199.95, 199.8);
    let mut pts = vec![(0.0, 0.0), (w, 0.0), (w, h)];
    let (x0, x1) = (199.81, 199.94);
    let step = (x1 - x0) / n as f64;
    for i in (0..n).rev() {
        let x = x0 + i as f64 * step;
        pts.push((x + step * 0.5, 0.1));
        pts.push((x, h));
    }
    pts.push((0.0, h));
    pts
}

/// n slots 2 rows tall, 150 m deep, on grid lines: cells next to them are inside and near.
fn grid_slots(n: usize) -> Vec<Coord> {
    let (w, h) = (199.8, 199.8);
    let mut pts = vec![(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)];
    for i in (0..n).rev() {
        let (y0, y1) = ((4 * i + 1) as f64 * 0.2, (4 * i + 3) as f64 * 0.2);
        if y1 < h {
            pts.extend([(0.0, y1), (150.0, y1), (150.0, y0), (0.0, y0)]);
        }
    }
    pts
}

/// n hairline slits (1e-7 m wide, below the tolerance) cut down from the top of a 20 m floor in
/// 2 m, to y = 10.1: the cells they cross stay inside, and every edge point between slits is
/// located against all slits to its right (the most expensive exact test per segment pair).
fn hair_slits(n: usize) -> Vec<Coord> {
    let mut pts = vec![(0.0, 0.0), (20.0, 0.0), (20.0, 20.0)];
    let step = 2.0 / n as f64;
    for i in (0..n).rev() {
        let x = 5.0 + i as f64 * step;
        pts.extend([(x + 1e-7, 20.0), (x + 1e-7, 10.1), (x, 10.1), (x, 20.0)]);
    }
    pts.push((0.0, 20.0));
    pts
}

/// A square of n cells a side 3e14 m from the origin, where the pad (1e-9 of the coordinates)
/// is larger than the floor: every cell is near all four walls.
fn far_square(n: usize) -> Vec<Coord> {
    let (o, w) = (3e14, n as f64 * 0.2 + 0.1);
    vec![(o, o), (o + w, o), (o + w, o + w), (o, o + w)]
}

/// The slowest refusal: margin spikes whose per-row work (counted first) is just under the
/// bound, plus 10 slots cut into the left wall at the top, so that the second count crosses the
/// bound only in the last rows, after preparing every row.
fn late_cut(spikes: usize, slots: usize) -> Vec<Coord> {
    let mut pts = margin_spikes(spikes);
    let h = 199.8;
    for i in 0..slots {
        let (y1, y0) = (h - (4 * i + 1) as f64 * 0.2, h - (4 * i + 3) as f64 * 0.2);
        pts.extend([(0.0, y1), (150.0, y1), (150.0, y0), (0.0, y0)]);
    }
    pts
}

type Family = (&'static str, fn(usize) -> Vec<Coord>, usize, usize);

const FAMILIES: [Family; 7] = [
    ("sawtooth", sawtooth, 10, 4000),
    ("comb", comb, 10, 2000),
    ("star", star, 16, 8000),
    ("margin_spikes", margin_spikes, 10, 8000),
    ("grid_slots", grid_slots, 1, 249),
    ("hair_slits", hair_slits, 10, 3000),
    ("far_square", far_square, 10, 998),
];

/// The largest n in `lo..=hi` whose outline the bound accepts, by bisection on the counted work
/// (`lo` must be accepted).
fn largest_accepted(make: fn(usize) -> Vec<Coord>, lo: usize, hi: usize) -> usize {
    let accepted = |n: usize| {
        frame_work_estimate(&make(n), 0.2).is_some_and(|w| w.units() <= MAX_FRAME_WORK)
    };
    assert!(accepted(lo));
    if accepted(hi) {
        return hi;
    }
    let (mut ok, mut bad) = (lo, hi);
    while bad - ok > 1 {
        let mid = ok + (bad - ok) / 2;
        if accepted(mid) {
            ok = mid;
        } else {
            bad = mid;
        }
    }
    ok
}

#[test]
fn worst_accepted_outlines_frame_within_the_budget_and_larger_ones_are_refused_within_it() {
    let limit = budget() * LOAD_FACTOR;
    let mut report = Vec::new();
    for (name, make, lo, hi) in FAMILIES {
        let n = largest_accepted(make, lo, hi);
        let poly = make(n);
        let w = frame_work_estimate(&poly, 0.2).expect("grid");
        let (took, got) = fastest_of_3(|| largest_inner_rectangle_checked(&poly, 0.2));
        assert!(
            matches!(got, Ok(Some(_))),
            "{name}({n}) accepted: {got:?}"
        );
        report.push(format!(
            "{name}({n}): {} vertices, {:.0}% of the bound, framed in {took:?}",
            poly.len(),
            w.units() as f64 * 100.0 / MAX_FRAME_WORK as f64
        ));
        assert!(took < limit, "{name}({n}) took {took:?} ({w:?})");
        if n < hi {
            // The family's largest member here is refused, and within the budget.
            let big = make(hi);
            let (took, got) = fastest_of_3(|| largest_inner_rectangle_checked(&big, 0.2));
            assert!(got.is_err(), "{name}({hi}): {got:?}");
            report.push(format!("{name}({hi}): refused in {took:?}"));
            assert!(took < limit, "{name}({hi}) refused in {took:?}");
        }
    }
    // The slowest refusal: the first count passes (its part of the work is under the bound),
    // the second crosses the bound in the last rows.
    let poly = late_cut(1250, 10);
    let w = frame_work_estimate(&poly, 0.2).expect("grid");
    let first_count = w.segments * FRAME_WORK_PER_SEGMENT
        + w.cells * FRAME_WORK_PER_CELL
        + w.segment_rows * FRAME_WORK_PER_SEGMENT_ROW
        + w.segment_row_steps * FRAME_WORK_PER_SEGMENT_ROW_STEP;
    assert!(first_count <= MAX_FRAME_WORK && w.units() > MAX_FRAME_WORK, "{w:?}");
    let (took, got) = fastest_of_3(|| largest_inner_rectangle_checked(&poly, 0.2));
    let note = got.expect_err("refused").note;
    assert!(note.contains("packed too densely near the grid"), "{note}");
    report.push(format!(
        "late_cut(1250, 10): first count at {:.0}% of the bound, refused in {took:?}",
        first_count as f64 * 100.0 / MAX_FRAME_WORK as f64
    ));
    assert!(took < limit, "late_cut refused in {took:?}");
    for line in &report {
        eprintln!("{line}");
    }
}
