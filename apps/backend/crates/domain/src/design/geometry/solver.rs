//! Port of `apps/api/app/domains/design/geometry/solver.py`: the constraint-based layout
//! generator for rectangular measured spaces.
//!
//! The core solver works on a rectangle with the entrance on the y=0 wall;
//! [`super::framing::frame_for_solver`] maps real measured floors (any outline, any entrance
//! wall) into that frame and the results back onto the plan. Two topologies:
//! - `spine`: a corridor perpendicular to the entrance wall, rooms in strips on both sides;
//! - `front`: a corridor parallel to the entrance wall; reception and storage in the front band,
//!   rooms behind.
//!
//! Rooms are assigned to strips and given lengths in centimetres; the output is checked by the
//! ported [`validate_layout`]. An infeasible request returns a report with issues and no rooms.
//!
//! # The integer model, and how Rust solves it
//!
//! Python builds an OR-Tools CP-SAT model and solves it with a 0.5 s deterministic budget. There is
//! no CP-SAT in Rust; this module solves the *same integer model* with its own deterministic
//! search. Per room `i` and strip `s` where the room fits (`depth + 1e-9 >= min_width` and
//! `lo <= hi`, with `lo = ceil(max(min_width, min_area / depth) * 100)` and
//! `hi = int(span * 100)`): one strip per room; `start, end` in `[0, hi]`; length between `lo`
//! and `min(hi, max(int(lo * 1.6), lo))`; no overlap with the other rooms of the strip or with
//! its reserved intervals. The objective, maximised, is CP-SAT's exactly: every created length
//! variable counts (an unused placement's length sits at its upper bound `hi`), minus, for
//! plumbing rooms when a sink exists and `prefer_rooms_near_plumbing` is set,
//! `|start + end - 2 * target|` of the used placement. The plumbing variables' domains
//! (`[0, 2 * hi]`) are hard constraints too: a used placement needs
//! `|start + end - 2 * target| <= 2 * hi`, and an unused one needs `|target| <= hi`.
//!
//! Search (all deterministic, bounded, and identical output for identical input):
//! - rooms whose parameters are identical in every strip are interchangeable; the search works
//!   on counts per such class, which is what keeps it small;
//! - without plumbing terms the optimum only depends on which rooms share each free interval of
//!   a strip (a "gap"): a gap of length `G` holding rooms `S` is feasible when the minimum
//!   lengths fit and is worth `min(G, sum of maximum lengths)`; an exact dynamic program over
//!   gaps and class counts finds the optimum ([`ASSIGNMENT_STATE_LIMIT`] bounds it);
//! - with plumbing terms an exact dynamic program over gaps, centimetre positions and class
//!   counts (range-maximum queries for the length choice) finds the optimum
//!   ([`POSITION_CELL_LIMIT`] bounds it);
//! - both are exact: the result is proven optimal, or the model proven infeasible
//!   ([`SolveStatus::Optimal`] / [`SolveStatus::Infeasible`]). Every program of up to
//!   [`EXACT_ROOM_LIMIT`] expanded rooms is solved exactly whenever the bound holds (with 10
//!   rooms of 10 different kinds and plumbing terms that is a total strip length of about 58 m;
//!   with three kinds of room it is several kilometres);
//! - beyond the bounds, or above [`EXACT_ROOM_LIMIT`] rooms when the exact search would exceed
//!   them, a constructive method (largest rooms first into the emptiest gap, then lengths
//!   widened in order) returns a solution that satisfies every hard constraint
//!   ([`SolveStatus::Feasible`]) or none ([`SolveStatus::Unknown`]); programs of more than
//!   [`MAX_ROOMS`] rooms, or strips longer than [`MAX_SPAN_CM`], are not searched
//!   ([`SolveStatus::Unknown`]).
//!
//! Rust layouts satisfy the same model and the same validator as Python's, and their objective is
//! at least CP-SAT's, but they are **not coordinate-identical** to OR-Tools output: the model
//! usually has many optimal solutions and the two searches pick different ones.

use crate::contracts::geometry::{
    Coord, GeometryValidationReport, LayoutPreferences, RoomPolygon, RoomRequirement, SiteGeometry,
    ValidationIssue,
};
use crate::geometry::{self, Bounds};
use crate::py;
use crate::spatial::validation::validate_layout;
use crate::DomainError;
use ss_core::Contract;

use super::framing::{frame_for_solver, MIN_FRAME_AREA};

/// Centimetres per meter in the integer model.
pub const CM: i64 = 100;
/// Programs up to this many expanded rooms are solved exactly (within the work bounds).
pub const EXACT_ROOM_LIMIT: usize = 10;
/// Programs with more expanded rooms are not searched.
pub const MAX_ROOMS: usize = 200;
/// Strips longer than this (centimetres, 10 km) are not searched.
pub const MAX_SPAN_CM: i64 = 1_000_000;
/// Work bound of the exact search with plumbing terms: positions times class-count states.
pub const POSITION_CELL_LIMIT: usize = 6_000_000;
/// Work bound of the exact search without plumbing terms: sub-multiset transitions per gap.
pub const ASSIGNMENT_STATE_LIMIT: usize = 2_000_000;
/// Scan steps per tie-break variant when reconstructing a solution of the exact search with
/// plumbing terms (the most seen on floors up to 60 m is about 6 million; one room on a 200 m
/// strip needs 24 million).
const RECONSTRUCT_STEP_LIMIT: usize = 30_000_000;

/// Direction rooms are laid along in a strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Rooms laid along x.
    X,
    /// Rooms laid along y.
    Y,
}

/// `Strip`: a band of the floor where rooms are laid side by side.
#[derive(Debug, Clone, PartialEq)]
pub struct Strip {
    pub name: &'static str,
    pub axis: Axis,
    pub origin: Coord,
    /// Length available along the axis (m).
    pub span: f64,
    /// Room depth perpendicular to the axis (m).
    pub depth: f64,
    /// `(start, end)` along the axis kept clear (m).
    pub reserved: Vec<(f64, f64)>,
}

/// The two layout topologies, in Python's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    Spine,
    Front,
}

impl Topology {
    pub const ALL: [Topology; 2] = [Topology::Spine, Topology::Front];

    pub fn as_str(self) -> &'static str {
        match self {
            Topology::Spine => "spine",
            Topology::Front => "front",
        }
    }
}

/// `Candidate`: one generated layout (or an infeasibility report with no rooms).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub rooms: Vec<RoomPolygon>,
    pub circulation: Vec<Vec<Coord>>,
    pub report: GeometryValidationReport,
}

/// The outcome of one solve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolveStatus {
    /// Proven optimal by the exact search.
    Optimal,
    /// Found by the constructive method; satisfies every hard constraint, not proven optimal.
    Feasible,
    /// Proven infeasible by the exact search.
    Infeasible,
    /// No solution found by the bounded constructive method (or the program exceeds
    /// [`MAX_ROOMS`] / [`MAX_SPAN_CM`]); not a proof of infeasibility.
    Unknown,
    /// Python's fast check: the program's minimum area exceeds the strips' usable area.
    CapacityExceeded,
    /// Some room fits no strip (Python returns before solving).
    NoEligibleStrip,
}

impl SolveStatus {
    /// Python's `_solve` returns a solution only for these.
    pub fn has_solution(self) -> bool {
        matches!(self, SolveStatus::Optimal | SolveStatus::Feasible)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SolveStatus::Optimal => "OPTIMAL",
            SolveStatus::Feasible => "FEASIBLE",
            SolveStatus::Infeasible => "INFEASIBLE",
            SolveStatus::Unknown => "UNKNOWN",
            SolveStatus::CapacityExceeded => "CAPACITY",
            SolveStatus::NoEligibleStrip => "NO_STRIP",
        }
    }
}

/// One room placed in a strip, in centimetres along the strip's axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub room: usize,
    pub strip: usize,
    pub start_cm: i64,
    pub length_cm: i64,
}

/// The result of [`solve`]: status, CP-SAT-equivalent objective value and the placements
/// (sorted by room index). `alternatives` holds other solutions with the same (optimal)
/// objective, from the search's other tie-breaks, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct SolveReport {
    pub status: SolveStatus,
    pub objective: Option<i64>,
    pub placements: Vec<Placement>,
    pub alternatives: Vec<Vec<Placement>>,
    /// Work done: DP cells (positions x states) for the exact search with plumbing terms,
    /// sub-multiset transitions for the one without, room-gap checks for the constructive method.
    /// Never above the bound that selected the method ([`POSITION_CELL_LIMIT`],
    /// [`ASSIGNMENT_STATE_LIMIT`], or `MAX_ROOMS` x gaps).
    pub work: usize,
}

impl SolveReport {
    fn none(status: SolveStatus) -> Self {
        SolveReport {
            status,
            objective: None,
            placements: vec![],
            alternatives: vec![],
            work: 0,
        }
    }

    /// Python's `_solve` return value: `(room_idx, strip_idx, start_m, length_m)` sorted, or
    /// `None`.
    pub fn solution(&self) -> Option<Vec<(usize, usize, f64, f64)>> {
        self.status
            .has_solution()
            .then(|| in_meters(&self.placements))
    }
}

fn in_meters(placements: &[Placement]) -> Vec<(usize, usize, f64, f64)> {
    placements
        .iter()
        .map(|p| {
            (
                p.room,
                p.strip,
                p.start_cm as f64 / CM as f64,
                p.length_cm as f64 / CM as f64,
            )
        })
        .collect()
}

fn not_rectangular() -> DomainError {
    DomainError::Value("foundation solver supports rectangular floors only".into())
}

/// `_bbox(site)`: the bounds of a rectangular floor; `Err` (Python's `ValueError`) otherwise.
pub fn bbox(site: &SiteGeometry) -> Result<Bounds, DomainError> {
    let b = Bounds::of(&site.floor_boundary).ok_or_else(not_rectangular)?;
    if (geometry::polygon_area(&site.floor_boundary) - b.area()).abs() > 1e-6 {
        return Err(not_rectangular());
    }
    Ok(b)
}

/// `_expand(reqs)`: each requirement repeated `count` times. `None` when the program has more
/// than [`MAX_ROOMS`] rooms.
pub fn expand(reqs: &[RoomRequirement]) -> Option<Vec<RoomRequirement>> {
    let mut out = Vec::new();
    for r in reqs {
        let n = usize::try_from(r.count.max(0)).ok()?;
        if out.len() + n > MAX_ROOMS {
            return None;
        }
        out.extend(std::iter::repeat_n(r.clone(), n));
    }
    Some(out)
}

/// `_strips(site, topology, corridor, corridor_x0)`: the strips and circulation polygons.
pub fn strips(
    site: &SiteGeometry,
    topology: Topology,
    corridor: f64,
    corridor_x0: Option<f64>,
) -> Result<(Vec<Strip>, Vec<Vec<Coord>>), DomainError> {
    let Bounds {
        minx: x0,
        miny: y0,
        maxx: x1,
        maxy: y1,
    } = bbox(site)?;
    let (w, h) = (x1 - x0, y1 - y0);
    let ent = site.entrances.first();
    let sorted_x = |o: &crate::contracts::geometry::Opening| {
        if o.end.0 < o.start.0 {
            (o.end.0, o.start.0)
        } else {
            (o.start.0, o.end.0)
        }
    };
    match topology {
        Topology::Spine => {
            let (c0, c1) = match corridor_x0 {
                None => {
                    let cx = match ent {
                        Some(e) => (e.start.0 + e.end.0) / 2.0,
                        None => x0 + w / 2.0,
                    };
                    (cx - corridor / 2.0, cx + corridor / 2.0)
                }
                Some(c) => (c, c + corridor),
            };
            let mut circ = vec![vec![(c0, y0), (c1, y0), (c1, y1), (c0, y1)]];
            let mut left_res = Vec::new();
            let mut right_res = Vec::new();
            if let Some(e) = ent {
                let (a, b) = sorted_x(e);
                if a < c0 - 1e-6 || b > c1 + 1e-6 {
                    // Door opens beside the corridor: reserve a vestibule from the door to the
                    // corridor.
                    let lo = if c0 < a { c0 } else { a };
                    let hi = if c1 > b { c1 } else { b };
                    circ.push(vec![
                        (lo, y0),
                        (hi, y0),
                        (hi, y0 + corridor),
                        (lo, y0 + corridor),
                    ]);
                    if a < c0 - 1e-6 {
                        left_res.push((0.0, corridor));
                    }
                    if b > c1 + 1e-6 {
                        right_res.push((0.0, corridor));
                    }
                }
            }
            Ok((
                vec![
                    Strip {
                        name: "left",
                        axis: Axis::Y,
                        origin: (x0, y0),
                        span: h,
                        depth: c0 - x0,
                        reserved: left_res,
                    },
                    Strip {
                        name: "right",
                        axis: Axis::Y,
                        origin: (c1, y0),
                        span: h,
                        depth: x1 - c1,
                        reserved: right_res,
                    },
                ],
                circ,
            ))
        }
        Topology::Front => {
            let half = (h - corridor) / 2.0;
            let front = if half < 4.0 { half } else { 4.0 };
            let (cy0, cy1) = (y0 + front, y0 + front + corridor);
            let mut circ = vec![vec![(x0, cy0), (x1, cy0), (x1, cy1), (x0, cy1)]];
            let mut reserved = Vec::new();
            if let Some(e) = ent {
                let (a, b) = sorted_x(e);
                reserved.push((a - x0 - 0.3, b - x0 + 0.3));
                circ.push(vec![
                    (a - 0.3, y0),
                    (b + 0.3, y0),
                    (b + 0.3, cy0),
                    (a - 0.3, cy0),
                ]);
            }
            Ok((
                vec![
                    Strip {
                        name: "front",
                        axis: Axis::X,
                        origin: (x0, y0),
                        span: w,
                        depth: front,
                        reserved,
                    },
                    Strip {
                        name: "back",
                        axis: Axis::X,
                        origin: (x0, cy1),
                        span: w,
                        depth: y1 - cy1,
                        reserved: vec![],
                    },
                ],
                circ,
            ))
        }
    }
}

// ---- the integer model --------------------------------------------------------------------------

/// Python's `int(x)` for a float, saturating (callers bound the magnitudes first).
fn py_int(x: f64) -> i64 {
    x.trunc() as i64
}

/// One room's parameters in one strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Place {
    lo: i64,
    max: i64,
    /// `2 * target` when the plumbing term applies to this placement.
    target2: Option<i64>,
}

/// A free interval `[lo, hi]` of a strip.
#[derive(Debug, Clone, Copy)]
struct Gap {
    strip: usize,
    lo: i64,
    hi: i64,
}

struct Model {
    hi: Vec<i64>,
    gaps: Vec<Gap>,
    /// Reserved intervals per strip (fixed, `[a, b)`), for the constraint check.
    reserved: Vec<Vec<(i64, i64)>>,
    /// Per room, per strip: the placement if the room may use the strip.
    allowed: Vec<Vec<Option<Place>>>,
    /// Sum of `hi` over every created placement (CP-SAT's unused lengths sit at `hi`).
    constant: i64,
}

enum Built {
    Model(Model),
    Status(SolveStatus),
}

fn build_model(
    strips: &[Strip],
    rooms: &[RoomRequirement],
    site: &SiteGeometry,
    prefs: &LayoutPreferences,
) -> Built {
    // Fast infeasibility: the program exceeds the usable strip area (Python's float sums).
    let capacity =
        py::sum_floats(strips.iter().map(|st| {
            st.depth * (st.span - py::sum_floats(st.reserved.iter().map(|(a, b)| b - a)))
        }));
    if py::sum_floats(rooms.iter().map(|r| r.min_area_sqm)) > capacity {
        return Built::Status(SolveStatus::CapacityExceeded);
    }
    let mut hi = Vec::with_capacity(strips.len());
    for st in strips {
        let h = st.span * CM as f64;
        if !h.is_finite() || h > MAX_SPAN_CM as f64 {
            return Built::Status(SolveStatus::Unknown);
        }
        hi.push(py_int(h));
    }
    let sink = site
        .fixture_locations
        .iter()
        .find(|f| f.kind == "sink")
        .map(|f| f.location);
    let mut allowed = Vec::with_capacity(rooms.len());
    let mut constant = 0i64;
    let mut forced_conflict = false;
    for r in rooms {
        let mut row: Vec<Option<Place>> = vec![None; strips.len()];
        let mut forced: Vec<usize> = Vec::new();
        for (s, st) in strips.iter().enumerate() {
            if st.depth + 1e-9 < r.min_width_m || st.depth == 0.0 {
                // (A zero depth with a sub-nanometre min width makes Python divide by zero.)
                continue;
            }
            let ratio = r.min_area_sqm / st.depth;
            let min_len = if ratio > r.min_width_m {
                ratio
            } else {
                r.min_width_m
            };
            let lo_f = (min_len * CM as f64).ceil();
            if lo_f.is_nan() || lo_f > hi[s] as f64 {
                continue;
            }
            let lo = lo_f as i64;
            let cap = py_int(lo as f64 * 1.6);
            let max = hi[s].min(cap.max(lo));
            let target2 = match sink {
                Some(sk) if prefs.prefer_rooms_near_plumbing && r.needs_plumbing => {
                    let rel = match st.axis {
                        Axis::Y => sk.1 - st.origin.1,
                        Axis::X => sk.0 - st.origin.0,
                    };
                    let t = rel * CM as f64;
                    if !t.is_finite() || t.abs() > 4.0 * MAX_SPAN_CM as f64 {
                        // Far outside every strip: |target| > hi in every strip.
                        forced.push(s);
                        Some(i64::MAX / 8)
                    } else {
                        let t = py_int(t);
                        if t.abs() > hi[s] {
                            forced.push(s);
                        }
                        Some(2 * t)
                    }
                }
                _ => None,
            };
            constant += hi[s];
            row[s] = Some(Place { lo, max, target2 });
        }
        if row.iter().all(Option::is_none) {
            return Built::Status(SolveStatus::NoEligibleStrip);
        }
        match forced.len() {
            0 => {}
            1 => {
                let f = forced[0];
                for (s, p) in row.iter_mut().enumerate() {
                    if s != f {
                        *p = None;
                    }
                }
            }
            _ => forced_conflict = true,
        }
        allowed.push(row);
    }
    let mut reserved = Vec::with_capacity(strips.len());
    let mut gaps = Vec::new();
    let mut reserved_conflict = false;
    for (s, st) in strips.iter().enumerate() {
        let mut fixed: Vec<(i64, i64)> = Vec::new();
        for (a, b) in &st.reserved {
            let a_c = py_int(a * CM as f64).max(0);
            let b_c = hi[s].min(py_int(b * CM as f64));
            if b_c > a_c {
                fixed.push((a_c, b_c));
            }
        }
        fixed.sort_unstable();
        if fixed.windows(2).any(|w| w[1].0 < w[0].1) {
            reserved_conflict = true;
        }
        let mut cursor = 0i64;
        for (a, b) in &fixed {
            if *a > cursor {
                gaps.push(Gap {
                    strip: s,
                    lo: cursor,
                    hi: *a,
                });
            }
            cursor = cursor.max(*b);
        }
        if hi[s] > cursor {
            gaps.push(Gap {
                strip: s,
                lo: cursor,
                hi: hi[s],
            });
        }
        reserved.push(fixed);
    }
    if forced_conflict || reserved_conflict {
        return Built::Status(SolveStatus::Infeasible);
    }
    Built::Model(Model {
        hi,
        gaps,
        reserved,
        allowed,
        constant,
    })
}

/// Rooms with identical parameters in every strip, grouped (classes in first-room order).
struct Class {
    rooms: Vec<usize>,
    place: Vec<Option<Place>>,
}

fn classes(model: &Model) -> Vec<Class> {
    let mut out: Vec<Class> = Vec::new();
    for (i, row) in model.allowed.iter().enumerate() {
        match out.iter_mut().find(|c| &c.place == row) {
            Some(c) => c.rooms.push(i),
            None => out.push(Class {
                rooms: vec![i],
                place: row.clone(),
            }),
        }
    }
    out
}

/// Mixed-radix indexing of "remaining rooms per class" states.
struct States {
    counts: Vec<usize>,
    strides: Vec<usize>,
    total: usize,
}

impl States {
    fn new(counts: Vec<usize>) -> Option<States> {
        let mut strides = Vec::with_capacity(counts.len());
        let mut total = 1usize;
        for c in &counts {
            strides.push(total);
            total = total.checked_mul(c + 1)?;
        }
        Some(States {
            counts,
            strides,
            total,
        })
    }

    fn digit(&self, state: usize, k: usize) -> usize {
        (state / self.strides[k]) % (self.counts[k] + 1)
    }

    fn full(&self) -> usize {
        self.total - 1
    }
}

const NEG: i64 = i64::MIN / 4;

fn penalty(p: &Place, start: i64, end: i64) -> i64 {
    p.target2.map_or(0, |t2| (start + end - t2).abs())
}

/// Gain of placing a room: its length minus the `hi` its unused length would have contributed,
/// minus the plumbing term. `None` when the plumbing domain constraint fails.
fn gain(p: &Place, hi: i64, start: i64, end: i64) -> Option<i64> {
    if let Some(t2) = p.target2 {
        if (start + end - t2).abs() > 2 * hi {
            return None;
        }
    }
    Some((end - start) - hi - penalty(p, start, end))
}

/// Placements per class: `(class, gap, start, end)`.
type ClassPlacements = Vec<(usize, usize, i64, i64)>;

/// Exact search without plumbing terms: rooms to gaps.
fn assignment_dp(
    model: &Model,
    cls: &[Class],
    st: &States,
    work_done: &mut usize,
) -> Option<Option<Vec<ClassPlacements>>> {
    // Work bound: sum over states of the sub-multisets per gap.
    let mut work = 1usize;
    for c in &st.counts {
        work = work.checked_mul((c + 1) * (c + 2) / 2)?;
    }
    if work.checked_mul(model.gaps.len().max(1))? > ASSIGNMENT_STATE_LIMIT {
        return None;
    }
    let l = st.total;
    let mut w = vec![NEG; l];
    w[st.full()] = 0;
    // back[g][state_out] = (state_in, subset as a state index)
    let mut back: Vec<Vec<(usize, usize)>> = Vec::with_capacity(model.gaps.len());
    for g in &model.gaps {
        let hi = model.hi[g.strip];
        let len = g.hi - g.lo;
        let mut next = w.clone();
        let mut bk: Vec<(usize, usize)> = (0..l).map(|s| (s, 0)).collect();
        for (r_in, &w_in) in w.iter().enumerate() {
            if w_in <= NEG / 2 {
                continue;
            }
            // Odometer over sub-multisets of r_in restricted to classes allowed in this strip.
            let limits: Vec<usize> = (0..cls.len())
                .map(|k| {
                    if cls[k].place[g.strip].is_some() {
                        st.digit(r_in, k)
                    } else {
                        0
                    }
                })
                .collect();
            let mut take = vec![0usize; cls.len()];
            loop {
                // advance odometer (skipping the empty subset, already in `next`)
                let mut k = 0;
                while k < take.len() {
                    if take[k] < limits[k] {
                        take[k] += 1;
                        break;
                    }
                    take[k] = 0;
                    k += 1;
                }
                if k == take.len() {
                    break;
                }
                *work_done += 1;
                let (mut sum_lo, mut sum_max, mut n) = (0i64, 0i64, 0i64);
                for (k, t) in take.iter().enumerate() {
                    if *t > 0 {
                        if let Some(p) = cls[k].place[g.strip] {
                            sum_lo += p.lo * *t as i64;
                            sum_max += p.max * *t as i64;
                            n += *t as i64;
                        }
                    }
                }
                if sum_lo > len {
                    continue;
                }
                let val = len.min(sum_max) - n * hi;
                let subset: usize = take
                    .iter()
                    .enumerate()
                    .map(|(k, t)| t * st.strides[k])
                    .sum();
                let out = r_in - subset;
                let cand = w_in + val;
                if cand > next[out] {
                    next[out] = cand;
                    bk[out] = (r_in, subset);
                }
            }
        }
        back.push(bk);
        w = next;
    }
    if w[0] <= NEG / 2 {
        return Some(None);
    }
    // Reconstruct the assignment, then lay it out once per tie-break variant.
    let mut chosen: Vec<(usize, usize)> = Vec::new(); // (gap, subset)
    let mut state = 0usize;
    for gi in (0..model.gaps.len()).rev() {
        let (r_in, subset) = back[gi][state];
        if subset != 0 {
            chosen.push((gi, subset));
        }
        state = r_in;
    }
    let mut variants: Vec<ClassPlacements> = Vec::new();
    for tie in Tie::ALL {
        let mut placements: ClassPlacements = Vec::new();
        for &(gi, subset) in &chosen {
            let g = model.gaps[gi];
            let len = g.hi - g.lo;
            let mut rooms: Vec<(usize, Place)> = Vec::new();
            for (k, c) in cls.iter().enumerate() {
                if let Some(p) = c.place[g.strip] {
                    for _ in 0..st.digit(subset, k) {
                        rooms.push((k, p));
                    }
                }
            }
            if tie.reverse_classes {
                rooms.reverse();
            }
            let sum_lo: i64 = rooms.iter().map(|(_, p)| p.lo).sum();
            let sum_max: i64 = rooms.iter().map(|(_, p)| p.max).sum();
            let used = len.min(sum_max);
            let mut extra = used - sum_lo;
            let mut cursor = if tie.pack_end { g.hi - used } else { g.lo };
            for (k, p) in rooms {
                let add = extra.min(p.max - p.lo);
                extra -= add;
                let length = p.lo + add;
                placements.push((k, gi, cursor, cursor + length));
                cursor += length;
            }
        }
        if !variants.contains(&placements) {
            variants.push(placements);
        }
    }
    Some(Some(variants))
}

/// Deterministic tie-breaks between optimal solutions. The search reconstructs one solution per
/// variant, in this order; all have the optimal objective. `generate_candidates` keeps the first
/// one the validator accepts (the model, like Python's, ignores openings other than the first
/// entrance, so equally good layouts can differ in what they cover).
#[derive(Debug, Clone, Copy)]
struct Tie {
    /// Within a gap, place classes in reverse order.
    reverse_classes: bool,
    /// Push rooms toward the end of their gap (otherwise toward its start).
    pack_end: bool,
}

impl Tie {
    const ALL: [Tie; 4] = [
        Tie {
            reverse_classes: false,
            pack_end: false,
        },
        Tie {
            reverse_classes: false,
            pack_end: true,
        },
        Tie {
            reverse_classes: true,
            pack_end: false,
        },
        Tie {
            reverse_classes: true,
            pack_end: true,
        },
    ];
}

/// Range-maximum (sparse table) over a slice.
struct Sparse {
    levels: Vec<Vec<i64>>,
}

impl Sparse {
    fn new(a: Vec<i64>) -> Sparse {
        let mut levels = vec![a];
        let mut width = 1usize;
        loop {
            let prev = &levels[levels.len() - 1];
            if width * 2 > prev.len() {
                break;
            }
            let next: Vec<i64> = (0..=prev.len() - width * 2)
                .map(|i| prev[i].max(prev[i + width]))
                .collect();
            levels.push(next);
            width *= 2;
        }
        Sparse { levels }
    }

    /// Maximum over `[l, r]` (inclusive); `NEG` for an empty range.
    fn query(&self, l: i64, r: i64) -> i64 {
        if l > r || r < 0 || self.levels[0].is_empty() {
            return NEG;
        }
        let l = l.max(0) as usize;
        let r = (r as usize).min(self.levels[0].len() - 1);
        if l > r {
            return NEG;
        }
        let span = r - l + 1;
        let k = (usize::BITS - 1 - span.leading_zeros()) as usize;
        let lv = &self.levels[k];
        lv[l].max(lv[r + 1 - (1 << k)])
    }
}

/// Exact search with plumbing terms: gaps x centimetre positions x class-count states.
fn position_dp(
    model: &Model,
    cls: &[Class],
    st: &States,
    work_done: &mut usize,
) -> Option<Option<Vec<ClassPlacements>>> {
    let positions: usize = model.gaps.iter().map(|g| (g.hi - g.lo + 1) as usize).sum();
    let cells = positions.checked_mul(st.total)?;
    if cells > POSITION_CELL_LIMIT {
        return None;
    }
    *work_done = cells;
    let l = st.total;
    let mut tables: Vec<Vec<i64>> = Vec::with_capacity(model.gaps.len());
    let mut carry = vec![NEG; l];
    carry[st.full()] = 0;
    for g in &model.gaps {
        let n = (g.hi - g.lo + 1) as usize;
        let hi = model.hi[g.strip];
        let mut v = vec![NEG; l * n];
        for r in (0..l).rev() {
            {
                let row = &mut v[r * n..(r + 1) * n];
                row[0] = row[0].max(carry[r]);
                for k in 1..n {
                    if row[k - 1] > row[k] {
                        row[k] = row[k - 1];
                    }
                }
            }
            let row: Vec<i64> = v[r * n..(r + 1) * n].to_vec();
            if row.iter().all(|x| *x <= NEG / 2) {
                continue;
            }
            let movable: Vec<(usize, Place)> = (0..cls.len())
                .filter(|k| st.digit(r, *k) >= 1)
                .filter_map(|k| cls[k].place[g.strip].map(|p| (k, p)))
                .collect();
            if movable.is_empty() {
                continue;
            }
            let shifted = |m: i64| -> Sparse {
                Sparse::new(
                    row.iter()
                        .enumerate()
                        .map(|(k, x)| {
                            if *x <= NEG / 2 {
                                NEG
                            } else {
                                x - m * (g.lo + k as i64)
                            }
                        })
                        .collect(),
                )
            };
            let need_plain = movable.iter().any(|(_, p)| p.target2.is_none());
            let need_plumb = movable.iter().any(|(_, p)| p.target2.is_some());
            let a_n = need_plain.then(|| shifted(1));
            let a_0 = need_plumb.then(|| shifted(0));
            let a_1 = need_plumb.then(|| shifted(2));
            for (k, p) in movable {
                let target = r - st.strides[k];
                for x in (g.lo + p.lo)..=g.hi {
                    let lo_y = g.lo.max(x - p.max);
                    let hi_y = x - p.lo;
                    let idx = |y: i64| y - g.lo;
                    let cand = match (p.target2, &a_n, &a_0, &a_1) {
                        (None, Some(t), _, _) => {
                            let b = t.query(idx(lo_y), idx(hi_y));
                            (b > NEG / 2).then(|| b + x - hi)
                        }
                        (Some(t2), _, Some(t0), Some(t1)) => {
                            let h2 = 2 * hi;
                            // x + y >= t2: gain t2 - 2y - hi
                            let a_lo = lo_y.max(t2 - x).max(t2 - h2 - x);
                            let a_hi = hi_y.min(t2 + h2 - x);
                            let ba = t1.query(idx(a_lo), idx(a_hi));
                            let ca = (ba > NEG / 2).then(|| ba + t2 - hi);
                            // x + y <= t2: gain 2x - t2 - hi
                            let b_lo = lo_y.max(t2 - h2 - x);
                            let b_hi = hi_y.min(t2 - x).min(t2 + h2 - x);
                            let bb = t0.query(idx(b_lo), idx(b_hi));
                            let cb = (bb > NEG / 2).then(|| bb + 2 * x - t2 - hi);
                            match (ca, cb) {
                                (Some(a), Some(b)) => Some(a.max(b)),
                                (a, b) => a.or(b),
                            }
                        }
                        _ => None,
                    };
                    if let Some(c) = cand {
                        let cell = &mut v[target * n + idx(x) as usize];
                        if c > *cell {
                            *cell = c;
                        }
                    }
                }
            }
        }
        for (r, c) in carry.iter_mut().enumerate() {
            *c = v[r * n + n - 1];
        }
        tables.push(v);
    }
    if carry[0] <= NEG / 2 {
        return Some(None);
    }
    // Reconstruct from the last gap, no rooms remaining, last position: once per tie-break.
    // Walking back toward the start of a gap (`pack_end`) scans every start position of every
    // class at every slack position, which grows with the square of the strip length; each
    // variant gets RECONSTRUCT_STEP_LIMIT scan steps (`None` when it runs out).
    let reconstruct = |tie: Tie| -> Option<ClassPlacements> {
        let mut steps = 0usize;
        let mut placements: ClassPlacements = Vec::new();
        let mut state = 0usize;
        let mut gi = model.gaps.len();
        let order: Vec<usize> = if tie.reverse_classes {
            (0..cls.len()).rev().collect()
        } else {
            (0..cls.len()).collect()
        };
        while gi > 0 {
            gi -= 1;
            let g = model.gaps[gi];
            let n = (g.hi - g.lo + 1) as usize;
            let hi = model.hi[g.strip];
            let v = &tables[gi];
            let mut x = g.hi;
            while x > g.lo {
                let cur = v[state * n + (x - g.lo) as usize];
                let slack = cur == v[state * n + (x - 1 - g.lo) as usize];
                // Packing toward the start of a gap leaves slack at its end: take the slack first
                // when walking back from the end. Packing toward the end does the opposite.
                if slack && !tie.pack_end {
                    x -= 1;
                    continue;
                }
                let mut found = None;
                'search: for &k in &order {
                    let Some(p) = cls[k].place[g.strip] else {
                        continue;
                    };
                    if st.digit(state, k) >= st.counts[k] {
                        continue;
                    }
                    let src = state + st.strides[k];
                    let lo_y = g.lo.max(x - p.max);
                    for y in lo_y..=(x - p.lo) {
                        steps += 1;
                        if steps > RECONSTRUCT_STEP_LIMIT {
                            return None;
                        }
                        let prev = v[src * n + (y - g.lo) as usize];
                        if prev <= NEG / 2 {
                            continue;
                        }
                        if let Some(gn) = gain(&p, hi, y, x) {
                            if prev + gn == cur {
                                found = Some((k, src, y));
                                break 'search;
                            }
                        }
                    }
                }
                match found {
                    Some((k, src, y)) => {
                        placements.push((k, gi, y, x));
                        state = src;
                        x = y;
                    }
                    None if slack => x -= 1,
                    None => return None,
                }
            }
        }
        (state == st.full()).then_some(placements)
    };
    let mut variants: Vec<ClassPlacements> = Vec::new();
    for (i, tie) in Tie::ALL.into_iter().enumerate() {
        // The first tie-break (toward the start) only scans at room ends; a later one that runs
        // out of its budget is dropped, since it is only an alternative optimum.
        let Some(placements) = reconstruct(tie) else {
            if i == 0 {
                return None;
            }
            continue;
        };
        if !variants.contains(&placements) {
            variants.push(placements);
        }
    }
    Some(Some(variants))
}

/// Constructive method (bounded): largest minimum length first into the allowed gap with the most
/// room left, then lengths widened in placement order.
fn constructive(model: &Model, rooms: usize, work_done: &mut usize) -> Option<Vec<Placement>> {
    *work_done = rooms * model.gaps.len();
    let min_lo = |i: usize| {
        model.allowed[i]
            .iter()
            .flatten()
            .map(|p| p.lo)
            .min()
            .unwrap_or(i64::MAX)
    };
    let mut order: Vec<usize> = (0..rooms).collect();
    order.sort_by_key(|i| (std::cmp::Reverse(min_lo(*i)), *i));
    let mut cursor: Vec<i64> = model.gaps.iter().map(|g| g.lo).collect();
    let mut in_gap: Vec<Vec<(usize, Place)>> = vec![Vec::new(); model.gaps.len()];
    for i in order {
        let mut best: Option<(i64, usize, Place)> = None;
        for (gi, g) in model.gaps.iter().enumerate() {
            if let Some(p) = model.allowed[i][g.strip] {
                let room_left = g.hi - cursor[gi];
                if room_left >= p.lo && best.is_none_or(|(b, _, _)| room_left > b) {
                    best = Some((room_left, gi, p));
                }
            }
        }
        let (_, gi, p) = best?;
        cursor[gi] += p.lo;
        in_gap[gi].push((i, p));
    }
    let mut out = Vec::with_capacity(rooms);
    for (gi, g) in model.gaps.iter().enumerate() {
        let mut extra = g.hi - cursor[gi];
        let mut pos = g.lo;
        for (i, p) in &in_gap[gi] {
            let add = extra.min(p.max - p.lo);
            extra -= add;
            let length = p.lo + add;
            out.push(Placement {
                room: *i,
                strip: g.strip,
                start_cm: pos,
                length_cm: length,
            });
            pos += length;
        }
    }
    out.sort_by_key(|p| p.room);
    Some(out)
}

/// Check every hard constraint of the integer model for `placements` (one per room, sorted by
/// room index) and return CP-SAT's objective value for them.
fn check_against(model: &Model, placements: &[Placement]) -> Result<i64, String> {
    if placements.len() != model.allowed.len() {
        return Err(format!(
            "{} placements for {} rooms",
            placements.len(),
            model.allowed.len()
        ));
    }
    let mut objective = model.constant;
    for (i, pl) in placements.iter().enumerate() {
        if pl.room != i {
            return Err(format!("placement {i} is for room {}", pl.room));
        }
        let p = model
            .allowed
            .get(i)
            .and_then(|row| row.get(pl.strip))
            .copied()
            .flatten()
            .ok_or_else(|| format!("room {i} may not use strip {}", pl.strip))?;
        let hi = model.hi[pl.strip];
        let (start, end) = (pl.start_cm, pl.start_cm + pl.length_cm);
        if pl.length_cm < p.lo || pl.length_cm > p.max {
            return Err(format!(
                "room {i} length {} outside [{}, {}]",
                pl.length_cm, p.lo, p.max
            ));
        }
        if start < 0 || end > hi {
            return Err(format!("room {i} [{start}, {end}) outside [0, {hi}]"));
        }
        for (a, b) in &model.reserved[pl.strip] {
            if start < *b && *a < end {
                return Err(format!("room {i} overlaps reserved [{a}, {b})"));
            }
        }
        for (j, other) in placements.iter().enumerate().skip(i + 1) {
            let (os, oe) = (other.start_cm, other.start_cm + other.length_cm);
            if other.strip == pl.strip && start < oe && os < end {
                return Err(format!("rooms {i} and {j} overlap"));
            }
        }
        objective += gain(&p, hi, start, end)
            .ok_or_else(|| format!("room {i} violates the plumbing distance domain"))?;
    }
    Ok(objective)
}

/// The integer model's verdict on `placements` for these strips and rooms: `Ok(objective)` when
/// every hard constraint holds, `Err(reason)` otherwise (also when the model itself is
/// infeasible before any placement). For tests and diagnostics.
pub fn check_solution(
    strips: &[Strip],
    rooms: &[RoomRequirement],
    site: &SiteGeometry,
    prefs: &LayoutPreferences,
    placements: &[Placement],
) -> Result<i64, String> {
    match build_model(strips, rooms, site, prefs) {
        Built::Model(m) => check_against(&m, placements),
        Built::Status(s) => Err(format!("model not solvable: {}", s.as_str())),
    }
}

/// `_solve(strips, rooms, site, prefs)`: solve the integer model (see the module documentation).
pub fn solve(
    strips: &[Strip],
    rooms: &[RoomRequirement],
    site: &SiteGeometry,
    prefs: &LayoutPreferences,
) -> SolveReport {
    if rooms.len() > MAX_ROOMS {
        return SolveReport::none(SolveStatus::Unknown);
    }
    let model = match build_model(strips, rooms, site, prefs) {
        Built::Model(m) => m,
        Built::Status(s) => return SolveReport::none(s),
    };
    let cls = classes(&model);
    let plumbing = cls
        .iter()
        .any(|c| c.place.iter().flatten().any(|p| p.target2.is_some()));
    let mut work = 0usize;
    let exact = States::new(cls.iter().map(|c| c.rooms.len()).collect()).and_then(|st| {
        let r = if plumbing {
            position_dp(&model, &cls, &st, &mut work)
        } else {
            assignment_dp(&model, &cls, &st, &mut work)
        };
        r.map(|res| (res, st))
    });
    let to_rooms = |per_class: ClassPlacements| -> Vec<Placement> {
        // Class placements to room indices: within a class, rooms in index order take the
        // placements in (strip, start) order.
        let mut by_class: Vec<Vec<(usize, i64, i64)>> = vec![Vec::new(); cls.len()];
        for (k, gi, s, e) in per_class {
            by_class[k].push((model.gaps[gi].strip, s, e));
        }
        let mut out = Vec::with_capacity(rooms.len());
        for (k, c) in cls.iter().enumerate() {
            by_class[k].sort_unstable();
            for (room, (strip, s, e)) in c.rooms.iter().zip(&by_class[k]) {
                out.push(Placement {
                    room: *room,
                    strip: *strip,
                    start_cm: *s,
                    length_cm: e - s,
                });
            }
        }
        out.sort_by_key(|p| p.room);
        out
    };
    let (variants, status) = match exact {
        Some((None, _)) => {
            return SolveReport {
                work,
                ..SolveReport::none(SolveStatus::Infeasible)
            }
        }
        Some((Some(per_variant), _)) => (
            per_variant.into_iter().map(to_rooms).collect::<Vec<_>>(),
            SolveStatus::Optimal,
        ),
        None => match constructive(&model, rooms.len(), &mut work) {
            Some(p) => (vec![p], SolveStatus::Feasible),
            None => {
                return SolveReport {
                    work,
                    ..SolveReport::none(SolveStatus::Unknown)
                }
            }
        },
    };
    let mut checked: Vec<(i64, Vec<Placement>)> = Vec::new();
    for v in variants {
        // The constructive method can break the plumbing domain constraint; the exact searches
        // cannot (a failure there would be a defect, and that variant is dropped).
        if let Ok(objective) = check_against(&model, &v) {
            if !checked.iter().any(|(_, w)| *w == v) {
                checked.push((objective, v));
            }
        }
    }
    let mut it = checked.into_iter();
    let Some((objective, placements)) = it.next() else {
        return SolveReport {
            work,
            ..SolveReport::none(SolveStatus::Unknown)
        };
    };
    SolveReport {
        status,
        work,
        objective: Some(objective),
        placements,
        alternatives: it
            .filter(|(o, _)| *o == objective)
            .map(|(_, v)| v)
            .collect(),
    }
}

/// `_to_polygon(st, start, length)`.
pub fn to_polygon(st: &Strip, start: f64, length: f64) -> Vec<Coord> {
    let (ox, oy) = st.origin;
    match st.axis {
        Axis::Y => vec![
            (ox, oy + start),
            (ox + st.depth, oy + start),
            (ox + st.depth, oy + start + length),
            (ox, oy + start + length),
        ],
        Axis::X => vec![
            (ox + start, oy),
            (ox + start + length, oy),
            (ox + start + length, oy + st.depth),
            (ox + start, oy + st.depth),
        ],
    }
}

/// `_corridor_offsets(site, corridor, rooms)`: spine corridor positions to try, at the entrance
/// first (`None`), then positions that keep both strips usable (duplicates kept, as in Python).
pub fn corridor_offsets(
    site: &SiteGeometry,
    corridor: f64,
    rooms: &[RoomRequirement],
) -> Result<Vec<Option<f64>>, DomainError> {
    let b = bbox(site)?;
    let (x0, x1) = (b.minx, b.maxx);
    let widest = rooms
        .iter()
        .map(|r| r.min_width_m)
        .reduce(|a, b| if b > a { b } else { a })
        .unwrap_or(0.0);
    let narrowest = rooms
        .iter()
        .map(|r| r.min_width_m)
        .reduce(|a, b| if b < a { b } else { a })
        .unwrap_or(0.0);
    let mut out = vec![None, Some(x0 + (x1 - x0 - corridor) / 2.0)];
    for d in [widest, narrowest] {
        out.push(Some(x0 + d));
        out.push(Some(x1 - d - corridor));
    }
    Ok(out
        .into_iter()
        .filter(|o| match o {
            None => true,
            Some(v) => x0 + narrowest <= *v && *v <= x1 - corridor - narrowest,
        })
        .collect())
}

/// What `_best_strips` tried for one topology.
#[derive(Debug, Clone, PartialEq)]
pub struct StripChoice {
    pub strips: Vec<Strip>,
    pub circulation: Vec<Vec<Coord>>,
    /// The corridor offset of the returned strips (`None`: at the entrance, or `front`).
    pub offset: Option<f64>,
    /// Every offset tried, in order, with its solve (the last one is the returned one when a
    /// solution was found).
    pub attempts: Vec<(Option<f64>, SolveReport)>,
    /// The solution, when an offset had one.
    pub solution: Option<SolveReport>,
}

/// Strips, circulation and the corridor offset they were built for.
type StripsAt = (Vec<Strip>, Vec<Vec<Coord>>, Option<f64>);

/// `_best_strips(site, topology, rooms, prefs)`: the first corridor offset with a solution, or
/// the first offset's strips with none.
pub fn best_strips(
    site: &SiteGeometry,
    topology: Topology,
    rooms: &[RoomRequirement],
    prefs: &LayoutPreferences,
) -> Result<StripChoice, DomainError> {
    let corridor = prefs.circulation_width_m;
    let offsets = match topology {
        Topology::Spine => corridor_offsets(site, corridor, rooms)?,
        Topology::Front => vec![None],
    };
    let mut first: Option<StripsAt> = None;
    let mut attempts = Vec::new();
    for off in offsets {
        let (st, circ) = strips(site, topology, corridor, off)?;
        if first.is_none() {
            first = Some((st.clone(), circ.clone(), off));
        }
        let rep = solve(&st, rooms, site, prefs);
        attempts.push((off, rep.clone()));
        if rep.status.has_solution() {
            return Ok(StripChoice {
                strips: st,
                circulation: circ,
                offset: off,
                attempts,
                solution: Some(rep),
            });
        }
    }
    let (st, circ, off) = first.ok_or_else(|| DomainError::Value("no corridor offset".into()))?;
    Ok(StripChoice {
        strips: st,
        circulation: circ,
        offset: off,
        attempts,
        solution: None,
    })
}

fn infeasible(name: &str, circulation: Vec<Vec<Coord>>, message: String) -> Candidate {
    Candidate {
        name: name.to_owned(),
        rooms: vec![],
        circulation,
        report: GeometryValidationReport::new(
            false,
            vec![ValidationIssue::new("INFEASIBLE", message, vec![])],
        ),
    }
}

/// Room polygons for a solution: coordinates rounded to 3 decimals, areas to 2, ids
/// `<room_type>-<n>` counted in room order. `Err` when a room fails its contract.
fn room_polygons(
    rooms: &[RoomRequirement],
    strips: &[Strip],
    placements: &[Placement],
) -> Result<Vec<RoomPolygon>, DomainError> {
    let mut counters: Vec<(String, usize)> = Vec::new();
    let mut polys = Vec::with_capacity(placements.len());
    for (i, s, start, length) in in_meters(placements) {
        let rt = rooms[i].room_type.clone();
        let n = match counters.iter_mut().find(|(t, _)| *t == rt) {
            Some((_, c)) => {
                *c += 1;
                *c
            }
            None => {
                counters.push((rt.clone(), 1));
                1
            }
        };
        let pts: Vec<Coord> = to_polygon(&strips[s], start, length)
            .into_iter()
            .map(|(x, y)| (py::round(x, 3), py::round(y, 3)))
            .collect();
        let area = py::round(geometry::polygon_area(&pts), 2);
        let room = RoomPolygon {
            room_id: format!("{rt}-{n}"),
            room_type: rt,
            polygon: pts,
            area_sqm: area,
        };
        room.validate()?;
        polys.push(room);
    }
    Ok(polys)
}

/// `generate_candidates(site, requirements, prefs, count)`: one candidate per topology
/// (`("spine", "front")[:max(1, count)]`). A candidate with no solution carries an `INFEASIBLE`
/// report and no rooms; otherwise its rooms (coordinates rounded to 3 decimals, areas to 2,
/// ids `<room_type>-<n>`) and circulation are mapped back onto the measured plan and its report
/// is `validate_layout`'s, with the framing notes appended to the disclaimer. When the search has
/// several optimal solutions, the first one the validator accepts is used (Python validates the
/// one CP-SAT returns); when none is accepted, the first one and its failing report.
///
/// `Err` only where Python raises: a generated room that fails its contract
/// (`RoomPolygon` validation, e.g. a room thinner than the coordinate rounding).
pub fn generate_candidates(
    site: &SiteGeometry,
    requirements: &[RoomRequirement],
    prefs: &LayoutPreferences,
    count: i64,
) -> Result<Vec<Candidate>, DomainError> {
    let Some(frame) = frame_for_solver(site, MIN_FRAME_AREA) else {
        return Ok(vec![infeasible(
            "none",
            vec![],
            "No usable rectangular area in this floor".into(),
        )]);
    };
    let rooms = expand(requirements);
    let framed = &frame.site;
    let n_topologies = usize::try_from(count.max(1)).unwrap_or(1).min(2);
    let mut out = Vec::new();
    for topology in &Topology::ALL[..n_topologies] {
        let choice = match &rooms {
            Some(rooms) => best_strips(framed, *topology, rooms, prefs)?,
            None => {
                // More rooms than the search accepts: no solution, as CP-SAT's time limit.
                let (st, circ) = strips(framed, *topology, prefs.circulation_width_m, None)?;
                StripChoice {
                    strips: st,
                    circulation: circ,
                    offset: None,
                    attempts: vec![],
                    solution: None,
                }
            }
        };
        let (Some(sol), Some(rooms)) = (choice.solution.as_ref(), rooms.as_ref()) else {
            out.push(infeasible(
                topology.as_str(),
                choice.circulation,
                format!("No {} layout satisfies the room program", topology.as_str()),
            ));
            continue;
        };
        // The first solution the validator accepts; the first solution when none is accepted.
        let mut chosen: Option<(Vec<RoomPolygon>, GeometryValidationReport)> = None;
        for placements in std::iter::once(&sol.placements).chain(&sol.alternatives) {
            let polys = room_polygons(rooms, &choice.strips, placements)?;
            let report = validate_layout(framed, &polys, requirements, Some(&choice.circulation));
            let valid = report.valid;
            if chosen.is_none() || valid {
                chosen = Some((polys, report));
            }
            if valid {
                break;
            }
        }
        let Some((mut polys, mut report)) = chosen else {
            continue;
        };
        if !frame.notes.is_empty() {
            report.disclaimer = format!("{} {}", report.disclaimer, frame.notes.join(" "));
        }
        let mut circulation = choice.circulation;
        if !frame.is_identity() {
            for p in &mut polys {
                p.polygon = p.polygon.iter().map(|pt| frame.to_site(*pt)).collect();
            }
            circulation = circulation
                .iter()
                .map(|ring| ring.iter().map(|pt| frame.to_site(*pt)).collect())
                .collect();
        }
        out.push(Candidate {
            name: topology.as_str().to_owned(),
            rooms: polys,
            circulation,
            report,
        });
    }
    Ok(out)
}
