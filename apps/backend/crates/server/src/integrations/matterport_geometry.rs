//! `floor_geometry_from_model` (`integrations/matterport/__init__.py`): pick the largest floor of
//! a Matterport model and turn its wall graph into `SiteGeometry` fields (meters, the model's
//! local frame).
//!
//! Python does this with Shapely (GEOS): `polygonize(unary_union(walls))`, `unary_union(faces)`,
//! the largest polygon's exterior, `simplify(0.05, preserve_topology=True)`, and
//! `wall.buffer(0.05).within(boundary.buffer(0.15))` to tell boundary walls from interior ones.
//! This port computes the same things directly:
//! 1. the wall segments are noded (split at every crossing, touch and overlap) and deduplicated;
//! 2. dangling walls are removed repeatedly (polygonize ignores dangles);
//! 3. the faces of the planar graph are traced; the boundary of their union is the set of edges
//!    with a bounded face on one side only, chained into rings (rings that touch at a vertex are
//!    separate polygons, as GEOS returns them); the ring enclosing the largest area is the
//!    footprint, holes dropped;
//! 4. Douglas-Peucker with a 0.05 m tolerance on the closed ring (GEOS's algorithm, without its
//!    topology checks, which a floor outline at 5 cm tolerance does not trigger);
//! 5. a wall is on the boundary when every point of it is within 0.10 m of the footprint ring:
//!    `buffer(0.05) within buffer(0.15)` without GEOS's polygonal approximation of round caps.
//!
//! Output differences, documented: the ring has GEOS's clockwise orientation and the same
//! vertices in the same cyclic order, but it starts at its lowest-leftmost vertex (GEOS's start
//! depends on its internal edge order); walls within a few millimetres of the 0.10 m boundary
//! limit may be classified differently than GEOS's polygonal buffers would. Numbers keep
//! Python's types: wall coordinates copied from the model stay integers when the model sent
//! integers, computed coordinates are floats.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Number, Value};
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};
use ss_domain::py;

use super::matterport::PROVIDER;

const SIMPLIFY_TOLERANCE: f64 = 0.05;
/// `buffer(0.15) - buffer(0.05)`.
const ON_BOUNDARY: f64 = 0.10;
const MIN_AREA: f64 = 4.0;

type P = (f64, f64);
/// A model coordinate: its value, and the JSON number as the model sent it (int or float).
type Coord = (f64, Number);
/// A vertex position.
type Pos = (Coord, Coord);

fn bad(msg: impl Into<String>) -> ProviderError {
    ProviderError::new(PROVIDER, ProviderErrorKind::BadResponse, msg)
}

fn get<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

/// `x or default` for a JSON value used as a list.
fn list(v: &Value) -> &[Value] {
    match v {
        Value::Array(a) => a,
        _ => &[],
    }
}

fn num(v: &Value, what: &str) -> Result<(f64, Number), ProviderError> {
    match v {
        Value::Number(n) => n
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| (f, n.clone()))
            .ok_or_else(|| bad(format!("{what} is not a finite number"))),
        _ => Err(bad(format!("{what} is not a number"))),
    }
}

/// `round(v, 3)` keeping Python's type: an int stays an int.
fn round3(n: &Number) -> Value {
    if n.is_i64() || n.is_u64() {
        Value::Number(n.clone())
    } else {
        json!(py::round(n.as_f64().unwrap_or(0.0), 3))
    }
}

fn cross(o: P, a: P, b: P) -> f64 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

fn dist_point_segment(p: P, a: P, b: P) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
    let (x, y) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - x).powi(2) + (p.1 - y).powi(2)).sqrt()
}

/// Parameters along `a-b` (0..1, exclusive of nothing) where `c-d` meets it.
fn split_params(a: P, b: P, c: P, d: P, out: &mut Vec<f64>) {
    let r = (b.0 - a.0, b.1 - a.1);
    let s = (d.0 - c.0, d.1 - c.1);
    let rr = r.0 * r.0 + r.1 * r.1;
    if rr == 0.0 {
        return;
    }
    let denom = r.0 * s.1 - r.1 * s.0;
    let qp = (c.0 - a.0, c.1 - a.1);
    let eps = 1e-12 * rr.sqrt().max(1.0);
    if denom.abs() <= 1e-15 * rr.sqrt() * (s.0 * s.0 + s.1 * s.1).sqrt() {
        // Parallel: collinear overlap splits at the other segment's endpoints.
        if cross(a, b, c).abs() <= eps * rr.sqrt() && cross(a, b, d).abs() <= eps * rr.sqrt() {
            for p in [c, d] {
                let t = ((p.0 - a.0) * r.0 + (p.1 - a.1) * r.1) / rr;
                if t > 0.0 && t < 1.0 {
                    out.push(t);
                }
            }
        }
        return;
    }
    let t = (qp.0 * s.1 - qp.1 * s.0) / denom;
    let u = (qp.0 * r.1 - qp.1 * r.0) / denom;
    let tol = 1e-12;
    if (-tol..=1.0 + tol).contains(&t)
        && (-tol..=1.0 + tol).contains(&u)
        && t > tol
        && t < 1.0 - tol
    {
        out.push(t);
    }
}

/// Vertex identity after noding: coordinates snapped to 1e-9 m.
fn key(p: P) -> (i64, i64) {
    ((p.0 * 1e9).round() as i64, (p.1 * 1e9).round() as i64)
}

struct Graph {
    pts: Vec<P>,
    /// Undirected edges as vertex index pairs.
    edges: Vec<(usize, usize)>,
}

/// Node and deduplicate the segments (`unary_union` of the wall lines).
fn node(segments: &[(P, P)]) -> Graph {
    let mut index: HashMap<(i64, i64), usize> = HashMap::new();
    let mut pts: Vec<P> = Vec::new();
    let mut vid = |p: P, pts: &mut Vec<P>| -> usize {
        *index.entry(key(p)).or_insert_with(|| {
            pts.push(p);
            pts.len() - 1
        })
    };
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut edges = Vec::new();
    for (i, &(a, b)) in segments.iter().enumerate() {
        let mut ts = vec![0.0, 1.0];
        for (j, &(c, d)) in segments.iter().enumerate() {
            if i != j {
                split_params(a, b, c, d, &mut ts);
            }
        }
        ts.sort_by(f64::total_cmp);
        ts.dedup_by(|x, y| (*x - *y).abs() < 1e-12);
        let at = |t: f64| -> P {
            if t == 0.0 {
                a
            } else if t == 1.0 {
                b
            } else {
                (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
            }
        };
        for w in ts.windows(2) {
            let (u, v) = (vid(at(w[0]), &mut pts), vid(at(w[1]), &mut pts));
            if u != v && seen.insert((u.min(v), u.max(v))) {
                edges.push((u, v));
            }
        }
    }
    Graph { pts, edges }
}

/// Remove dangling edges until none is left (`polygonize` ignores dangles).
fn remove_dangles(g: &mut Graph) {
    loop {
        let mut degree = vec![0usize; g.pts.len()];
        for &(u, v) in &g.edges {
            degree[u] += 1;
            degree[v] += 1;
        }
        let before = g.edges.len();
        g.edges.retain(|&(u, v)| degree[u] > 1 && degree[v] > 1);
        if g.edges.len() == before {
            return;
        }
    }
}

fn signed_area(ring: &[P]) -> f64 {
    let n = ring.len();
    let mut s = 0.0;
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        s += a.0 * b.1 - b.0 * a.1;
    }
    s / 2.0
}

fn point_in_ring(p: P, ring: &[P]) -> bool {
    let n = ring.len();
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (ring[i], ring[j]);
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Trace cycles of directed half-edges `(u, v)`: from `u -> v`, continue with the outgoing
/// half-edge of `v` that is next clockwise from `v -> u` (the face lies on the left).
fn trace(pts: &[P], half: &[(usize, usize)]) -> Vec<Vec<usize>> {
    let mut out_of: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, &(u, _)) in half.iter().enumerate() {
        out_of.entry(u).or_default().push(i);
    }
    let angle = |&(u, v): &(usize, usize)| (pts[v].1 - pts[u].1).atan2(pts[v].0 - pts[u].0);
    for list in out_of.values_mut() {
        list.sort_by(|x, y| angle(&half[*x]).total_cmp(&angle(&half[*y])));
    }
    let pos: HashMap<(usize, usize), usize> =
        half.iter().enumerate().map(|(i, &e)| (e, i)).collect();
    let mut used = vec![false; half.len()];
    let mut cycles = Vec::new();
    for start in 0..half.len() {
        if used[start] {
            continue;
        }
        let mut cycle = Vec::new();
        let mut e = start;
        while !used[e] {
            used[e] = true;
            let (u, v) = half[e];
            cycle.push(u);
            let list = &out_of[&v];
            // Position of the twin `v -> u` among v's outgoing edges (sorted counter-clockwise),
            // or of the angle it would have when the twin is not a half-edge of this set.
            let twin_angle = (pts[u].1 - pts[v].1).atan2(pts[u].0 - pts[v].0);
            let k = match pos
                .get(&(v, u))
                .and_then(|t| list.iter().position(|x| x == t))
            {
                Some(k) => k,
                None => list
                    .iter()
                    .position(|x| angle(&half[*x]) > twin_angle)
                    .unwrap_or(list.len()),
            };
            e = list[(k + list.len() - 1) % list.len()];
        }
        cycles.push(cycle);
    }
    cycles
}

/// The rings bounding the union of the bounded faces, as point lists (counter-clockwise).
fn union_rings(g: &Graph) -> Vec<Vec<P>> {
    let mut half = Vec::with_capacity(g.edges.len() * 2);
    for &(u, v) in &g.edges {
        half.push((u, v));
        half.push((v, u));
    }
    let cycles = trace(&g.pts, &half);
    let rings: Vec<Vec<P>> = cycles
        .iter()
        .map(|c| c.iter().map(|&i| g.pts[i]).collect())
        .collect();
    let areas: Vec<f64> = rings.iter().map(|r| signed_area(r)).collect();
    // A clockwise cycle is the outer boundary of a connected piece; it borders a bounded face
    // when some counter-clockwise cycle of another piece encloses it.
    let bounded: Vec<bool> = (0..cycles.len())
        .map(|i| {
            if areas[i] > 0.0 {
                return true;
            }
            let probe = rings[i][0];
            let members: HashSet<usize> = cycles[i].iter().copied().collect();
            (0..cycles.len()).any(|j| {
                areas[j] > 0.0
                    && !cycles[j].iter().any(|v| members.contains(v))
                    && point_in_ring(probe, &rings[j])
            })
        })
        .collect();
    let mut face_of: HashMap<(usize, usize), usize> = HashMap::new();
    for (ci, c) in cycles.iter().enumerate() {
        for k in 0..c.len() {
            face_of.insert((c[k], c[(k + 1) % c.len()]), ci);
        }
    }
    let boundary: Vec<(usize, usize)> = half
        .iter()
        .copied()
        .filter(|&(u, v)| {
            let f = face_of.get(&(u, v)).copied();
            let t = face_of.get(&(v, u)).copied();
            matches!(f, Some(f) if bounded[f] && areas[f] > 0.0)
                && !matches!(t, Some(t) if bounded[t])
        })
        .collect();
    trace(&g.pts, &boundary)
        .into_iter()
        .map(|c| c.into_iter().map(|i| g.pts[i]).collect::<Vec<P>>())
        .filter(|r: &Vec<P>| r.len() >= 3 && signed_area(r) > 0.0)
        .collect()
}

/// Douglas-Peucker on a closed ring (first point fixed, as GEOS treats the ring as a line).
fn simplify_ring(ring: &[P], tol: f64) -> Vec<P> {
    let mut pts = ring.to_vec();
    pts.push(ring[0]);
    let mut keep = vec![true; pts.len()];
    fn section(pts: &[P], keep: &mut [bool], i: usize, j: usize, tol: f64) {
        if i + 1 >= j {
            return;
        }
        let (mut max, mut at) = (-1.0, i + 1);
        for k in i + 1..j {
            let d = dist_point_segment(pts[k], pts[i], pts[j]);
            if d > max {
                max = d;
                at = k;
            }
        }
        if max <= tol {
            for k in keep.iter_mut().take(j).skip(i + 1) {
                *k = false;
            }
        } else {
            section(pts, keep, i, at, tol);
            section(pts, keep, at, j, tol);
        }
    }
    let n = pts.len();
    section(&pts, &mut keep, 0, n - 1, tol);
    let out: Vec<P> = pts[..n - 1]
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(p, _)| *p)
        .collect();
    if out.len() < 3 {
        ring.to_vec()
    } else {
        out
    }
}

/// Clockwise, starting at the lowest-leftmost vertex.
fn canonical(ring: Vec<P>) -> Vec<P> {
    let mut r = ring;
    if signed_area(&r) > 0.0 {
        r.reverse();
    }
    let start = (0..r.len())
        .min_by(|&a, &b| r[a].0.total_cmp(&r[b].0).then(r[a].1.total_cmp(&r[b].1)))
        .unwrap_or(0);
    r.rotate_left(start);
    r
}

/// Is every point of `a-b` within `limit` of the ring? (Sampled every centimetre.)
fn near_ring(a: P, b: P, ring: &[P], limit: f64) -> bool {
    let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
    let steps = ((len / 0.01).ceil() as usize).clamp(1, 20_000);
    let n = ring.len();
    (0..=steps).all(|s| {
        let t = s as f64 / steps as f64;
        let p = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        (0..n).any(|i| dist_point_segment(p, ring[i], ring[(i + 1) % n]) <= limit)
    })
}

/// `_segment`: the part of wall `a-b` of `width` centred at `center` (a fraction along the wall).
fn opening_segment(a: P, b: P, center: f64, width: f64) -> Option<(P, P)> {
    // `** 0.5` is the platform's `pow`, as in CPython (not always bit-identical to `sqrt`).
    let length = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).powf(0.5);
    if length <= 0.0 {
        return None;
    }
    let half = (width / 2.0 / length).min(0.5);
    let (t0, t1) = ((center - half).max(0.0), (center + half).min(1.0));
    Some((
        (a.0 + (b.0 - a.0) * t0, a.1 + (b.1 - a.1) * t0),
        (a.0 + (b.0 - a.0) * t1, a.1 + (b.1 - a.1) * t1),
    ))
}

/// `x or default` for a JSON number.
fn num_or(v: &Value, default: f64) -> Result<f64, ProviderError> {
    if !py::truthy(v) {
        return Ok(default);
    }
    Ok(num(v, "opening value")?.0)
}

/// `floor_geometry_from_model(model)`: `None` when the model has no usable floor, the walls
/// enclose no face, or the footprint is smaller than 4 m2.
pub fn floor_geometry_from_model(
    model: &Map<String, Value>,
) -> Result<Option<Value>, ProviderError> {
    let model_v = Value::Object(model.clone());
    let floors: Vec<&Value> = list(get(&model_v, "floors"))
        .iter()
        .filter(|f| py::truthy(get(f, "vertices")) && py::truthy(get(f, "edges")))
        .collect();
    let mut floor: Option<&Value> = None;
    let mut best = (f64::NEG_INFINITY, 0usize);
    for f in floors {
        let area = get(get(f, "dimensions"), "areaFloor")
            .as_f64()
            .unwrap_or(0.0);
        let k = (area, list(get(f, "edges")).len());
        // `max(...)` keeps the first of equal maxima.
        if floor.is_none() || k.0 > best.0 || (k.0 == best.0 && k.1 > best.1) {
            floor = Some(f);
            best = k;
        }
    }
    let Some(floor) = floor else { return Ok(None) };
    let mut pos: HashMap<String, Pos> = HashMap::new();
    for v in list(get(floor, "vertices")) {
        let id = v.get("id").ok_or_else(|| bad("vertex without id"))?;
        let p = get(v, "position");
        let x = num(get(p, "x"), "vertex x")?;
        let y = num(get(p, "y"), "vertex y")?;
        pos.insert(id.to_string(), (x, y));
    }
    let mut segments: Vec<(&Value, Pos, Pos)> = Vec::new();
    for e in list(get(floor, "edges")) {
        let ids: Vec<String> = list(get(e, "vertices"))
            .iter()
            .map(|v| v.get("id").map(Value::to_string).unwrap_or_default())
            .collect();
        if ids.len() == 2 {
            if let (Some(a), Some(b)) = (pos.get(&ids[0]), pos.get(&ids[1])) {
                if (a.0 .0, a.1 .0) != (b.0 .0, b.1 .0) {
                    segments.push((e, a.clone(), b.clone()));
                }
            }
        }
    }
    let plain: Vec<(P, P)> = segments
        .iter()
        .map(|(_, a, b)| ((a.0 .0, a.1 .0), (b.0 .0, b.1 .0)))
        .collect();
    let mut graph = node(&plain);
    remove_dangles(&mut graph);
    let rings = union_rings(&graph);
    // The largest polygon (first of equal areas), holes dropped.
    let Some(footprint) = rings.into_iter().fold(None::<Vec<P>>, |acc, r| match acc {
        Some(a) if signed_area(&a).abs() >= signed_area(&r).abs() => Some(a),
        _ => Some(r),
    }) else {
        return Ok(None);
    };
    let boundary = simplify_ring(&canonical(footprint), SIMPLIFY_TOLERANCE);
    if signed_area(&boundary).abs() < MIN_AREA {
        return Ok(None);
    }
    let (mut fixed_walls, mut entrances, mut windows, mut interior_doors) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let pt = |p: P| json!([p.0, p.1]);
    for ((e, a, b), (pa, pb)) in segments.iter().zip(&plain) {
        let on_boundary = near_ring(*pa, *pb, &boundary, ON_BOUNDARY);
        for op in list(get(e, "openings")) {
            let center = num_or(get(op, "relativeCenter"), 0.5)?;
            let width = num_or(get(op, "width"), 0.9)?;
            let Some((s0, s1)) = opening_segment(*pa, *pb, center, width) else {
                continue;
            };
            match get(op, "type").as_str() {
                Some("window") if on_boundary => {
                    windows.push(json!({"kind": "window", "start": pt(s0), "end": pt(s1)}))
                }
                Some("doorway" | "opening") if on_boundary => {
                    entrances.push(json!({"kind": "entrance", "start": pt(s0), "end": pt(s1)}))
                }
                Some("doorway" | "opening") => interior_doors
                    .push(json!({"kind": "interior_door", "start": pt(s0), "end": pt(s1)})),
                _ => {}
            }
        }
        if get(e, "type").as_str() == Some("wall") && !on_boundary {
            fixed_walls.push(json!([
                [round3(&a.0 .1), round3(&a.1 .1)],
                [round3(&b.0 .1), round3(&b.1 .1)]
            ]));
        }
    }
    let coords: Vec<Value> = boundary
        .iter()
        .map(|p| json!([py::round(p.0, 3), py::round(p.1, 3)]))
        .collect();
    let floor_id = get(floor, "id");
    let rooms: Vec<Value> = list(get(&model_v, "rooms"))
        .iter()
        .filter(|r| get(get(r, "floor"), "id") == floor_id)
        .map(|r| {
            let label = match get(r, "label") {
                v if py::truthy(v) => v.clone(),
                _ => {
                    let first = list(get(r, "classifications"))
                        .first()
                        .cloned()
                        .unwrap_or(Value::Null);
                    get(&first, "label").clone()
                }
            };
            json!({"label": label, "area_sqm": get(get(r, "dimensions"), "areaFloor")})
        })
        .collect();
    Ok(Some(json!({
        "source_id": get(&model_v, "id"),
        "model_name": get(&model_v, "name"),
        "floor_label": get(floor, "label"),
        "floor_count": list(get(&model_v, "floors")).len(),
        "measured_area_sqm": get(get(floor, "dimensions"), "areaFloor"),
        "geometry_source": "MATTERPORT_MEASURED",
        "data_origin": "LIVE",
        "floor_boundary": coords,
        "fixed_walls": fixed_walls,
        "entrances": entrances,
        "windows": windows,
        "interior_doors": interior_doors,
        "rooms": rooms,
        "scale_verified": true,
        "measurement_uncertainty_m": null,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square_ring(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<(P, P)> {
        let p = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
        (0..4).map(|i| (p[i], p[(i + 1) % 4])).collect()
    }

    fn footprint(segs: &[(P, P)]) -> Vec<P> {
        let mut g = node(segs);
        remove_dangles(&mut g);
        let rings = union_rings(&g);
        let best = rings
            .into_iter()
            .max_by(|a, b| signed_area(a).total_cmp(&signed_area(b)))
            .unwrap();
        simplify_ring(&canonical(best), SIMPLIFY_TOLERANCE)
    }

    /// Footprints Shapely 2.2 / GEOS 3.14 returns for the same walls (`polygonize`,
    /// `unary_union`, largest polygon, `simplify(0.05)`), compared up to the starting vertex.
    #[test]
    fn footprints_match_geos() {
        let rot = |v: Vec<P>| canonical(v);
        // Two separate loops: the larger.
        let mut two = square_ring(0.0, 0.0, 4.0, 4.0);
        two.extend(square_ring(10.0, 0.0, 16.0, 6.0));
        assert_eq!(
            footprint(&two),
            rot(vec![(10.0, 0.0), (10.0, 6.0), (16.0, 6.0), (16.0, 0.0)])
        );
        // A dangling wall is ignored.
        let mut dangle = square_ring(0.0, 0.0, 4.0, 4.0);
        dangle.push(((4.0, 4.0), (6.0, 6.0)));
        assert_eq!(
            footprint(&dangle),
            rot(vec![(4.0, 0.0), (0.0, 0.0), (0.0, 4.0), (4.0, 4.0)])
        );
        // An inner loop does not change the outline.
        let mut inner = square_ring(0.0, 0.0, 10.0, 10.0);
        inner.extend(square_ring(4.0, 4.0, 6.0, 6.0));
        assert_eq!(
            footprint(&inner),
            vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0)]
        );
        // A wall crossing the room and sticking out on both sides.
        let mut crossing = square_ring(0.0, 0.0, 10.0, 10.0);
        crossing.push(((-2.0, 5.0), (12.0, 5.0)));
        assert_eq!(
            footprint(&crossing),
            vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0)]
        );
        // A vertex 3 cm off the line is simplified away.
        let near = vec![
            ((0.0, 0.0), (5.0, 0.03)),
            ((5.0, 0.03), (10.0, 0.0)),
            ((10.0, 0.0), (10.0, 10.0)),
            ((10.0, 10.0), (0.0, 10.0)),
            ((0.0, 10.0), (0.0, 0.0)),
        ];
        assert_eq!(
            footprint(&near),
            vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0)]
        );
        // A rotated square and a triangle keep their vertices.
        let tri = vec![
            ((1.0, 1.0), (9.0, 2.0)),
            ((9.0, 2.0), (4.0, 7.0)),
            ((4.0, 7.0), (1.0, 1.0)),
        ];
        assert_eq!(
            footprint(&tri),
            rot(vec![(9.0, 2.0), (1.0, 1.0), (4.0, 7.0)])
        );
        let diamond = vec![
            ((5.0, 0.0), (10.0, 5.0)),
            ((10.0, 5.0), (5.0, 10.0)),
            ((5.0, 10.0), (0.0, 5.0)),
            ((0.0, 5.0), (5.0, 0.0)),
        ];
        assert_eq!(
            footprint(&diamond),
            rot(vec![(10.0, 5.0), (5.0, 0.0), (0.0, 5.0), (5.0, 10.0)])
        );
    }

    #[test]
    fn rings_touching_at_a_vertex_are_separate() {
        let mut segs = square_ring(0.0, 0.0, 4.0, 4.0);
        segs.extend(square_ring(4.0, 4.0, 9.0, 9.0));
        let mut g = node(&segs);
        remove_dangles(&mut g);
        let mut areas: Vec<f64> = union_rings(&g).iter().map(|r| signed_area(r)).collect();
        areas.sort_by(f64::total_cmp);
        assert_eq!(areas, vec![16.0, 25.0]);
    }
}
