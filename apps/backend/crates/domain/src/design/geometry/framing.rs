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
use crate::geometry::{self, Bounds};
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

/// `largest_inner_rectangle(poly, cell)`: the largest axis-aligned rectangle on a `cell` grid
/// (anchored at the polygon's lower-left bound) whose cells all lie inside
/// `poly.buffer(1e-6)`, by the histogram method. `(x0, y0, x1, y1)`, or `None` when the grid has
/// fewer than 2 cells in either direction or no cell fits.
pub fn largest_inner_rectangle(poly: &[Coord], cell: f64) -> Option<(f64, f64, f64, f64)> {
    let b = Bounds::of(poly)?;
    let nx_f = ((b.maxx - b.minx) / cell).trunc();
    let ny_f = ((b.maxy - b.miny) / cell).trunc();
    if !(nx_f >= 2.0 && ny_f >= 2.0) || !nx_f.is_finite() || !ny_f.is_finite() {
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
                if area > best {
                    best = area;
                    best_rect = Some((left, j + 1 - heights[top], i, j + 1));
                }
            }
            stack.push(i);
        }
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
/// `min_area` square meters.
pub fn frame_for_solver(site: &SiteGeometry, min_area: f64) -> Option<Frame> {
    if is_solver_ready(site) {
        return Some(Frame {
            site: site.clone(),
            map: FrameMap::Identity,
            notes: vec![],
        });
    }
    let (x0, y0, x1, y1) = largest_inner_rectangle(&site.floor_boundary, INNER_RECTANGLE_CELL)?;
    let (w, h) = (x1 - x0, y1 - y0);
    if w * h < min_area {
        return None;
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
    Some(Frame {
        site: framed,
        map: FrameMap::Rect { side, x0, y0, w, h },
        notes,
    })
}
