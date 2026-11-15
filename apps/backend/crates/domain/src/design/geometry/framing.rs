//! Port of `apps/api/app/domains/design/geometry/framing.py`: fit the rectangular layout solver
//! to real (measured, non-rectangular) floors.
//!
//! 1. [`normalize_plan`] (used when ingesting Matterport geometry): rotate the plan so its dominant
//!    wall direction is axis-aligned and translate it to a positive local frame.
//! 2. [`frame_for_solver`]: find the largest axis-aligned rectangle inside the floor boundary,
//!    pick the measured entrance nearest to it, and rotate the rectangle (multiples of 90
//!    degrees) so that entrance lies on y=0. The solver runs in that frame; [`Frame::to_site`]
//!    maps results back onto the real plan.
//!
//! Assumptions are returned as notes and surfaced in the validation report: space outside the
//! rectangle is left unprogrammed, and existing interior partitions are treated as removable
//! (verify they are non-structural).

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::contracts::geometry::{Coord, Opening, SiteGeometry};
use crate::geometry::{self, Bounds, Location, RayStep};
use crate::py;
use crate::DomainError;

/// Python's `max(a, b)` for floats: `b` only when `b > a` (so `max(-0.0, 0.0)` is `-0.0`).
fn py_max(a: f64, b: f64) -> f64 {
    if b > a {
        b
    } else {
        a
    }
}

/// Python's `min(a, b)` for floats: `b` only when `b < a`.
fn py_min(a: f64, b: f64) -> f64 {
    if b < a {
        b
    } else {
        a
    }
}

/// Python's float `%` (the result takes the sign of the divisor).
fn py_mod(x: f64, m: f64) -> f64 {
    let r = x % m;
    if r != 0.0 {
        if (m < 0.0) != (r < 0.0) {
            r + m
        } else {
            r
        }
    } else {
        0.0f64.copysign(m)
    }
}

/// `dominant_angle(coords)`: the length-weighted wall direction in degrees, folded into
/// `[-45, 45)` and bucketed by `round()` (ties to even); the first bucket with the largest total
/// wins. Walls shorter than 0.3 m are ignored; no usable wall gives 0.0.
pub fn dominant_angle(coords: &[Coord]) -> f64 {
    let mut acc: IndexMap<i64, f64> = IndexMap::new();
    let n = coords.len();
    for i in 0..n {
        let (x0, y0) = coords[i];
        let (x1, y1) = coords[(i + 1) % n];
        let length = (x1 - x0).hypot(y1 - y0);
        if length < 0.3 {
            continue;
        }
        let mut ang = py_mod((y1 - y0).atan2(x1 - x0).to_degrees(), 90.0);
        if ang >= 45.0 {
            ang -= 90.0;
        }
        let key = ang.round_ties_even() as i64;
        *acc.entry(key).or_insert(0.0) += length;
    }
    let mut best: Option<(i64, f64)> = None;
    for (k, v) in &acc {
        if best.is_none_or(|(_, bv)| *v > bv) {
            best = Some((*k, *v));
        }
    }
    best.map_or(0.0, |(k, _)| k as f64)
}

fn point_of(v: &Value, what: &str) -> Result<Coord, DomainError> {
    let bad = || DomainError::BadRecord(format!("{what} is not an [x, y] pair: {v}"));
    let a = v.as_array().ok_or_else(bad)?;
    if a.len() != 2 {
        return Err(bad());
    }
    let x = a[0].as_f64().ok_or_else(bad)?;
    let y = a[1].as_f64().ok_or_else(bad)?;
    Ok((x, y))
}

fn list_of<'a>(doc: &'a Map<String, Value>, key: &str) -> Result<&'a [Value], DomainError> {
    match doc.get(key) {
        None => Ok(&[]),
        Some(Value::Array(a)) => Ok(a.as_slice()),
        Some(other) => Err(DomainError::BadRecord(format!(
            "{key} is not a list: {other}"
        ))),
    }
}

/// `normalize_plan(doc)`: rotate and translate every coordinate of a geometry document into an
/// axis-aligned positive frame, rounded to millimetres. As in Python, `floor_boundary`,
/// `fixed_walls`, `entrances`, `windows` and `interior_doors` are transformed (the last four are
/// always present in the output, empty when absent), `coordinate_reference` records the
/// rotation, and every other key (including `exits`, `fixed_obstacles` and
/// `fixture_locations`) is copied unchanged. A missing `floor_boundary`, or an opening without
/// `start`/`end`, is an error where Python raises `KeyError`.
pub fn normalize_plan(doc: &Value) -> Result<Value, DomainError> {
    let obj = doc
        .as_object()
        .ok_or_else(|| DomainError::BadRecord("geometry document is not an object".into()))?;
    let fb = obj
        .get("floor_boundary")
        .and_then(Value::as_array)
        .ok_or_else(|| DomainError::BadRecord("geometry document has no floor_boundary".into()))?;
    let boundary: Vec<Coord> = fb
        .iter()
        .map(|p| point_of(p, "floor_boundary point"))
        .collect::<Result<_, _>>()?;
    let angle = dominant_angle(&boundary);
    // Shapely `affinity.rotate(poly, -angle, origin=(0, 0))` for the bounds: radians as
    // `angle * pi / 180`, cosines and sines below 2.5e-16 snapped to zero.
    let theta = -angle * std::f64::consts::PI / 180.0;
    let mut cosp = theta.cos();
    let mut sinp = theta.sin();
    if cosp.abs() < 2.5e-16 {
        cosp = 0.0;
    }
    if sinp.abs() < 2.5e-16 {
        sinp = 0.0;
    }
    let rotated: Vec<Coord> = geometry::closed_ring(&boundary)
        .iter()
        .map(|(x, y)| (cosp * x + -sinp * y + 0.0, sinp * x + cosp * y + 0.0))
        .collect();
    let bounds = Bounds::of(&rotated)
        .ok_or_else(|| DomainError::BadRecord("floor_boundary is empty".into()))?;
    let (minx, miny) = (bounds.minx, bounds.miny);
    // `f(p)`: `math.radians(-angle)`, then round to 3 decimals.
    let r = (-angle).to_radians();
    let (c, s) = (r.cos(), r.sin());
    let f = |p: Coord| -> Value {
        let (x, y) = p;
        let xr = x * c - y * s;
        let yr = x * s + y * c;
        Value::from(vec![py::round(xr - minx, 3), py::round(yr - miny, 3)])
    };
    let mut out = obj.clone();
    out.insert(
        "floor_boundary".into(),
        Value::Array(boundary.iter().map(|p| f(*p)).collect()),
    );
    let mut walls = Vec::new();
    for w in list_of(obj, "fixed_walls")? {
        let pair = w.as_array().filter(|a| a.len() == 2).ok_or_else(|| {
            DomainError::BadRecord(format!("fixed_walls entry is not a pair: {w}"))
        })?;
        walls.push(Value::Array(vec![
            f(point_of(&pair[0], "fixed_walls point")?),
            f(point_of(&pair[1], "fixed_walls point")?),
        ]));
    }
    out.insert("fixed_walls".into(), Value::Array(walls));
    for key in ["entrances", "windows", "interior_doors"] {
        let mut items = Vec::new();
        for o in list_of(obj, key)? {
            let mut m = o
                .as_object()
                .cloned()
                .ok_or_else(|| DomainError::BadRecord(format!("{key} entry is not an object")))?;
            for end in ["start", "end"] {
                let p = m
                    .get(end)
                    .ok_or_else(|| DomainError::BadRecord(format!("{key} entry has no {end}")))?;
                let v = f(point_of(p, end)?);
                m.insert(end.into(), v);
            }
            items.push(Value::Object(m));
        }
        out.insert(key.into(), Value::Array(items));
    }
    out.insert(
        "coordinate_reference".into(),
        Value::String(format!(
            "local_floor_origin (rotated {:.0} deg from provider frame)",
            -angle
        )),
    );
    Ok(Value::Object(out))
}

/// Grid cell of [`largest_inner_rectangle`] (meters).
pub const INNER_RECTANGLE_CELL: f64 = 0.2;

/// Largest grid [`largest_inner_rectangle`] scans (cells).
pub const MAX_GRID_CELLS: usize = 1_000_000;

/// Work bound of [`largest_inner_rectangle`], in the work units of [`FrameWork::units`]: about
/// 0.8 s of the workspace's debug profile or 0.2 s of a release build on the development
/// machine, 20 % under the framing budget (1 s and 0.25 s). The scan's work is counted exactly
/// before any cell is tested; an outline whose count exceeds this gets no rectangle (and
/// [`frame_for_solver_checked`] says why). The weights below are the measured cost of each kind
/// of work (rust-domain-6): one unit is about a nanosecond of the debug profile (ss-domain at
/// opt-level 1) or a quarter of a nanosecond of a release build, whichever is slower for that
/// kind of work, taken from the most expensive outline family measured for it.
pub const MAX_FRAME_WORK: u64 = 800_000_000;

/// Work units per boundary segment: building, sorting and placing the segments (measured on
/// floors with up to a million vertices: 217 ns in release, 591 ns in the debug profile).
pub const FRAME_WORK_PER_SEGMENT: u64 = 900;
/// Work units per grid cell: every cell is visited and fed to the histogram (12 ns, 59 ns).
pub const FRAME_WORK_PER_CELL: u64 = 60;
/// Work units per segment per grid row it reaches: the per-row preparation (31 ns in release
/// plus [`FRAME_WORK_PER_SEGMENT_ROW_STEP`] per step).
pub const FRAME_WORK_PER_SEGMENT_ROW: u64 = 130;
/// Work units per step of [`FrameWork::segment_row_steps`]: the per-row sorts, binary searches
/// and Fenwick updates (3 ns in release, 12.5 ns in the debug profile).
pub const FRAME_WORK_PER_SEGMENT_ROW_STEP: u64 = 13;
/// Work units per cell near the boundary: each runs the exact tolerance test (290 ns, 982 ns).
pub const FRAME_WORK_PER_NEAR_CELL: u64 = 1200;
/// Work units per segment near a near cell: its crossings, tolerance bands and ray steps
/// (181 ns, 688 ns).
pub const FRAME_WORK_PER_NEAR_SEGMENT: u64 = 750;
/// Work units per pair of segments near the same cell (`k` squared for a cell with `k`
/// segments near it): the exact test locates up to one point per crossing against every near
/// segment. Measured on hairline slits narrower than the tolerance, the most expensive case
/// found (8.6 ns in release, 45 ns in the debug profile; a wall split into collinear pieces
/// costs 0.8 ns in release).
pub const FRAME_WORK_PER_NEAR_PAIR: u64 = 45;

/// Tolerance of the containment test (Python's `poly.buffer(1e-6)`).
const INNER_RECTANGLE_EPS: f64 = 1e-6;

/// The framing work bound was exceeded: no rectangle was computed. `note` says which bound
/// and is suitable for a validation issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkBoundExceeded {
    pub note: String,
}

/// The work of [`largest_inner_rectangle`]'s prepared scan of one outline, counted before the
/// scan (the scan does exactly this work). Each field is a count; [`FrameWork::units`] weighs
/// them by the measured cost of each kind of work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameWork {
    /// Boundary segments.
    pub segments: u64,
    /// Grid cells.
    pub cells: u64,
    /// Sum over grid rows of the segments that reach the row (its band widened by the pad).
    pub segment_rows: u64,
    /// Sum over grid rows of `a * (bits of a)`, `a` the segments that reach the row: the row's
    /// sorts, binary searches and Fenwick updates cost about `a log a`.
    pub segment_row_steps: u64,
    /// Cells that run the exact tolerance test: a segment is near, or the cell has no extent.
    pub near_cells: u64,
    /// Sum over near cells of the number of segments near the cell.
    pub near_segments: u64,
    /// Sum over near cells of the square of the number of segments near the cell.
    pub near_pairs: u64,
}

impl FrameWork {
    /// The weighted total, saturating.
    pub fn units(&self) -> u64 {
        [
            (self.segments, FRAME_WORK_PER_SEGMENT),
            (self.cells, FRAME_WORK_PER_CELL),
            (self.segment_rows, FRAME_WORK_PER_SEGMENT_ROW),
            (self.segment_row_steps, FRAME_WORK_PER_SEGMENT_ROW_STEP),
            (self.near_cells, FRAME_WORK_PER_NEAR_CELL),
            (self.near_segments, FRAME_WORK_PER_NEAR_SEGMENT),
            (self.near_pairs, FRAME_WORK_PER_NEAR_PAIR),
        ]
        .iter()
        .fold(0u64, |acc, &(n, w)| acc.saturating_add(n.saturating_mul(w)))
    }
}

/// An axis-aligned rectangle `(x0, y0, x1, y1)`.
pub type Rect = (f64, f64, f64, f64);

/// `largest_inner_rectangle(poly, cell)`: the largest axis-aligned rectangle on a `cell` grid
/// (anchored at the polygon's lower-left bound) whose cells all lie inside
/// `poly.buffer(1e-6)`, by the histogram method. `(x0, y0, x1, y1)`, or `None` when the grid has
/// fewer than 2 cells in either direction, no cell fits, or a work bound is exceeded
/// ([`MAX_GRID_CELLS`], [`MAX_FRAME_WORK`]; deliberate bounds, Python has none).
/// [`largest_inner_rectangle_checked`] tells the two `None` cases apart.
pub fn largest_inner_rectangle(poly: &[Coord], cell: f64) -> Option<Rect> {
    largest_inner_rectangle_checked(poly, cell).ok().flatten()
}

/// [`largest_inner_rectangle`], with `Err` when a work bound stopped the scan.
///
/// Each cell's inside test is exactly `contains_with_tolerance(poly, cell, 1e-6)` (the
/// reference, [`largest_inner_rectangle_reference`], calls it for every cell against the whole
/// boundary). Here the boundary is prepared per grid row instead:
///
/// - the row's segments are those whose y-range meets the row band widened by a pad (the
///   tolerance plus 1 mm plus a relative term for large coordinates, far above any rounding in
///   the test); for each, the x-range of its part inside the widened band, widened by the pad;
/// - a cell whose x-range meets no such x-range has no boundary within the pad, so the whole
///   cell is on one side of the boundary and every point the exact test would locate gets the
///   same answer: it is decided by the ray-crossing parity at the row centre of the segments to
///   its right (each of those is crossed exactly when it straddles that height), kept up to date
///   as the scan moves right;
/// - a cell near the boundary runs the exact test ([`geometry::covers_ring_near`]) on the
///   segments near it, with point location split the same way: the exact ray steps of the near
///   segments plus the straddle parity, at the point's height, of the segments to the right
///   (a Fenwick tree over the row's segment end heights).
///
/// The cost is about cells plus rows times vertices plus the near-cell tests, instead of cells
/// times vertices. A non-finite vertex gives `Ok(None)` (contracts parsed from JSON cannot carry
/// one).
///
/// **Work bound.** The scan's work ([`FrameWork`]) is counted exactly before any cell is tested,
/// in two steps, each of which refuses (`Err`) as soon as the weighted count exceeds
/// [`MAX_FRAME_WORK`]: first the segments, the cells and the rows each segment reaches (two
/// binary searches per segment), then the cells near the boundary and the segments near each
/// (the per-row preparation of the scan without the cell tests). A refused outline costs at most
/// that counting; an accepted one does exactly the work it was counted for.
pub fn largest_inner_rectangle_checked(
    poly: &[Coord],
    cell: f64,
) -> Result<Option<Rect>, WorkBoundExceeded> {
    let Some(grid) = Grid::new(poly, cell)? else {
        return Ok(None);
    };
    let mut segs = grid.segments();
    let mut work = FrameWork {
        segments: segs.len() as u64,
        cells: (grid.nx as u64).saturating_mul(grid.ny as u64),
        ..FrameWork::default()
    };
    grid.count_segment_rows(&segs, &mut work);
    if work.units() > MAX_FRAME_WORK {
        return Err(WorkBoundExceeded {
            note: format!(
                "Floor outline too detailed to frame: {} boundary segments on a {} x {} cell grid, which reach a grid row {} times in all (estimated framing work {} units, above the bound of {MAX_FRAME_WORK}); no layout was generated. Simplify the measured outline.",
                work.segments,
                grid.nx,
                grid.ny,
                work.segment_rows,
                work.units()
            ),
        });
    }
    segs.sort_by(|s, t| s.ymin.total_cmp(&t.ymin));
    if !grid.count_near_work(&segs, &mut work, MAX_FRAME_WORK) {
        return Err(WorkBoundExceeded {
            note: format!(
                "Floor outline too detailed to frame: boundary segments are packed too densely near the grid (estimated framing work above the bound of {MAX_FRAME_WORK} units); no layout was generated. Simplify the measured outline."
            ),
        });
    }
    Ok(grid.scan(&segs, &mut FrameWork::default()))
}

/// The work [`largest_inner_rectangle_checked`] counts for an outline before its scan, counted
/// in full (no work bound; only [`MAX_GRID_CELLS`] applies). `None` when there is no grid to
/// scan. For the tests and timing drivers; not for request paths.
#[doc(hidden)]
pub fn frame_work_estimate(poly: &[Coord], cell: f64) -> Option<FrameWork> {
    let grid = Grid::new(poly, cell).ok().flatten()?;
    let mut segs = grid.segments();
    let mut work = FrameWork {
        segments: segs.len() as u64,
        cells: (grid.nx as u64).saturating_mul(grid.ny as u64),
        ..FrameWork::default()
    };
    grid.count_segment_rows(&segs, &mut work);
    segs.sort_by(|s, t| s.ymin.total_cmp(&t.ymin));
    grid.count_near_work(&segs, &mut work, u64::MAX);
    Some(work)
}

/// The scan of [`largest_inner_rectangle_checked`] without its work bound (only
/// [`MAX_GRID_CELLS`] applies), with the work the scan itself did, counted as it went. `None`
/// when there is no grid to scan. For the tests that check [`frame_work_estimate`] against the
/// scan; not for request paths.
#[doc(hidden)]
pub fn largest_inner_rectangle_counted(
    poly: &[Coord],
    cell: f64,
) -> Option<(Option<Rect>, FrameWork)> {
    let grid = Grid::new(poly, cell).ok().flatten()?;
    let mut segs = grid.segments();
    segs.sort_by(|s, t| s.ymin.total_cmp(&t.ymin));
    let mut work = FrameWork {
        segments: segs.len() as u64,
        cells: (grid.nx as u64).saturating_mul(grid.ny as u64),
        ..FrameWork::default()
    };
    let rect = grid.scan(&segs, &mut work);
    Some((rect, work))
}

/// The grid of [`largest_inner_rectangle_checked`] and the closed boundary ring.
struct Grid {
    b: Bounds,
    cell: f64,
    nx: usize,
    ny: usize,
    ring: Vec<Coord>,
    pad: f64,
    /// Column edges `cell_x(0..=nx)`.
    xs: Vec<f64>,
}

impl Grid {
    /// The grid, `Ok(None)` when there is nothing to scan (fewer than 2 cells either way, fewer
    /// than 4 ring points, a non-finite vertex), `Err` beyond [`MAX_GRID_CELLS`].
    fn new(poly: &[Coord], cell: f64) -> Result<Option<Grid>, WorkBoundExceeded> {
        let Some(b) = Bounds::of(poly) else {
            return Ok(None);
        };
        let nx_f = ((b.maxx - b.minx) / cell).trunc();
        let ny_f = ((b.maxy - b.miny) / cell).trunc();
        if !(nx_f >= 2.0 && ny_f >= 2.0) || !nx_f.is_finite() || !ny_f.is_finite() {
            return Ok(None);
        }
        // Bounded work: Python would scan any grid; floors with more cells than this (about
        // 200 m x 200 m at 0.2 m) get no rectangle.
        if nx_f * ny_f > MAX_GRID_CELLS as f64 {
            return Err(WorkBoundExceeded {
                note: format!(
                    "Floor too large to frame: a {nx_f:.0} x {ny_f:.0} cell grid exceeds the bound of {MAX_GRID_CELLS} cells; no layout was generated."
                ),
            });
        }
        let (nx, ny) = (nx_f as usize, ny_f as usize);
        let ring = geometry::closed_ring(poly);
        // `contains_with_tolerance` is false for every cell when the ring has fewer than 4 points.
        if ring.len() < 4 || ring.iter().any(|p| !p.0.is_finite() || !p.1.is_finite()) {
            return Ok(None);
        }
        let max_abs = ring
            .iter()
            .fold(0.0f64, |m, p| m.max(p.0.abs()).max(p.1.abs()));
        let pad = INNER_RECTANGLE_EPS + 1e-3 + 1e-9 * max_abs;
        let mut grid = Grid {
            b,
            cell,
            nx,
            ny,
            ring,
            pad,
            xs: Vec::new(),
        };
        grid.xs = (0..=nx).map(|i| grid.cell_x(i)).collect();
        Ok(Some(grid))
    }

    fn cell_x(&self, i: usize) -> f64 {
        self.b.minx + i as f64 * self.cell
    }

    fn cell_y(&self, j: usize) -> f64 {
        self.b.miny + j as f64 * self.cell
    }

    /// `xs[1..].partition_point(|&x| x < xlo)`: the first cell whose right edge reaches `xlo`.
    /// The column edges are non-decreasing and, like `xlo`, finite, so the partition point is the
    /// one `r` with `xs[r] < xlo` (or `r == 0`) and `xs[r + 1] >= xlo` (or `r == nx`); the grid's
    /// arithmetic gives it directly except in rounding corner cases, which fall back to the
    /// binary search.
    fn first_near_cell(&self, xlo: f64) -> usize {
        let (xs, nx) = (&self.xs, self.nx);
        // `as` saturates (NaN and negatives give 0).
        let r = ((((xlo - self.b.minx) / self.cell).ceil() - 1.0) as usize).min(nx);
        if (r == 0 || xs[r] < xlo) && (r == nx || xs[r + 1] >= xlo) {
            r
        } else {
            xs[1..].partition_point(|&x| x < xlo)
        }
    }

    /// `xs[..nx].partition_point(|&x| x <= xhi)`: one past the last cell whose left edge is at
    /// most `xhi`. As [`Grid::first_near_cell`]: the one `r` with `xs[r - 1] <= xhi` (or
    /// `r == 0`) and `xs[r] > xhi` (or `r == nx`), with the binary search as the fallback.
    fn near_cells_end(&self, xhi: f64) -> usize {
        let (xs, nx) = (&self.xs, self.nx);
        let r = ((((xhi - self.b.minx) / self.cell).floor() + 1.0) as usize).min(nx);
        if (r == 0 || xs[r - 1] <= xhi) && (r == nx || xs[r] > xhi) {
            r
        } else {
            xs[..nx].partition_point(|&x| x <= xhi)
        }
    }

    /// Row `j`'s band, widened by the pad: `(lo, hi)`.
    fn band(&self, j: usize) -> (f64, f64) {
        (self.cell_y(j) - self.pad, self.cell_y(j + 1) + self.pad)
    }

    /// The ring's segments, in ring order.
    fn segments(&self) -> Vec<Seg> {
        self.ring
            .windows(2)
            .map(|w| Seg {
                a: w[0],
                b: w[1],
                ymin: w[0].1.min(w[1].1),
                ymax: w[0].1.max(w[1].1),
            })
            .collect()
    }

    /// Set [`FrameWork::segment_rows`] and [`FrameWork::segment_row_steps`]: the segments the
    /// scan's sweep visits in each row. The sweep keeps a segment in row `j` exactly when
    /// `ymin <= hi(j)` and `ymax >= lo(j)` for that row's band `(lo, hi)`; both band ends are
    /// non-decreasing in `j` (a positive cell, a constant pad), so each segment's rows are one
    /// range, found by two binary searches, and a difference array over the rows gives each
    /// row's count.
    fn count_segment_rows(&self, segs: &[Seg], work: &mut FrameWork) {
        let (los, his): (Vec<f64>, Vec<f64>) = (0..self.ny).map(|j| self.band(j)).unzip();
        let mut diff = vec![0i64; self.ny + 1];
        for s in segs {
            let first = his.partition_point(|&hi| hi < s.ymin);
            let end = los.partition_point(|&lo| lo <= s.ymax);
            if first < end {
                diff[first] += 1;
                diff[end] -= 1;
            }
        }
        let mut a = 0i64;
        for d in &diff[..self.ny] {
            a += d;
            work.add_row(a.unsigned_abs());
        }
    }

    /// Add the near-cell counts of [`FrameWork`] to `work`, row by row, with the scan's own row
    /// preparation ([`RowSweep`]) and a difference array over the row's cells instead of the
    /// cell tests. Stops, returning `false`, as soon as `work.units()` exceeds `limit`.
    fn count_near_work(&self, segs: &[Seg], work: &mut FrameWork, limit: u64) -> bool {
        let nx = self.nx;
        let degenerate_cols = self.xs.windows(2).any(|w| w[1] == w[0]);
        let mut sweep = RowSweep::new(segs);
        let mut row: Vec<RowSeg> = Vec::new();
        // diff[i]: segments whose near range starts at cell i minus those whose range ends there.
        let mut diff = vec![0i64; nx + 1];
        for j in 0..self.ny {
            sweep.advance(self, j, &mut row);
            let degenerate_row = self.cell_y(j + 1) == self.cell_y(j);
            let (mut from, mut to) = (nx, 0usize);
            for r in &row {
                if r.i_lo < r.i_hi_end {
                    diff[r.i_lo] += 1;
                    diff[r.i_hi_end] -= 1;
                    from = from.min(r.i_lo);
                    to = to.max(r.i_hi_end);
                }
            }
            // Cells with no extent run the exact test even with no segment near them.
            if degenerate_row || degenerate_cols {
                (from, to) = (0, nx);
            }
            let mut k = 0i64;
            for i in from..to {
                k += diff[i];
                diff[i] = 0;
                if k > 0 || degenerate_row || self.xs[i + 1] == self.xs[i] {
                    work.add_near_cell(k.unsigned_abs());
                }
            }
            if to <= nx {
                diff[to] = 0;
            }
            if work.units() > limit {
                return false;
            }
        }
        true
    }

    /// The prepared scan: the largest rectangle, with the work done added to `work` (segment
    /// rows and near cells).
    fn scan(&self, segs: &[Seg], work: &mut FrameWork) -> Option<Rect> {
        let (nx, ny) = (self.nx, self.ny);
        let eps = INNER_RECTANGLE_EPS;
        let xs = &self.xs;
        let mut sweep = RowSweep::new(segs);
        let mut best = 0usize;
        let mut best_rect: Option<(usize, usize, usize, usize)> = None;
        let mut heights = vec![0usize; nx];
        let mut row: Vec<RowSeg> = Vec::new();
        let mut by_lo: Vec<usize> = Vec::new();
        let mut by_xlo: Vec<usize> = Vec::new();
        let mut levels: Vec<f64> = Vec::new();
        let mut near: Vec<usize> = Vec::new();
        let mut near_segs: Vec<(Coord, Coord)> = Vec::new();
        for j in 0..ny {
            let (y0, y1) = (self.cell_y(j), self.cell_y(j + 1));
            sweep.advance(self, j, &mut row);
            work.add_row(row.len() as u64);
            // Straddle parity of the segments right of the current cell: a Fenwick tree (XOR) over
            // the row's distinct end heights; a segment straddles height y (GEOS's half-open rule,
            // ymin <= y < ymax) exactly when an odd number of its two end heights are <= y.
            levels.clear();
            for r in &row {
                levels.push(r.seg.ymin);
                levels.push(r.seg.ymax);
            }
            levels.sort_by(f64::total_cmp);
            levels.dedup();
            let mut fenwick = vec![false; levels.len() + 1];
            let rank = |levels: &[f64], y: f64| levels.partition_point(|&v| v < y);
            let y_ref = (y0 + y1) / 2.0;
            let mut parity_ref = false;
            for r in &row {
                fenwick_toggle(&mut fenwick, rank(&levels, r.seg.ymin));
                fenwick_toggle(&mut fenwick, rank(&levels, r.seg.ymax));
                parity_ref ^= r.seg.straddles(y_ref);
            }
            by_xlo.clear();
            by_xlo.extend(0..row.len());
            by_xlo.sort_by(|&a, &c| row[a].xlo.total_cmp(&row[c].xlo));
            by_lo.clear();
            by_lo.extend((0..row.len()).filter(|&k| row[k].i_lo < row[k].i_hi_end));
            by_lo.sort_by_key(|&k| row[k].i_lo);
            let (mut q, mut p) = (0usize, 0usize);
            near.clear();
            for (i, h) in heights.iter_mut().enumerate() {
                let (cx0, cx1) = (xs[i], xs[i + 1]);
                // Segments that start (in x) before this cell's right edge are no longer to its right.
                while q < by_xlo.len() && row[by_xlo[q]].xlo <= cx1 {
                    let s = row[by_xlo[q]].seg;
                    fenwick_toggle(&mut fenwick, rank(&levels, s.ymin));
                    fenwick_toggle(&mut fenwick, rank(&levels, s.ymax));
                    parity_ref ^= s.straddles(y_ref);
                    q += 1;
                }
                while p < by_lo.len() && row[by_lo[p]].i_lo <= i {
                    near.push(by_lo[p]);
                    p += 1;
                }
                near.retain(|&k| i < row[k].i_hi_end);
                let cell_ring = [(cx0, y0), (cx1, y0), (cx1, y1), (cx0, y1)];
                let degenerate = cx1 == cx0 || y1 == y0;
                let inside = if near.is_empty() && !degenerate {
                    parity_ref
                        && geometry::ring_area(&[
                            (cx0, y0),
                            (cx1, y0),
                            (cx1, y1),
                            (cx0, y1),
                            (cx0, y0),
                        ]) > 0.0
                } else {
                    work.add_near_cell(near.len() as u64);
                    near_segs.clear();
                    near_segs.extend(near.iter().map(|&k| (row[k].seg.a, row[k].seg.b)));
                    let mut locate = |m: Coord| -> Location {
                        let mut odd =
                            fenwick_prefix(&fenwick, levels.partition_point(|&v| v <= m.1));
                        for &(a, c) in &near_segs {
                            match geometry::ray_step(m, a, c) {
                                RayStep::Boundary => return Location::Boundary,
                                RayStep::Cross => odd = !odd,
                                RayStep::Skip => {}
                            }
                        }
                        if odd {
                            Location::Interior
                        } else {
                            Location::Exterior
                        }
                    };
                    geometry::covers_ring_near(&cell_ring, &near_segs, &mut locate, eps)
                };
                *h = if inside { *h + 1 } else { 0 };
            }
            histogram_step(&heights, j, &mut best, &mut best_rect);
        }
        let (i0, j0, i1, j1) = best_rect?;
        Some((
            self.cell_x(i0),
            self.cell_y(j0),
            self.cell_x(i1),
            self.cell_y(j1),
        ))
    }
}

impl FrameWork {
    /// Count one grid row that `a` segments reach.
    fn add_row(&mut self, a: u64) {
        self.segment_rows = self.segment_rows.saturating_add(a);
        let bits = u64::from(u64::BITS - a.leading_zeros());
        self.segment_row_steps = self
            .segment_row_steps
            .saturating_add(a.saturating_mul(bits));
    }

    /// Count one near cell with `k` segments near it.
    fn add_near_cell(&mut self, k: u64) {
        self.near_cells = self.near_cells.saturating_add(1);
        self.near_segments = self.near_segments.saturating_add(k);
        self.near_pairs = self.near_pairs.saturating_add(k.saturating_mul(k));
    }
}

/// The segments of each grid row in turn: the active list of a sweep, row by row from the
/// bottom, over the segments sorted by `ymin`.
struct RowSweep<'a> {
    segs: &'a [Seg],
    next: usize,
    active: Vec<Seg>,
}

impl<'a> RowSweep<'a> {
    fn new(segs: &'a [Seg]) -> Self {
        RowSweep {
            segs,
            next: 0,
            active: Vec::new(),
        }
    }

    /// Fill `row` with the segments of grid row `j` (called for `j = 0, 1, ...` in turn): those
    /// whose y-range meets the row's padded band, each with the padded x-range of its part
    /// inside the band and the cells that x-range meets.
    fn advance(&mut self, grid: &Grid, j: usize, row: &mut Vec<RowSeg>) {
        let (band_lo, band_hi) = grid.band(j);
        let pad = grid.pad;
        while self.next < self.segs.len() && self.segs[self.next].ymin <= band_hi {
            self.active.push(self.segs[self.next]);
            self.next += 1;
        }
        self.active.retain(|s| s.ymax >= band_lo);
        row.clear();
        for s in &self.active {
            let (xlo, xhi) = x_range_in_band(s, band_lo, band_hi);
            let (xlo, xhi) = (xlo - pad, xhi + pad);
            // Cells i with xs[i + 1] >= xlo and xs[i] <= xhi are near (an empty range when
            // i_lo > i_hi); cells left of i_lo have this segment on their right.
            let i_lo = grid.first_near_cell(xlo);
            let i_hi_end = grid.near_cells_end(xhi);
            row.push(RowSeg {
                seg: *s,
                xlo,
                i_lo,
                i_hi_end,
            });
        }
    }
}

/// A boundary segment and its y-range.
#[derive(Debug, Clone, Copy)]
struct Seg {
    a: Coord,
    b: Coord,
    ymin: f64,
    ymax: f64,
}

impl Seg {
    /// GEOS's straddle rule for a horizontal ray at height `y` (`ymin <= y < ymax`).
    fn straddles(&self, y: f64) -> bool {
        self.ymin <= y && y < self.ymax
    }
}

/// A segment of one grid row: its padded x-start, and the cells it is near
/// (`i_lo..i_hi_end`).
#[derive(Debug, Clone, Copy)]
struct RowSeg {
    seg: Seg,
    xlo: f64,
    i_lo: usize,
    i_hi_end: usize,
}

/// x-range of the part of `s` with `y` in `[lo, hi]` (the caller has checked that `s` reaches
/// the band). Rounding here is far below the pad the caller adds.
fn x_range_in_band(s: &Seg, lo: f64, hi: f64) -> (f64, f64) {
    let (a, b) = (s.a, s.b);
    let dy = b.1 - a.1;
    let (t0, t1) = if dy == 0.0 {
        (0.0, 1.0)
    } else {
        let ta = ((lo - a.1) / dy).clamp(0.0, 1.0);
        let tb = ((hi - a.1) / dy).clamp(0.0, 1.0);
        (ta.min(tb), ta.max(tb))
    };
    let at = |t: f64| {
        if t <= 0.0 {
            a.0
        } else if t >= 1.0 {
            b.0
        } else {
            a.0 + t * (b.0 - a.0)
        }
    };
    let (x0, x1) = (at(t0), at(t1));
    (x0.min(x1), x0.max(x1))
}

/// Toggle the parity of position `k` (0-based) of an XOR Fenwick tree.
fn fenwick_toggle(tree: &mut [bool], k: usize) {
    let mut i = k + 1;
    while i < tree.len() {
        tree[i] = !tree[i];
        i += i.isolate_lowest_one();
    }
}

/// XOR of the first `n` positions of an XOR Fenwick tree.
fn fenwick_prefix(tree: &[bool], n: usize) -> bool {
    let mut acc = false;
    let mut i = n.min(tree.len().saturating_sub(1));
    while i > 0 {
        acc ^= tree[i];
        i -= i.isolate_lowest_one();
    }
    acc
}

/// One row of the histogram method: the largest rectangle under `heights` ending at row `j`.
fn histogram_step(
    heights: &[usize],
    j: usize,
    best: &mut usize,
    best_rect: &mut Option<(usize, usize, usize, usize)>,
) {
    let nx = heights.len();
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..=nx {
        let h = if i < nx { heights[i] } else { 0 };
        while let Some(&top) = stack.last() {
            if heights[top] < h {
                break;
            }
            stack.pop();
            let left = stack.last().map_or(0, |s| s + 1);
            let area = heights[top] * (i - left);
            if area > *best {
                *best = area;
                *best_rect = Some((left, j + 1 - heights[top], i, j + 1));
            }
        }
        stack.push(i);
    }
}

/// The pre-F3 implementation, kept verbatim as the reference for the differential tests: every
/// cell is tested with [`geometry::contains_with_tolerance`] against the whole boundary (cells
/// times vertices). Bounded by [`MAX_GRID_CELLS`] only. Not for request paths.
#[doc(hidden)]
pub fn largest_inner_rectangle_reference(poly: &[Coord], cell: f64) -> Option<Rect> {
    let b = Bounds::of(poly)?;
    let nx_f = ((b.maxx - b.minx) / cell).trunc();
    let ny_f = ((b.maxy - b.miny) / cell).trunc();
    if !(nx_f >= 2.0 && ny_f >= 2.0) || !nx_f.is_finite() || !ny_f.is_finite() {
        return None;
    }
    if nx_f * ny_f > MAX_GRID_CELLS as f64 {
        return None;
    }
    let (nx, ny) = (nx_f as usize, ny_f as usize);
    let ring = geometry::closed_ring(poly);
    let cell_x = |i: usize| b.minx + i as f64 * cell;
    let cell_y = |j: usize| b.miny + j as f64 * cell;
    let mut best = 0usize;
    let mut best_rect: Option<(usize, usize, usize, usize)> = None;
    let mut heights = vec![0usize; nx];
    for j in 0..ny {
        for (i, h) in heights.iter_mut().enumerate() {
            let cell_ring = [
                (cell_x(i), cell_y(j)),
                (cell_x(i + 1), cell_y(j)),
                (cell_x(i + 1), cell_y(j + 1)),
                (cell_x(i), cell_y(j + 1)),
            ];
            let inside = geometry::contains_with_tolerance(&ring, &cell_ring, 1e-6);
            *h = if inside { *h + 1 } else { 0 };
        }
        histogram_step(&heights, j, &mut best, &mut best_rect);
    }
    let (i0, j0, i1, j1) = best_rect?;
    Some((cell_x(i0), cell_y(j0), cell_x(i1), cell_y(j1)))
}

/// Which side of the layout rectangle becomes the frame's entrance wall (y = 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bottom,
    Top,
    Left,
    Right,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Bottom => "bottom",
            Side::Top => "top",
            Side::Left => "left",
            Side::Right => "right",
        }
    }
}

/// The map between the measured site and the solver frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameMap {
    /// The site is already solver-ready (Python's `lambda p: p`).
    Identity,
    /// The layout rectangle `(x0, y0)`-`(x0 + w, y0 + h)` turned so `side` lies on y = 0.
    Rect {
        side: Side,
        x0: f64,
        y0: f64,
        w: f64,
        h: f64,
    },
}

/// `Frame`: the solver-frame site, the map back to the measured site, and the notes.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub site: SiteGeometry,
    pub map: FrameMap,
    pub notes: Vec<String>,
}

impl Frame {
    /// Python's `frame.to_site`: frame -> site, rounded to 3 decimals (identity for a
    /// solver-ready site, unrounded).
    pub fn to_site(&self, p: Coord) -> Coord {
        match self.map {
            FrameMap::Identity => p,
            FrameMap::Rect { side, x0, y0, w, h } => {
                let (u, v) = p;
                let (x, y) = match side {
                    Side::Bottom => (u, v),
                    Side::Top => (w - u, h - v),
                    Side::Left => (v, h - u),
                    Side::Right => (w - v, u),
                };
                (py::round(x + x0, 3), py::round(y + y0, 3))
            }
        }
    }

    /// Python's `fwd`: site -> frame (unrounded).
    pub fn to_frame(&self, p: Coord) -> Coord {
        match self.map {
            FrameMap::Identity => p,
            FrameMap::Rect { side, x0, y0, w, h } => fwd(side, x0, y0, w, h, p),
        }
    }

    /// True when the solver runs on the measured site itself.
    pub fn is_identity(&self) -> bool {
        self.map == FrameMap::Identity
    }
}

fn fwd(side: Side, x0: f64, y0: f64, w: f64, h: f64, p: Coord) -> Coord {
    let (x, y) = (p.0 - x0, p.1 - y0);
    match side {
        Side::Bottom => (x, y),
        Side::Top => (w - x, h - y),
        Side::Left => (h - y, x),
        Side::Right => (y, w - x),
    }
}

/// `_is_solver_ready(site)`: an axis-aligned rectangle with every entrance on its y-min wall.
pub fn is_solver_ready(site: &SiteGeometry) -> bool {
    let Some(b) = Bounds::of(&site.floor_boundary) else {
        return false;
    };
    if (geometry::polygon_area(&site.floor_boundary) - b.area()).abs() > 1e-6 {
        return false;
    }
    let y0 = b.miny;
    site.entrances
        .iter()
        .all(|o| (o.start.1 - y0).abs() < 1e-6 && (o.end.1 - y0).abs() < 1e-6)
}

/// Minimum layout rectangle area of [`frame_for_solver`] (m2).
pub const MIN_FRAME_AREA: f64 = 20.0;

/// `frame_for_solver(site, min_area)`. `None` when the floor has no usable rectangle of at least
/// `min_area` square meters, or when a framing work bound stopped the scan
/// ([`frame_for_solver_checked`] tells the two apart).
pub fn frame_for_solver(site: &SiteGeometry, min_area: f64) -> Option<Frame> {
    frame_for_solver_checked(site, min_area).ok().flatten()
}

/// [`frame_for_solver`], with `Err` (and a note saying which bound) when a work bound of
/// [`largest_inner_rectangle_checked`] stopped the scan: such a floor gets no frame rather than
/// a long computation. `Ok(None)` is Python's `None` (no usable rectangle).
pub fn frame_for_solver_checked(
    site: &SiteGeometry,
    min_area: f64,
) -> Result<Option<Frame>, WorkBoundExceeded> {
    frame_with(site, min_area, largest_inner_rectangle_checked)
}

/// [`frame_for_solver`] on [`largest_inner_rectangle_reference`] (the pre-F3 scan), for the
/// differential tests. Not for request paths.
#[doc(hidden)]
pub fn frame_for_solver_reference(site: &SiteGeometry, min_area: f64) -> Option<Frame> {
    frame_with(site, min_area, |poly, cell| {
        Ok(largest_inner_rectangle_reference(poly, cell))
    })
    .ok()
    .flatten()
}

fn frame_with(
    site: &SiteGeometry,
    min_area: f64,
    inner_rectangle: impl Fn(&[Coord], f64) -> Result<Option<Rect>, WorkBoundExceeded>,
) -> Result<Option<Frame>, WorkBoundExceeded> {
    if is_solver_ready(site) {
        return Ok(Some(Frame {
            site: site.clone(),
            map: FrameMap::Identity,
            notes: vec![],
        }));
    }
    let Some((x0, y0, x1, y1)) = inner_rectangle(&site.floor_boundary, INNER_RECTANGLE_CELL)?
    else {
        return Ok(None);
    };
    let (w, h) = (x1 - x0, y1 - y0);
    if w * h < min_area {
        return Ok(None);
    }
    let mut notes = vec![format!(
        "Layout uses the largest rectangular area of the measured floor ({w:.1} x {h:.1} m = {:.0} m2 of {:.0} m2); the rest is left unprogrammed.",
        w * h,
        geometry::polygon_area(&site.floor_boundary)
    )];
    if !site.fixed_walls.is_empty() {
        notes.push(format!(
            "{} existing interior partition(s) assumed removable; verify they are non-structural.",
            site.fixed_walls.len()
        ));
    }
    let rbox = Bounds {
        minx: x0,
        miny: y0,
        maxx: x1,
        maxy: y1,
    };
    let sides = [
        (Side::Bottom, (x0, y0), (x1, y0)),
        (Side::Top, (x0, y1), (x1, y1)),
        (Side::Left, (x0, y0), (x0, y1)),
        (Side::Right, (x1, y0), (x1, y1)),
    ];
    let mut best: Option<(f64, Side, &Opening)> = None;
    for o in &site.entrances {
        let mid = ((o.start.0 + o.end.0) / 2.0, (o.start.1 + o.end.1) / 2.0);
        for (name, a, b) in sides {
            let d = geometry::point_segment_distance(mid, a, b);
            if best.is_none_or(|(bd, _, _)| d < bd) {
                best = Some((d, name, o));
            }
        }
    }
    let side = best.map_or(Side::Bottom, |(_, s, _)| s);
    match best {
        None => notes.push(
            "No measured entrance on the outer wall; entrance assumed at the centre of one side."
                .into(),
        ),
        Some((d, _, _)) if d > 1.0 => notes.push(format!(
            "Nearest measured entrance is {d:.1} m from the layout area; a corridor connection is assumed."
        )),
        Some(_) => {}
    }
    let (fw, fh) = match side {
        Side::Bottom | Side::Top => (w, h),
        Side::Left | Side::Right => (h, w),
    };
    let (mut lo, mut hi) = match best {
        Some((_, _, o)) => {
            let u0 = fwd(side, x0, y0, w, h, o.start).0;
            let u1 = fwd(side, x0, y0, w, h, o.end).0;
            let a = py_min(py_max(u0, 0.0), fw);
            let b = py_min(py_max(u1, 0.0), fw);
            if b < a {
                (b, a)
            } else {
                (a, b)
            }
        }
        None => (fw / 2.0, fw / 2.0),
    };
    if hi - lo < 0.9 {
        let c = py_min(py_max((lo + hi) / 2.0, 0.45), fw - 0.45);
        lo = c - 0.45;
        hi = c + 0.45;
    }
    let mut obstacles: Vec<Vec<Coord>> = Vec::new();
    for ob in &site.fixed_obstacles {
        if let Some(clipped) = geometry::clip_polygon_to_box(ob, &rbox) {
            if geometry::polygon_area(&clipped) > 0.01 {
                obstacles.push(
                    clipped
                        .iter()
                        .map(|p| fwd(side, x0, y0, w, h, *p))
                        .collect(),
                );
            }
        }
    }
    let mut framed = site.clone();
    framed.floor_boundary = vec![(0.0, 0.0), (fw, 0.0), (fw, fh), (0.0, fh)];
    framed.fixed_obstacles = obstacles;
    framed.fixed_walls = vec![];
    framed.entrances = vec![Opening {
        kind: "entrance".into(),
        start: (py::round(lo, 3), 0.0),
        end: (py::round(hi, 3), 0.0),
    }];
    framed.exits = vec![];
    framed.windows = vec![];
    framed.fixture_locations = site
        .fixture_locations
        .iter()
        .filter(|fx| rbox.contains_strictly(fx.location))
        .map(|fx| {
            let mut f = fx.clone();
            f.location = fwd(side, x0, y0, w, h, fx.location);
            f
        })
        .collect();
    framed.restricted_areas = vec![];
    Ok(Some(Frame {
        site: framed,
        map: FrameMap::Rect { side, x0, y0, w, h },
        notes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arithmetic cell indices equal the binary searches they replace, for x-ranges at,
    /// next to and between column edges, beyond both grid ends, and on grids far from the
    /// origin where consecutive column edges round to the same value.
    #[test]
    fn near_cell_indices_equal_the_binary_searches() {
        let mut checked = 0;
        for (o, w, cell) in [
            (0.0, 10.0, 0.2),
            (-3.7, 9.3, 0.2),
            (6.1e5, 37.0, 0.2),
            (3.8e6, 12.5, 0.2),
            (1e15, 100.0, 0.2),
            (1e16, 60.0, 0.2),
            (0.0, 1e-3, 1e-4),
            (2.5, 7.0, 0.35),
        ] {
            let poly = [(o, o), (o + w, o), (o + w, o + w), (o, o + w)];
            let Ok(Some(grid)) = Grid::new(&poly, cell) else {
                panic!("grid for {o} {w} {cell}");
            };
            let xs = grid.xs.clone();
            let nx = grid.nx;
            let mut probes = vec![f64::MIN, -1e300, o - 1e9, o + w + 1e9, 1e300, f64::MAX];
            for &x in &xs {
                for d in [0.0, 1e-12, 1e-9, 1e-6, 1e-3, grid.pad, cell / 2.0] {
                    probes.extend([x - d, x + d, x.next_down(), x.next_up()]);
                }
            }
            for x in probes {
                assert_eq!(
                    grid.first_near_cell(x),
                    xs[1..].partition_point(|&e| e < x),
                    "first_near_cell({x}) on {o} {w} {cell}"
                );
                assert_eq!(
                    grid.near_cells_end(x),
                    xs[..nx].partition_point(|&e| e <= x),
                    "near_cells_end({x}) on {o} {w} {cell}"
                );
                checked += 1;
            }
        }
        assert!(checked > 10_000, "{checked}");
    }
}
