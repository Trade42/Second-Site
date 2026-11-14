//! Port of `apps/api/app/domains/spatial/validation.py`: deterministic geometry validation.
//!
//! Checks, in Python's order: rooms inside the floor boundary, no room overlaps, no rooms in
//! circulation, fixed obstacles out of circulation, entrances and exits kept clear, minimum
//! usable area (fixed obstacles excluded) and minimum width, and the room count. Issue codes,
//! messages, `room_ids` and order are Python's. The Shapely operations are the ones in
//! [`crate::geometry`].
//!
//! Differences from Python, all for inputs the contracts reject: Python's Shapely calls raise on
//! some invalid (self-intersecting) room polygons and `min_dimension` raises for a room with no
//! area; this port never raises, reports `INVALID_POLYGON` as Python does, and treats the width
//! of a shape with no area as 0.

use indexmap::IndexMap;

use crate::contracts::geometry::{
    Coord, GeometryValidationReport, RoomPolygon, RoomRequirement, SiteGeometry, ValidationIssue,
};
use crate::geometry;
use crate::py;

/// Tolerance for containment, overlaps and the area and width comparisons.
pub const EPS: f64 = 1e-6;

/// `min_dimension(poly)`: the shorter side of the polygon's minimum rotated rectangle.
pub fn min_dimension(polygon: &[Coord]) -> f64 {
    geometry::min_rotated_rectangle_min_side(polygon)
}

/// `validate_layout(site, rooms, requirements, circulation)`. `circulation` of `None` or an
/// empty list skips the circulation checks, as Python's `if circulation:` does.
pub fn validate_layout(
    site: &SiteGeometry,
    rooms: &[RoomPolygon],
    requirements: &[RoomRequirement],
    circulation: Option<&[Vec<Coord>]>,
) -> GeometryValidationReport {
    let mut issues: Vec<ValidationIssue> = Vec::new();
    let boundary = &site.floor_boundary;
    // Python's dict comprehension: the first position of a room id, its last polygon.
    let mut polys: IndexMap<&str, &[Coord]> = IndexMap::new();
    for r in rooms {
        polys.insert(r.room_id.as_str(), r.polygon.as_slice());
    }
    let obstacles: &[Vec<Coord>] = &site.fixed_obstacles;
    // `{r.room_type: r for r in requirements}`: the last requirement of a type wins.
    let mut req_by_type: IndexMap<&str, &RoomRequirement> = IndexMap::new();
    for r in requirements {
        req_by_type.insert(r.room_type.as_str(), r);
    }

    for (rid, poly) in &polys {
        if !geometry::is_valid(poly) {
            issues.push(ValidationIssue::new(
                "INVALID_POLYGON",
                format!("{rid} polygon invalid"),
                vec![(*rid).to_owned()],
            ));
            continue;
        }
        if !geometry::contains_with_tolerance(boundary, poly, EPS) {
            issues.push(ValidationIssue::new(
                "OUTSIDE_BOUNDARY",
                format!("{rid} extends outside the floor"),
                vec![(*rid).to_owned()],
            ));
        }
    }
    let ids: Vec<(&str, &[Coord])> = polys.iter().map(|(k, v)| (*k, *v)).collect();
    for (i, (a, pa)) in ids.iter().enumerate() {
        for (b, pb) in &ids[i + 1..] {
            if geometry::intersection_area(pa, pb) > EPS {
                issues.push(ValidationIssue::new(
                    "ROOM_OVERLAP",
                    format!("{a} overlaps {b}"),
                    vec![(*a).to_owned(), (*b).to_owned()],
                ));
            }
        }
    }
    if let Some(circ) = circulation.filter(|c| !c.is_empty()) {
        for (rid, poly) in &ids {
            if geometry::area_inside_union(poly, circ) > EPS {
                issues.push(ValidationIssue::new(
                    "BLOCKS_CIRCULATION",
                    format!("{rid} intrudes into circulation"),
                    vec![(*rid).to_owned()],
                ));
            }
        }
        if !obstacles.is_empty() && geometry::union_intersection_area(circ, obstacles) > EPS {
            issues.push(ValidationIssue::new(
                "OBSTACLE_IN_CIRCULATION",
                "fixed obstacle blocks circulation",
                vec![],
            ));
        }
    }
    for opening in site.entrances.iter().chain(&site.exits) {
        for (rid, poly) in &ids {
            let rt = rooms
                .iter()
                .find(|r| r.room_id == *rid)
                .map_or("", |r| r.room_type.as_str());
            // Blocked when a non-reception room covers part of the opening itself (touching its
            // end is fine).
            if rt != "reception"
                && geometry::segment_length_inside(poly, opening.start, opening.end) > 0.01
            {
                issues.push(ValidationIssue::new(
                    "OPENING_BLOCKED",
                    format!("{rid} blocks {}", opening.kind),
                    vec![(*rid).to_owned()],
                ));
            }
        }
    }
    for r in rooms {
        let poly = polys.get(r.room_id.as_str()).copied().unwrap_or(&[]);
        let Some(req) = req_by_type.get(r.room_type.as_str()) else {
            continue;
        };
        let usable = geometry::difference_area(poly, obstacles);
        if usable + EPS < req.min_area_sqm {
            issues.push(ValidationIssue::new(
                "BELOW_MIN_AREA",
                format!(
                    "{} usable area {usable:.1} m2 < {}",
                    r.room_id,
                    py::float_repr(req.min_area_sqm)
                ),
                vec![r.room_id.clone()],
            ));
        }
        if min_dimension(poly) + EPS < req.min_width_m {
            issues.push(ValidationIssue::new(
                "BELOW_MIN_WIDTH",
                format!(
                    "{} narrower than {} m",
                    r.room_id,
                    py::float_repr(req.min_width_m)
                ),
                vec![r.room_id.clone()],
            ));
        }
    }
    // Python's ints are unbounded; i128 cannot overflow on a list of i64 counts.
    let expected: i128 = requirements.iter().map(|r| i128::from(r.count)).sum();
    if (rooms.len() as i128) < expected {
        issues.push(ValidationIssue::new(
            "MISSING_ROOMS",
            format!("{} of {expected} required rooms placed", rooms.len()),
            vec![],
        ));
    }
    GeometryValidationReport::new(issues.is_empty(), issues)
}
