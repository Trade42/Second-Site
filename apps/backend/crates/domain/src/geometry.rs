//! Planar geometry behind the layout code: the Shapely (GEOS 3.14) operations that
//! `contracts/geometry.py`, `domains/spatial/validation.py` and `domains/design/geometry/*.py`
//! call, ported for the shapes those modules build. Coordinates are meters.
//!
//! | Shapely call | Rust |
//! |---|---|
//! | `Polygon(points)` (ring closed automatically) | [`closed_ring`] |
//! | `poly.area`, `poly.envelope.area`, `poly.bounds` | [`polygon_area`], [`Bounds`] |
//! | `poly.is_valid`, `explain_validity(poly)` | [`explain_validity`], [`is_valid`] |
//! | `a.buffer(eps).contains(b)`, `prep(a.buffer(eps)).contains(box)` | [`contains_with_tolerance`] |
//! | `a.intersection(b).area`, `unary_union(...)`, `a.difference(b).area` | [`intersection_area`], [`area_inside_union`], [`union_intersection_area`] |
//! | `poly.intersection(LineString).length` | [`segment_length_inside`] |
//! | `poly.minimum_rotated_rectangle` side lengths | [`min_rotated_rectangle_min_side`] |
//! | `LineString.distance(Point)` | [`point_segment_distance`] |
//! | `box.contains(Point)` | [`Bounds::contains_strictly`] |
//! | `obstacle.intersection(box)` (a polygon) | [`clip_polygon_to_box`] |
//!
//! Exactness: predicates use exact orientation tests (`geo`'s robust kernel, as GEOS uses
//! exact-sign orientation). Areas use GEOS's shoelace formula. Intersection areas of convex
//! pieces are computed by exact-arithmetic-free Sutherland-Hodgman clipping (no snapping); only
//! overlays of two non-convex polygons go through `geo`'s `i_overlay` boolean operations, which
//! snap to an internal grid (about 1e-8 m at floor scale). The buffered containment test uses the
//! exact distance `<= eps` region where GEOS approximates the round joins with 16 segments per
//! quarter circle (a difference below 5e-9 m at `eps = 1e-6`).

use geo::kernels::{Kernel, Orientation, RobustKernel};
use geo::{BooleanOps, Coord, LineString, MultiPolygon, Polygon};

/// A 2-D point `(x, y)` (Python's `tuple[float, float]`).
pub type Pt = (f64, f64);

/// Shapely's `Polygon(points)` ring: the points, with the first point appended when the last one
/// differs from it (exact comparison).
pub fn closed_ring(points: &[Pt]) -> Vec<Pt> {
    let mut ring = points.to_vec();
    if let (Some(first), Some(last)) = (points.first(), points.last()) {
        if first != last {
            ring.push(*first);
        }
    }
    ring
}

/// GEOS `Area::ofRingSigned` (the shoelace formula with x measured from the first vertex) of a
/// closed ring, as an absolute value. Fewer than 3 coordinates give 0.
pub fn ring_area(ring: &[Pt]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let x0 = ring[0].0;
    let mut p1 = ring[0];
    let mut p2 = (ring[1].0 - x0, ring[1].1);
    let mut sum = 0.0;
    for next in &ring[2..] {
        let p0y = p1.1;
        p1 = p2;
        p2 = (next.0 - x0, next.1);
        sum += p1.0 * (p0y - p2.1);
    }
    (sum / 2.0).abs()
}

/// `Polygon(points).area`.
pub fn polygon_area(points: &[Pt]) -> f64 {
    ring_area(&closed_ring(points))
}

/// An axis-aligned envelope (`poly.bounds`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub minx: f64,
    pub miny: f64,
    pub maxx: f64,
    pub maxy: f64,
}

impl Bounds {
    /// The envelope of `points`; `None` when there are none.
    pub fn of(points: &[Pt]) -> Option<Bounds> {
        let (first, rest) = points.split_first()?;
        let mut b = Bounds {
            minx: first.0,
            miny: first.1,
            maxx: first.0,
            maxy: first.1,
        };
        for p in rest {
            // GEOS Envelope::expandToInclude keeps the first value on ties.
            if p.0 < b.minx {
                b.minx = p.0;
            }
            if p.0 > b.maxx {
                b.maxx = p.0;
            }
            if p.1 < b.miny {
                b.miny = p.1;
            }
            if p.1 > b.maxy {
                b.maxy = p.1;
            }
        }
        Some(b)
    }

    /// `poly.envelope.area`.
    pub fn area(&self) -> f64 {
        (self.maxx - self.minx) * (self.maxy - self.miny)
    }

    /// `box(...).contains(Point(p))`: strictly inside (not on the boundary).
    pub fn contains_strictly(&self, p: Pt) -> bool {
        self.minx < p.0 && p.0 < self.maxx && self.miny < p.1 && p.1 < self.maxy
    }

    /// The box as a closed counter-clockwise ring (`shapely.geometry.box`).
    pub fn ring(&self) -> Vec<Pt> {
        vec![
            (self.maxx, self.miny),
            (self.maxx, self.maxy),
            (self.minx, self.maxy),
            (self.minx, self.miny),
            (self.maxx, self.miny),
        ]
    }
}

// ---- predicates -------------------------------------------------------------------------------

fn gc(p: Pt) -> Coord<f64> {
    Coord { x: p.0, y: p.1 }
}

/// Exact orientation index: 1 left (counter-clockwise), -1 right, 0 collinear.
pub fn orientation(p: Pt, q: Pt, r: Pt) -> i32 {
    match RobustKernel::orient2d(gc(p), gc(q), gc(r)) {
        Orientation::CounterClockwise => 1,
        Orientation::Clockwise => -1,
        Orientation::Collinear => 0,
    }
}

fn dist(a: Pt, b: Pt) -> f64 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    (dx * dx + dy * dy).sqrt()
}

/// GEOS `Distance::pointToSegment` (the distance `LineString.distance(Point)` uses for one
/// segment).
pub fn point_segment_distance(p: Pt, a: Pt, b: Pt) -> f64 {
    if a == b {
        return dist(p, a);
    }
    let len2 = (b.0 - a.0) * (b.0 - a.0) + (b.1 - a.1) * (b.1 - a.1);
    let r = ((p.0 - a.0) * (b.0 - a.0) + (p.1 - a.1) * (b.1 - a.1)) / len2;
    if r <= 0.0 {
        return dist(p, a);
    }
    if r >= 1.0 {
        return dist(p, b);
    }
    let s = ((a.1 - p.1) * (b.0 - a.0) - (a.0 - p.0) * (b.1 - a.1)) / len2;
    s.abs() * len2.sqrt()
}

/// Where a point lies relative to a closed ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Interior,
    Boundary,
    Exterior,
}

/// GEOS `RayCrossingCounter::locatePointInRing` for a closed ring.
pub fn locate(p: Pt, ring: &[Pt]) -> Location {
    let mut crossings = 0u32;
    for w in ring.windows(2) {
        let (p1, p2) = (w[0], w[1]);
        if p1.0 < p.0 && p2.0 < p.0 {
            continue;
        }
        if p == p2 {
            return Location::Boundary;
        }
        if p1.1 == p.1 && p2.1 == p.1 {
            let (lo, hi) = if p1.0 > p2.0 {
                (p2.0, p1.0)
            } else {
                (p1.0, p2.0)
            };
            if p.0 >= lo && p.0 <= hi {
                return Location::Boundary;
            }
            continue;
        }
        if (p1.1 > p.1 && p2.1 <= p.1) || (p2.1 > p.1 && p1.1 <= p.1) {
            let mut orient = orientation(p1, p2, p);
            if orient == 0 {
                return Location::Boundary;
            }
            if p2.1 < p1.1 {
                orient = -orient;
            }
            if orient == 1 {
                crossings += 1;
            }
        }
    }
    if crossings % 2 == 1 {
        Location::Interior
    } else {
        Location::Exterior
    }
}

// ---- validity (GEOS IsValidOp for a polygon with one shell) ------------------------------------

/// Double-double arithmetic (GEOS `math::DD`), used for the intersection point GEOS reports.
#[derive(Clone, Copy)]
struct Dd {
    hi: f64,
    lo: f64,
}

const SPLIT: f64 = 134_217_729.0;

impl Dd {
    fn new(hi: f64) -> Self {
        Dd { hi, lo: 0.0 }
    }

    fn add(self, y: Dd) -> Dd {
        let (hi, lo, yhi, ylo) = (self.hi, self.lo, y.hi, y.lo);
        let s_ = hi + yhi;
        let t_ = lo + ylo;
        let mut e = s_ - hi;
        let f = t_ - lo;
        let mut s = s_ - e;
        let mut t = t_ - f;
        s = (yhi - e) + (hi - s);
        t = (ylo - f) + (lo - t);
        e = s + t_;
        let h_ = s_ + e;
        let h = e + (s_ - h_);
        e = t + h;
        let zhi = h_ + e;
        let zlo = e + (h_ - zhi);
        Dd { hi: zhi, lo: zlo }
    }

    fn sub(self, y: Dd) -> Dd {
        self.add(Dd {
            hi: -y.hi,
            lo: -y.lo,
        })
    }

    fn mul(self, y: Dd) -> Dd {
        let (hi, lo, yhi, ylo) = (self.hi, self.lo, y.hi, y.lo);
        let mut c_ = SPLIT * hi;
        let mut hx = c_ - hi;
        let mut c = SPLIT * yhi;
        hx = c_ - hx;
        let tx = hi - hx;
        let mut hy = c - yhi;
        c_ = hi * yhi;
        hy = c - hy;
        let ty = yhi - hy;
        c = ((((hx * hy - c_) + hx * ty) + tx * hy) + tx * ty) + (hi * ylo + lo * yhi);
        let zhi = c_ + c;
        hx = c_ - zhi;
        let zlo = c + hx;
        Dd { hi: zhi, lo: zlo }
    }

    fn div(self, y: Dd) -> Dd {
        let (hi, lo, yhi, ylo) = (self.hi, self.lo, y.hi, y.lo);
        let c_ = hi / yhi;
        let mut c = SPLIT * c_;
        let mut hc = c - c_;
        let mut u = SPLIT * yhi;
        hc = c - hc;
        let tc = c_ - hc;
        let mut hy = u - yhi;
        let u_ = c_ * yhi;
        hy = u - hy;
        let ty = yhi - hy;
        u = (((hc * hy - u_) + hc * ty) + tc * hy) + tc * ty;
        c = ((((hi - u_) - u) + lo) - c_ * ylo) / yhi;
        u = c_ + c;
        Dd {
            hi: u,
            lo: (c_ - u) + c,
        }
    }

    fn value(self) -> f64 {
        self.hi + self.lo
    }
}

/// GEOS `CGAlgorithmsDD::intersection` of the lines through two segments; `None` for parallel
/// lines or a non-finite result.
fn dd_line_intersection(p1: Pt, p2: Pt, q1: Pt, q2: Pt) -> Option<Pt> {
    let (q1x, q1y, q2x, q2y) = (Dd::new(q1.0), Dd::new(q1.1), Dd::new(q2.0), Dd::new(q2.1));
    let (p1x, p1y, p2x, p2y) = (Dd::new(p1.0), Dd::new(p1.1), Dd::new(p2.0), Dd::new(p2.1));
    let qdy = q2y.sub(q1y);
    let qdx = q2x.sub(q1x);
    let pdy = p2y.sub(p1y);
    let pdx = p2x.sub(p1x);
    let denom = qdy.mul(pdx).sub(qdx.mul(pdy));
    let numx = q2x
        .sub(q1x)
        .mul(p1y.sub(q1y))
        .sub(q2y.sub(q1y).mul(p1x.sub(q1x)));
    let frac_p = numx.div(denom);
    let x = p1x.add(p2x.sub(p1x).mul(frac_p)).value();
    let numy = p2x
        .sub(p1x)
        .mul(p1y.sub(q1y))
        .sub(p2y.sub(p1y).mul(p1x.sub(q1x)));
    let frac_q = numy.div(denom);
    let y = q1y.add(q2y.sub(q1y).mul(frac_q)).value();
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

fn env_contains(a: Pt, b: Pt, q: Pt) -> bool {
    a.0.min(b.0) <= q.0 && q.0 <= a.0.max(b.0) && a.1.min(b.1) <= q.1 && q.1 <= a.1.max(b.1)
}

fn envs_intersect(p1: Pt, p2: Pt, q1: Pt, q2: Pt) -> bool {
    !(q1.0.max(q2.0) < p1.0.min(p2.0)
        || q1.0.min(q2.0) > p1.0.max(p2.0)
        || q1.1.max(q2.1) < p1.1.min(p2.1)
        || q1.1.min(q2.1) > p1.1.max(p2.1))
}

/// Result of GEOS `LineIntersector::computeIntersection` for two segments.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SegInter {
    None,
    /// One point; `proper` when it is interior to both segments.
    Point {
        at: Pt,
        proper: bool,
    },
    /// A collinear overlap; `first` is GEOS's `getIntersection(0)`.
    Collinear {
        first: Pt,
    },
}

fn nearest_endpoint(p1: Pt, p2: Pt, q1: Pt, q2: Pt) -> Pt {
    let mut best = p1;
    let mut min = point_segment_distance(p1, q1, q2);
    for (pt, a, b) in [(p2, q1, q2), (q1, p1, p2), (q2, p1, p2)] {
        let d = point_segment_distance(pt, a, b);
        if d < min {
            min = d;
            best = pt;
        }
    }
    best
}

fn segment_intersection(p1: Pt, p2: Pt, q1: Pt, q2: Pt) -> SegInter {
    if !envs_intersect(p1, p2, q1, q2) {
        return SegInter::None;
    }
    let pq1 = orientation(p1, p2, q1);
    let pq2 = orientation(p1, p2, q2);
    if (pq1 > 0 && pq2 > 0) || (pq1 < 0 && pq2 < 0) {
        return SegInter::None;
    }
    let qp1 = orientation(q1, q2, p1);
    let qp2 = orientation(q1, q2, p2);
    if (qp1 > 0 && qp2 > 0) || (qp1 < 0 && qp2 < 0) {
        return SegInter::None;
    }
    if pq1 == 0 && pq2 == 0 && qp1 == 0 && qp2 == 0 {
        let q1in = env_contains(p1, p2, q1);
        let q2in = env_contains(p1, p2, q2);
        let p1in = env_contains(q1, q2, p1);
        let p2in = env_contains(q1, q2, p2);
        let point_or = |first: Pt, single: bool| {
            if single {
                SegInter::Point {
                    at: first,
                    proper: false,
                }
            } else {
                SegInter::Collinear { first }
            }
        };
        return if q1in && q2in {
            SegInter::Collinear { first: q1 }
        } else if p1in && p2in {
            SegInter::Collinear { first: p1 }
        } else if q1in && p1in {
            point_or(q1, q1 == p1 && !q2in && !p2in)
        } else if q1in && p2in {
            point_or(q1, q1 == p2 && !q2in && !p1in)
        } else if q2in && p1in {
            point_or(q2, q2 == p1 && !q1in && !p2in)
        } else if q2in && p2in {
            point_or(q2, q2 == p2 && !q1in && !p1in)
        } else {
            SegInter::None
        };
    }
    if pq1 == 0 || pq2 == 0 || qp1 == 0 || qp2 == 0 {
        let at = if p1 == q1 || p1 == q2 {
            p1
        } else if p2 == q1 || p2 == q2 {
            p2
        } else if pq1 == 0 {
            q1
        } else if pq2 == 0 {
            q2
        } else if qp1 == 0 {
            p1
        } else {
            p2
        };
        return SegInter::Point { at, proper: false };
    }
    let at = match dd_line_intersection(p1, p2, q1, q2) {
        Some(pt) if env_contains(p1, p2, pt) && env_contains(q1, q2, pt) => pt,
        _ => nearest_endpoint(p1, p2, q1, q2),
    };
    SegInter::Point { at, proper: true }
}

/// C++ `ostream << double` at precision 15 (`%.15g`), as GEOS writes error locations.
pub fn format_g15(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    const P: i32 = 15;
    let sci = format!("{:.*e}", (P - 1) as usize, x);
    let Some((mant, exp)) = sci.split_once('e') else {
        return sci;
    };
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            s.to_owned()
        }
    };
    if (-4..P).contains(&exp) {
        let decimals = (P - 1 - exp).max(0) as usize;
        strip(&format!("{x:.decimals$}"))
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip(mant), exp.abs())
    }
}

fn located(kind: &str, p: Pt) -> String {
    format!("{kind}[{} {}]", format_g15(p.0), format_g15(p.1))
}

/// `shapely.validation.explain_validity(Polygon(points))` for a polygon without holes:
/// `"Valid Geometry"`, or GEOS's reason with its location, e.g. `"Self-intersection[1 1]"`.
///
/// The checks and their order are GEOS 3.14 `IsValidOp`: invalid (non-finite) coordinates, too
/// few distinct points (`< 4` in the closed ring), then the ring's segment intersections: a
/// proper crossing or a collinear overlap is `Self-intersection`, any other contact between
/// non-adjacent segments is `Ring Self-intersection`. A crossing point is computed in
/// double-double arithmetic as GEOS does. When a ring has several invalid intersections that
/// report different reasons or locations, the one GEOS reports depends on its spatial-index
/// traversal, which this port does not reproduce: it visits monotone-chain pairs in ring order
/// (see `ring_intersection_error`), so the reason text may then differ; the valid/invalid
/// decision does not.
pub fn explain_validity(points: &[Pt]) -> String {
    let ring = closed_ring(points);
    if ring.is_empty() {
        return "Valid Geometry".into();
    }
    if let Some(p) = ring.iter().find(|p| !p.0.is_finite() || !p.1.is_finite()) {
        return located("Invalid Coordinate", *p);
    }
    let mut pts: Vec<Pt> = Vec::with_capacity(ring.len());
    for p in &ring {
        if pts.last() != Some(p) {
            pts.push(*p);
        }
    }
    if pts.len() < 4 {
        return located("Too few points in geometry component", ring[0]);
    }
    match ring_intersection_error(&pts) {
        Some((kind, at)) => located(kind, at),
        None => "Valid Geometry".into(),
    }
}

/// GEOS `MonotoneChainBuilder::findChainEnd` quadrant of a segment.
fn quadrant(a: Pt, b: Pt) -> u8 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    match (dx >= 0.0, dy >= 0.0) {
        (true, true) => 0,
        (true, false) => 3,
        (false, true) => 1,
        (false, false) => 2,
    }
}

/// Monotone chains `(start, end)` (vertex indices) of a ring without repeated points.
fn monotone_chains(pts: &[Pt]) -> Vec<(usize, usize)> {
    let n = pts.len();
    let mut out = Vec::new();
    let mut start = 0;
    while start + 1 < n {
        let quad = quadrant(pts[start], pts[start + 1]);
        let mut last = start + 1;
        while last < n && quadrant(pts[last - 1], pts[last]) == quad {
            last += 1;
        }
        out.push((start, last - 1));
        start = last - 1;
    }
    out
}

fn chain_env(pts: &[Pt], (a, b): (usize, usize)) -> Bounds {
    Bounds::of(&pts[a..=b]).unwrap_or(Bounds {
        minx: 0.0,
        miny: 0.0,
        maxx: 0.0,
        maxy: 0.0,
    })
}

/// The invalid intersection GEOS reports for a closed ring without repeated points:
/// segment pairs are visited chain pair by chain pair (`MonotoneChain::computeOverlaps`
/// recursion order); within the first chain pair that has an invalid intersection the last one
/// found is reported.
fn ring_intersection_error(pts: &[Pt]) -> Option<(&'static str, Pt)> {
    let classify = |i: usize, j: usize| -> Option<(&'static str, Pt)> {
        if i == j {
            return None;
        }
        match segment_intersection(pts[i], pts[i + 1], pts[j], pts[j + 1]) {
            SegInter::None => None,
            SegInter::Collinear { first } => Some(("Self-intersection", first)),
            SegInter::Point { at, proper: true } => Some(("Self-intersection", at)),
            SegInter::Point { at, proper: false } => {
                let delta = i.abs_diff(j);
                let adjacent = delta <= 1 || delta >= pts.len() - 2;
                (!adjacent).then_some(("Ring Self-intersection", at))
            }
        }
    };
    fn overlaps(
        pts: &[Pt],
        (s0, e0): (usize, usize),
        (s1, e1): (usize, usize),
        found: &mut Option<(&'static str, Pt)>,
        classify: &dyn Fn(usize, usize) -> Option<(&'static str, Pt)>,
    ) {
        if e0 - s0 == 1 && e1 - s1 == 1 {
            if let Some(f) = classify(s0, s1) {
                *found = Some(f);
            }
            return;
        }
        if !envs_intersect(pts[s0], pts[e0], pts[s1], pts[e1]) {
            return;
        }
        let m0 = (s0 + e0) / 2;
        let m1 = (s1 + e1) / 2;
        if s0 < m0 {
            if s1 < m1 {
                overlaps(pts, (s0, m0), (s1, m1), found, classify);
            }
            if m1 < e1 {
                overlaps(pts, (s0, m0), (m1, e1), found, classify);
            }
        }
        if m0 < e0 {
            if s1 < m1 {
                overlaps(pts, (m0, e0), (s1, m1), found, classify);
            }
            if m1 < e1 {
                overlaps(pts, (m0, e0), (m1, e1), found, classify);
            }
        }
    }
    let chains = monotone_chains(pts);
    let envs: Vec<Bounds> = chains.iter().map(|c| chain_env(pts, *c)).collect();
    let touch = |a: &Bounds, b: &Bounds| {
        !(b.minx > a.maxx || b.maxx < a.minx || b.miny > a.maxy || b.maxy < a.miny)
    };
    for qi in 0..chains.len() {
        for ti in (qi + 1)..chains.len() {
            if !touch(&envs[qi], &envs[ti]) {
                continue;
            }
            let mut found = None;
            overlaps(pts, chains[qi], chains[ti], &mut found, &classify);
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

/// Every reason GEOS could report for a ring with several invalid intersections: one
/// `explain_validity`-style string per invalid segment pair (in segment order, duplicates
/// removed). GEOS reports one of these; [`explain_validity`] picks by chain order. For tests and
/// diagnostics.
pub fn invalidity_candidates(points: &[Pt]) -> Vec<String> {
    let ring = closed_ring(points);
    let mut pts: Vec<Pt> = Vec::with_capacity(ring.len());
    for p in &ring {
        if pts.last() != Some(p) {
            pts.push(*p);
        }
    }
    let mut out: Vec<String> = Vec::new();
    if pts.len() < 4 {
        return out;
    }
    let nseg = pts.len() - 1;
    for i in 0..nseg {
        for j in (i + 1)..nseg {
            let found = match segment_intersection(pts[i], pts[i + 1], pts[j], pts[j + 1]) {
                SegInter::None => None,
                SegInter::Collinear { first } => Some(located("Self-intersection", first)),
                SegInter::Point { at, proper: true } => Some(located("Self-intersection", at)),
                SegInter::Point { at, proper: false } => {
                    let delta = j - i;
                    (delta > 1 && delta < pts.len() - 2)
                        .then(|| located("Ring Self-intersection", at))
                }
            };
            if let Some(f) = found {
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
    }
    out
}

/// `Polygon(points).is_valid`.
pub fn is_valid(points: &[Pt]) -> bool {
    explain_validity(points) == "Valid Geometry"
}

// ---- containment within a tolerance ------------------------------------------------------------

/// Parameter interval `[t0, t1]` of the line `p + t d` within distance `eps` of segment `a b`.
fn stadium_interval(p: Pt, d: Pt, a: Pt, b: Pt, eps: f64) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    let mut take = |iv: Option<(f64, f64)>| {
        if let Some((x, y)) = iv {
            if x <= y {
                lo = lo.min(x);
                hi = hi.max(y);
            }
        }
    };
    // The two end disks.
    for c in [a, b] {
        let (wx, wy) = (p.0 - c.0, p.1 - c.1);
        let qa = d.0 * d.0 + d.1 * d.1;
        let qb = 2.0 * (d.0 * wx + d.1 * wy);
        let qc = wx * wx + wy * wy - eps * eps;
        let disc = qb * qb - 4.0 * qa * qc;
        if qa > 0.0 && disc >= 0.0 {
            let s = disc.sqrt();
            take(Some(((-qb - s) / (2.0 * qa), (-qb + s) / (2.0 * qa))));
        }
    }
    // The band of points whose projection falls on the segment.
    let (ux, uy) = (b.0 - a.0, b.1 - a.1);
    let len2 = ux * ux + uy * uy;
    if len2 > 0.0 {
        let len = len2.sqrt();
        let (wx, wy) = (p.0 - a.0, p.1 - a.1);
        // along(t) = (w + t d) . u in [0, len2]; across(t) = cross(u, w + t d) in [-eps len, eps len]
        let along0 = wx * ux + wy * uy;
        let along1 = d.0 * ux + d.1 * uy;
        let across0 = ux * wy - uy * wx;
        let across1 = ux * d.1 - uy * d.0;
        let band = |c0: f64, c1: f64, min: f64, max: f64| -> Option<(f64, f64)> {
            if c1 == 0.0 {
                (min <= c0 && c0 <= max).then_some((f64::NEG_INFINITY, f64::INFINITY))
            } else {
                let t0 = (min - c0) / c1;
                let t1 = (max - c0) / c1;
                Some((t0.min(t1), t0.max(t1)))
            }
        };
        if let (Some(x), Some(y)) = (
            band(along0, along1, 0.0, len2),
            band(across0, across1, -eps * len, eps * len),
        ) {
            take(Some((x.0.max(y.0), x.1.min(y.1))));
        }
    }
    (lo <= hi).then_some((lo, hi))
}

/// Parameters in `[0, 1]` where segment `p q` meets segment `a b` (crossings, touches and the
/// ends of collinear overlaps).
fn meeting_params(p: Pt, q: Pt, a: Pt, b: Pt, out: &mut Vec<f64>) {
    let d = (q.0 - p.0, q.1 - p.1);
    let len2 = d.0 * d.0 + d.1 * d.1;
    if len2 == 0.0 {
        return;
    }
    let param = |x: Pt| ((x.0 - p.0) * d.0 + (x.1 - p.1) * d.1) / len2;
    match segment_intersection(p, q, a, b) {
        SegInter::None => {}
        SegInter::Point { at, .. } => out.push(param(at)),
        SegInter::Collinear { .. } => {
            for x in [a, b, p, q] {
                if env_contains(p, q, x) && env_contains(a, b, x) {
                    out.push(param(x));
                }
            }
        }
    }
}

/// `Polygon(outer).buffer(eps).contains(Polygon(inner))` (and the prepared form
/// `prep(poly.buffer(eps)).contains(box)`): every point of `inner` lies inside `outer` or within
/// `eps` of its boundary. Both arguments are rings (closed or not). `outer` is treated as a
/// simple polygon; an inner shape with no extent is never contained, as in GEOS.
pub fn contains_with_tolerance(outer: &[Pt], inner: &[Pt], eps: f64) -> bool {
    let o = closed_ring(outer);
    let r = closed_ring(inner);
    if o.len() < 4 || r.len() < 2 {
        return false;
    }
    // Every edge of `inner` must be covered; a simply connected region that contains a closed
    // curve contains its interior too.
    let mut any_extent = false;
    for e in r.windows(2) {
        let (p, q) = (e[0], e[1]);
        let d = (q.0 - p.0, q.1 - p.1);
        if d.0 == 0.0 && d.1 == 0.0 {
            if locate(p, &o) == Location::Exterior
                && !o
                    .windows(2)
                    .any(|s| point_segment_distance(p, s[0], s[1]) <= eps)
            {
                return false;
            }
            continue;
        }
        any_extent = true;
        let mut cuts = vec![0.0, 1.0];
        let mut bands: Vec<(f64, f64)> = Vec::new();
        for s in o.windows(2) {
            meeting_params(p, q, s[0], s[1], &mut cuts);
            if let Some((t0, t1)) = stadium_interval(p, d, s[0], s[1], eps) {
                if t1 >= 0.0 && t0 <= 1.0 {
                    bands.push((t0, t1));
                    cuts.push(t0.clamp(0.0, 1.0));
                    cuts.push(t1.clamp(0.0, 1.0));
                }
            }
        }
        cuts.retain(|t| (0.0..=1.0).contains(t));
        cuts.sort_by(f64::total_cmp);
        for w in cuts.windows(2) {
            if w[1] <= w[0] {
                continue;
            }
            let t = (w[0] + w[1]) / 2.0;
            if bands.iter().any(|(t0, t1)| *t0 <= t && t <= *t1) {
                continue;
            }
            let m = (p.0 + t * d.0, p.1 + t * d.1);
            if locate(m, &o) == Location::Exterior {
                return false;
            }
        }
        // The end points: covered when inside or near the boundary.
        for x in [p, q] {
            if locate(x, &o) == Location::Exterior
                && !o
                    .windows(2)
                    .any(|s| point_segment_distance(x, s[0], s[1]) <= eps)
            {
                return false;
            }
        }
    }
    any_extent && ring_area(&r) > 0.0
}

// ---- areas of intersections --------------------------------------------------------------------

fn signed_ring_area(ring: &[Pt]) -> f64 {
    // Same shoelace as `ring_area`, keeping the sign (positive for counter-clockwise).
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let x0 = ring[0].0;
    let mut p1 = ring[0];
    let mut p2 = (ring[1].0 - x0, ring[1].1);
    let mut sum = 0.0;
    for next in &ring[2..] {
        let p0y = p1.1;
        p1 = p2;
        p2 = (next.0 - x0, next.1);
        sum += p1.0 * (p0y - p2.1);
    }
    -(sum / 2.0)
}

/// True when the ring (closed) is convex: no two turns of opposite sign. Collinear vertices are
/// allowed. A ring with no extent is not convex.
pub fn is_convex(ring: &[Pt]) -> bool {
    let r = closed_ring(ring);
    let mut pts: Vec<Pt> = Vec::with_capacity(r.len());
    for p in &r {
        if pts.last() != Some(p) {
            pts.push(*p);
        }
    }
    if pts.len() < 4 {
        return false;
    }
    pts.pop();
    let n = pts.len();
    let mut sign = 0;
    for i in 0..n {
        let o = orientation(pts[i], pts[(i + 1) % n], pts[(i + 2) % n]);
        if o != 0 {
            if sign == 0 {
                sign = o;
            } else if o != sign {
                return false;
            }
        }
    }
    sign != 0 && is_valid(&pts)
}

/// Clip `subject` (a closed ring) by the convex ring `clip` (Sutherland-Hodgman). The area of the
/// result equals the area of the intersection; the result may contain zero-width spikes.
fn clip_convex(subject: &[Pt], clip: &[Pt]) -> Vec<Pt> {
    let c = closed_ring(clip);
    let ccw = signed_ring_area(&c) > 0.0;
    let mut out: Vec<Pt> = closed_ring(subject);
    if out.last() == out.first() {
        out.pop();
    }
    for e in c.windows(2) {
        let (a, b) = if ccw { (e[0], e[1]) } else { (e[1], e[0]) };
        if a == b || out.is_empty() {
            continue;
        }
        let input = std::mem::take(&mut out);
        let inside = |p: Pt| orientation(a, b, p) >= 0;
        let cross = |p: Pt, q: Pt| -> Pt {
            // Intersection of segment p q with the line a b. Axis-aligned clip lines and
            // axis-aligned segments give exact coordinates (rectangles clip to exact rectangles).
            if a.0 == b.0 && p.0 != q.0 {
                let y = if p.1 == q.1 {
                    p.1
                } else {
                    p.1 + (a.0 - p.0) * (q.1 - p.1) / (q.0 - p.0)
                };
                return (a.0, y);
            }
            if a.1 == b.1 && p.1 != q.1 {
                let x = if p.0 == q.0 {
                    p.0
                } else {
                    p.0 + (a.1 - p.1) * (q.0 - p.0) / (q.1 - p.1)
                };
                return (x, a.1);
            }
            let den = (q.0 - p.0) * (b.1 - a.1) - (q.1 - p.1) * (b.0 - a.0);
            if den == 0.0 {
                return q;
            }
            let t = ((a.0 - p.0) * (b.1 - a.1) - (a.1 - p.1) * (b.0 - a.0)) / den;
            (p.0 + t * (q.0 - p.0), p.1 + t * (q.1 - p.1))
        };
        let n = input.len();
        for i in 0..n {
            let cur = input[i];
            let prev = input[(i + n - 1) % n];
            match (inside(prev), inside(cur)) {
                (true, true) => out.push(cur),
                (true, false) => out.push(cross(prev, cur)),
                (false, true) => {
                    out.push(cross(prev, cur));
                    out.push(cur);
                }
                (false, false) => {}
            }
        }
    }
    if let Some(first) = out.first().copied() {
        out.push(first);
    }
    out
}

fn to_geo(ring: &[Pt]) -> Polygon<f64> {
    Polygon::new(
        LineString::from(closed_ring(ring).into_iter().map(gc).collect::<Vec<_>>()),
        vec![],
    )
}

fn multi_area(mp: &MultiPolygon<f64>) -> f64 {
    mp.0.iter()
        .map(|p| {
            let ext: Vec<Pt> = p.exterior().coords().map(|c| (c.x, c.y)).collect();
            let holes: f64 = p
                .interiors()
                .iter()
                .map(|h| ring_area(&h.coords().map(|c| (c.x, c.y)).collect::<Vec<_>>()))
                .sum();
            ring_area(&ext) - holes
        })
        .sum()
}

/// Ear-clipping triangulation of a valid simple ring (closed or not); `None` when the ring is
/// invalid or the ears cannot be found.
fn triangulate(ring: &[Pt]) -> Option<Vec<Vec<Pt>>> {
    if !is_valid(ring) {
        return None;
    }
    let mut pts: Vec<Pt> = Vec::new();
    for p in closed_ring(ring) {
        if pts.last() != Some(&p) {
            pts.push(p);
        }
    }
    pts.pop();
    if signed_ring_area(&closed_ring(&pts)) < 0.0 {
        pts.reverse();
    }
    let in_triangle = |p: Pt, a: Pt, b: Pt, c: Pt| {
        orientation(a, b, p) >= 0 && orientation(b, c, p) >= 0 && orientation(c, a, p) >= 0
    };
    let mut idx: Vec<usize> = (0..pts.len()).collect();
    let mut out = Vec::new();
    while idx.len() > 3 {
        let m = idx.len();
        let mut clipped = false;
        for k in 0..m {
            let (a, b, c) = (
                pts[idx[(k + m - 1) % m]],
                pts[idx[k]],
                pts[idx[(k + 1) % m]],
            );
            match orientation(a, b, c) {
                o if o < 0 => continue,
                0 => {
                    idx.remove(k);
                    clipped = true;
                    break;
                }
                _ => {}
            }
            let blocked = idx.iter().any(|&j| {
                let p = pts[j];
                p != a && p != b && p != c && in_triangle(p, a, b, c)
            });
            if blocked {
                continue;
            }
            out.push(vec![a, b, c]);
            idx.remove(k);
            clipped = true;
            break;
        }
        if !clipped {
            return None;
        }
    }
    if idx.len() == 3 && orientation(pts[idx[0]], pts[idx[1]], pts[idx[2]]) > 0 {
        out.push(vec![pts[idx[0]], pts[idx[1]], pts[idx[2]]]);
    }
    Some(out)
}

/// Interior-disjoint convex parts covering a polygon: the ring itself when convex, else its
/// triangles.
fn convex_parts(ring: &[Pt]) -> Option<Vec<Vec<Pt>>> {
    if is_convex(ring) {
        Some(vec![closed_ring(ring)])
    } else {
        triangulate(ring)
    }
}

/// Upper bound on convex clips for one intersection-area evaluation before falling back to
/// `geo` overlays.
const MAX_CLIPS: usize = 50_000;

/// Area of `first ∩ rest[0] ∩ rest[1] ∩ ...`: `first` is clipped by every combination of convex
/// parts of the others (the parts of each polygon are interior-disjoint and cover it, so the
/// areas add up). `None` when a polygon cannot be decomposed or the work exceeds [`MAX_CLIPS`].
fn intersection_area_of(first: &[Pt], rest: &[&[Pt]]) -> Option<f64> {
    let parts: Vec<Vec<Vec<Pt>>> = rest
        .iter()
        .map(|r| convex_parts(r))
        .collect::<Option<_>>()?;
    let mut combos = 1usize;
    for p in &parts {
        combos = combos.checked_mul(p.len().max(1))?;
    }
    if combos > MAX_CLIPS {
        return None;
    }
    fn go(cur: &[Pt], parts: &[Vec<Vec<Pt>>]) -> f64 {
        match parts.split_first() {
            None => ring_area(cur),
            Some((head, tail)) => head
                .iter()
                .map(|c| {
                    let clipped = clip_convex(cur, c);
                    if ring_area(&clipped) == 0.0 {
                        0.0
                    } else {
                        go(&clipped, tail)
                    }
                })
                .sum(),
        }
    }
    Some(go(&closed_ring(first), &parts))
}

/// `Polygon(a).intersection(Polygon(b)).area` for two simple polygons.
pub fn intersection_area(a: &[Pt], b: &[Pt]) -> f64 {
    intersection_area_of(a, &[b]).unwrap_or_else(|| multi_area(&to_geo(a).intersection(&to_geo(b))))
}

/// Largest family size for which unions are evaluated by inclusion-exclusion.
const MAX_INCLUSION_EXCLUSION: usize = 8;

/// `Polygon(target).intersection(unary_union([Polygon(p) for p in pieces])).area`.
pub fn area_inside_union(target: &[Pt], pieces: &[Vec<Pt>]) -> f64 {
    union_intersection_area(&[target.to_vec()], pieces)
}

/// `unary_union(a).intersection(unary_union(b)).area` for two families of simple polygons.
///
/// For small families the area is computed by inclusion-exclusion: the sum over non-empty subsets
/// S of `a` and T of `b` of `(-1)^(|S|+|T|) area(∩S ∩ ∩T)`, each term by convex clipping
/// ([`intersection_area_of`]). Larger families, or shapes that cannot be decomposed, go through
/// `geo` overlays.
pub fn union_intersection_area(a: &[Vec<Pt>], b: &[Vec<Pt>]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a.len() + b.len() <= MAX_INCLUSION_EXCLUSION {
        let mut total = 0.0;
        let mut ok = true;
        'outer: for smask in 1u32..(1 << a.len()) {
            for tmask in 1u32..(1 << b.len()) {
                let mut chosen: Vec<&[Pt]> = (0..a.len())
                    .filter(|i| smask & (1 << i) != 0)
                    .map(|i| a[i].as_slice())
                    .collect();
                chosen.extend(
                    (0..b.len())
                        .filter(|i| tmask & (1 << i) != 0)
                        .map(|i| b[i].as_slice()),
                );
                let Some(area) = intersection_area_of(chosen[0], &chosen[1..]) else {
                    ok = false;
                    break 'outer;
                };
                if (smask.count_ones() + tmask.count_ones()) % 2 == 0 {
                    total += area;
                } else {
                    total -= area;
                }
            }
        }
        if ok {
            return total;
        }
    }
    let ua = geo::unary_union(a.iter().map(|p| to_geo(p)).collect::<Vec<_>>().iter());
    let ub = geo::unary_union(b.iter().map(|p| to_geo(p)).collect::<Vec<_>>().iter());
    multi_area(&ua.intersection(&ub))
}

/// `Polygon(target).difference(unary_union(holes)).area`.
pub fn difference_area(target: &[Pt], holes: &[Vec<Pt>]) -> f64 {
    if holes.is_empty() {
        return polygon_area(target);
    }
    if holes.len() < MAX_INCLUSION_EXCLUSION {
        return polygon_area(target) - area_inside_union(target, holes);
    }
    let u = geo::unary_union(holes.iter().map(|p| to_geo(p)).collect::<Vec<_>>().iter());
    multi_area(&MultiPolygon::new(vec![to_geo(target)]).difference(&u))
}

// ---- lengths -----------------------------------------------------------------------------------

/// `Polygon(ring).intersection(LineString([p, q])).length`: the length of the part of segment
/// `p q` inside the closed polygon, boundary included.
pub fn segment_length_inside(ring: &[Pt], p: Pt, q: Pt) -> f64 {
    let o = closed_ring(ring);
    let d = (q.0 - p.0, q.1 - p.1);
    let len2 = d.0 * d.0 + d.1 * d.1;
    if len2 == 0.0 || o.len() < 2 {
        return 0.0;
    }
    let param = |x: Pt| ((x.0 - p.0) * d.0 + (x.1 - p.1) * d.1) / len2;
    // Cut points with their coordinates: GEOS measures the result line between its vertices
    // (the segment's ends and the intersection nodes), so the length must not depend on the
    // direction of `p q` (a fraction of the parameter range times the length does).
    let mut cuts: Vec<(f64, Pt)> = vec![(0.0, p), (1.0, q)];
    let mut on_edge: Vec<(f64, f64)> = Vec::new();
    for s in o.windows(2) {
        match segment_intersection(p, q, s[0], s[1]) {
            SegInter::None => {}
            SegInter::Point { at, .. } => cuts.push((param(at), at)),
            SegInter::Collinear { .. } => {
                let mut ts: Vec<f64> = Vec::new();
                for x in [s[0], s[1], p, q] {
                    if env_contains(p, q, x) && env_contains(s[0], s[1], x) {
                        cuts.push((param(x), x));
                        ts.push(param(x));
                    }
                }
                if let (Some(lo), Some(hi)) = (
                    ts.iter().copied().reduce(f64::min),
                    ts.iter().copied().reduce(f64::max),
                ) {
                    on_edge.push((lo, hi));
                }
            }
        }
    }
    cuts.retain(|(t, _)| (0.0..=1.0).contains(t));
    cuts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut inside = 0.0;
    for w in cuts.windows(2) {
        let ((t0, a), (t1, b)) = (w[0], w[1]);
        if t1 <= t0 {
            continue;
        }
        let t = (t0 + t1) / 2.0;
        let covered = on_edge.iter().any(|(lo, hi)| *lo <= t && t <= *hi) || {
            let m = (p.0 + t * d.0, p.1 + t * d.1);
            locate(m, &o) != Location::Exterior
        };
        if covered {
            inside += dist(a, b);
        }
    }
    inside
}

// ---- minimum rotated rectangle -----------------------------------------------------------------

/// Convex hull (counter-clockwise, no repeated or collinear points) of the distinct points.
fn convex_hull(points: &[Pt]) -> Vec<Pt> {
    let mut pts: Vec<Pt> = points.to_vec();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let mut lower: Vec<Pt> = Vec::new();
    for p in &pts {
        while lower.len() >= 2
            && orientation(lower[lower.len() - 2], lower[lower.len() - 1], *p) <= 0
        {
            lower.pop();
        }
        lower.push(*p);
    }
    let mut upper: Vec<Pt> = Vec::new();
    for p in pts.iter().rev() {
        while upper.len() >= 2
            && orientation(upper[upper.len() - 2], upper[upper.len() - 1], *p) <= 0
        {
            upper.pop();
        }
        upper.push(*p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// The shorter side of `Polygon(points).minimum_rotated_rectangle` (GEOS 3.14
/// `MinimumAreaRectangle`: the minimum-area enclosing rectangle, one side flush with a hull
/// edge). A shape with no area (all points collinear, one point, none) gives 0.0, where Shapely
/// returns a line or a point and `min_dimension` then fails.
pub fn min_rotated_rectangle_min_side(points: &[Pt]) -> f64 {
    let hull = convex_hull(points);
    if hull.len() < 3 {
        return 0.0;
    }
    let n = hull.len();
    let mut best: Option<(f64, f64)> = None; // (area, min side)
    for i in 0..n {
        let a = hull[i];
        let b = hull[(i + 1) % n];
        let (ux, uy) = (b.0 - a.0, b.1 - a.1);
        let len = (ux * ux + uy * uy).sqrt();
        if len == 0.0 {
            continue;
        }
        let (ux, uy) = (ux / len, uy / len);
        let (mut smin, mut smax, mut hmax) = (f64::INFINITY, f64::NEG_INFINITY, 0.0f64);
        for p in &hull {
            let (wx, wy) = (p.0 - a.0, p.1 - a.1);
            let s = wx * ux + wy * uy;
            let h = (ux * wy - uy * wx).abs();
            smin = smin.min(s);
            smax = smax.max(s);
            hmax = hmax.max(h);
        }
        let width = smax - smin;
        let area = width * hmax;
        if best.is_none_or(|(ba, _)| area < ba) {
            best = Some((area, width.min(hmax)));
        }
    }
    best.map_or(0.0, |(_, side)| side)
}

// ---- clipping an obstacle to the layout rectangle -----------------------------------------------

/// Liang-Barsky: the parameter interval of segment `p q` inside the box, if any.
fn box_interval(p: Pt, q: Pt, bx: &Bounds) -> Option<(f64, f64)> {
    let (dx, dy) = (q.0 - p.0, q.1 - p.1);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (pp, qq) in [
        (-dx, p.0 - bx.minx),
        (dx, bx.maxx - p.0),
        (-dy, p.1 - bx.miny),
        (dy, bx.maxy - p.1),
    ] {
        if pp == 0.0 {
            if qq < 0.0 {
                return None;
            }
        } else {
            let r = qq / pp;
            if pp < 0.0 {
                if r > t1 {
                    return None;
                }
                if r > t0 {
                    t0 = r;
                }
            } else {
                if r < t0 {
                    return None;
                }
                if r < t1 {
                    t1 = r;
                }
            }
        }
    }
    Some((t0, t1))
}

/// The first vertex of GEOS's overlay output ring for `ring ∩ box`. GEOS splits the input ring
/// (in its input order, from its first vertex) where it crosses the box boundary; the first
/// piece inside the box is the first result edge, and the output ring starts at that piece's
/// second point when the input is clockwise (the result orientation), or at its next-to-last
/// point when the input is counter-clockwise (the edge is used backwards). A ring that never
/// crosses the box is one closed piece; with no piece inside, the result is the box itself,
/// which starts at its lower-left corner. (Matched against Shapely on 2,651 clipped polygons.)
fn clip_start(ring: &[Pt], bx: &Bounds) -> Pt {
    let closed = closed_ring(ring);
    let ccw = signed_ring_area(&closed) > 0.0;
    let mut pieces: Vec<Vec<Pt>> = Vec::new();
    let mut cur: Option<Vec<Pt>> = None;
    let mut any_node = false;
    for w in closed.windows(2) {
        let (p, q) = (w[0], w[1]);
        let at = |t: f64| (p.0 + t * (q.0 - p.0), p.1 + t * (q.1 - p.1));
        match box_interval(p, q, bx) {
            Some((t0, t1)) if t1 > t0 => {
                if t0 > 0.0 {
                    any_node = true;
                    if let Some(c) = cur.take() {
                        pieces.push(c);
                    }
                    cur = Some(vec![at(t0)]);
                } else if cur.is_none() {
                    cur = Some(vec![p]);
                }
                if let Some(c) = cur.as_mut() {
                    if t1 < 1.0 {
                        any_node = true;
                        c.push(at(t1));
                        if let Some(c) = cur.take() {
                            pieces.push(c);
                        }
                    } else {
                        c.push(q);
                    }
                }
            }
            _ => {
                if let Some(c) = cur.take() {
                    pieces.push(c);
                }
            }
        }
    }
    if let Some(c) = cur.take() {
        pieces.push(c);
    }
    let first = if !any_node && !pieces.is_empty() {
        closed
    } else {
        match pieces.into_iter().next() {
            Some(p) => p,
            None => return (bx.minx, bx.miny),
        }
    };
    let k = if ccw {
        first.len().saturating_sub(2)
    } else {
        1.min(first.len().saturating_sub(1))
    };
    first[k]
}

/// Insert into each edge of `ring` (closed) the vertices of `source` that lie strictly inside it,
/// in order along the edge.
fn reinsert_vertices(ring: &[Pt], source: &[Pt]) -> Vec<Pt> {
    let mut out: Vec<Pt> = Vec::with_capacity(ring.len());
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        out.push(a);
        let mut on: Vec<(f64, Pt)> = source
            .iter()
            .filter(|v| {
                **v != a && **v != b && orientation(a, b, **v) == 0 && env_contains(a, b, **v)
            })
            .map(|v| ((v.0 - a.0).hypot(v.1 - a.1), *v))
            .collect();
        on.sort_by(|x, y| x.0.total_cmp(&y.0));
        on.dedup_by(|x, y| x.1 == y.1);
        out.extend(on.into_iter().map(|(_, v)| v));
    }
    if let Some(first) = ring.first() {
        out.push(*first);
    }
    out
}

/// `Polygon(ring).intersection(box)` when the result is a single polygon: its exterior ring
/// without the closing point, clockwise and starting where GEOS's overlay output starts
/// ([`clip_start`]). `None` when the intersection is empty, has no area or has more than one
/// part. Convex rings are clipped exactly (Sutherland-Hodgman); others go through `geo`.
pub fn clip_polygon_to_box(ring: &[Pt], bx: &Bounds) -> Option<Vec<Pt>> {
    let closed = closed_ring(ring);
    if closed.len() < 4 {
        return None;
    }
    // A ring inside the box comes back unchanged from clipping (GEOS returns its vertices);
    // `geo`'s overlay would snap them to its grid and add vertices.
    let inside_box = closed
        .iter()
        .all(|p| bx.minx <= p.0 && p.0 <= bx.maxx && bx.miny <= p.1 && p.1 <= bx.maxy);
    let pieces: Vec<Vec<Pt>> = if inside_box || is_convex(&closed) {
        let clipped = clip_convex(&closed, &bx.ring());
        if ring_area(&clipped) > 0.0 {
            vec![clipped]
        } else {
            vec![]
        }
    } else {
        to_geo(&closed)
            .intersection(&to_geo(&bx.ring()))
            .0
            .into_iter()
            .map(|p| {
                // `geo` drops collinear vertices; GEOS keeps every input vertex that lies on the
                // result boundary, so put the obstacle's back.
                let ring: Vec<Pt> = p.exterior().coords().map(|c| (c.x, c.y)).collect();
                reinsert_vertices(&ring, &closed)
            })
            .collect()
    };
    if pieces.len() != 1 {
        return None;
    }
    let mut out: Vec<Pt> = Vec::new();
    for p in &pieces[0] {
        if out.last() != Some(p) {
            out.push(*p);
        }
    }
    if out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    if out.len() < 3 {
        return None;
    }
    if signed_ring_area(&closed_ring(&out)) > 0.0 {
        out.reverse();
    }
    let s = clip_start(ring, bx);
    let start = out
        .iter()
        .enumerate()
        .map(|(i, p)| (i, (p.0 - s.0).hypot(p.1 - s.1)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(0, |(i, _)| i);
    out.rotate_left(start);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_validity_examples_match_geos() {
        // Expected strings are Shapely 2.2 / GEOS 3.14 output.
        assert_eq!(
            explain_validity(&[(0.0, 0.0), (2.0, 2.0), (2.0, 0.0), (0.0, 2.0)]),
            "Self-intersection[1 1]"
        );
        assert_eq!(
            explain_validity(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]),
            "Valid Geometry"
        );
        assert_eq!(
            explain_validity(&[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)]),
            "Self-intersection[1 0]"
        );
        assert_eq!(
            explain_validity(&[(0.0, 0.0), (1.0, 1.0), (0.0, 0.0)]),
            "Too few points in geometry component[0 0]"
        );
        assert_eq!(
            explain_validity(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (2.0, 0.0), (0.0, 4.0)]),
            "Ring Self-intersection[2 0]"
        );
        assert_eq!(
            explain_validity(&[(0.1, 0.1), (2.25, 2.25), (2.25, 0.1), (0.1, 2.25)]),
            "Self-intersection[1.175 1.175]"
        );
    }

    #[test]
    fn g15_formatting() {
        assert_eq!(format_g15(1.0 / 3.0), "0.333333333333333");
        assert_eq!(format_g15(1e20), "1e+20");
        assert_eq!(format_g15(1.5e-7), "1.5e-07");
        assert_eq!(format_g15(120.0), "120");
        assert_eq!(format_g15(-0.0), "-0");
    }

    #[test]
    fn areas_and_containment() {
        let sq = [(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)];
        assert_eq!(polygon_area(&sq), 16.0);
        let inner = [(1.0, 1.0), (4.0000005, 1.0), (4.0000005, 2.0), (1.0, 2.0)];
        assert!(contains_with_tolerance(&sq, &inner, 1e-6));
        let out = [(1.0, 1.0), (4.000002, 1.0), (4.000002, 2.0), (1.0, 2.0)];
        assert!(!contains_with_tolerance(&sq, &out, 1e-6));
        let other = [(3.0, 3.0), (7.0, 3.0), (7.0, 7.0), (3.0, 7.0)];
        assert_eq!(intersection_area(&sq, &other), 1.0);
        let door = segment_length_inside(&sq, (1.0, 0.0), (2.5, 0.0));
        assert!((door - 1.5).abs() < 1e-12);
        assert_eq!(
            min_rotated_rectangle_min_side(&[(0.0, 0.0), (3.0, 0.0), (3.0, 2.0), (0.0, 2.0)]),
            2.0
        );
    }
}
