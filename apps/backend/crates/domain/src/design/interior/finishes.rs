//! Port of `apps/api/app/domains/design/interior/finishes.py`: deterministic finish presets.

use crate::contracts::geometry::{FinishSpecification, RoomPolygon};

/// `STYLE_PRESETS`: `(style, floor material, wall material)`; `None` is the fallback preset.
pub const STYLE_PRESETS: &[(Option<&str>, &str, &str)] = &[
    (
        Some("warm_minimalist"),
        "light oak-look LVP",
        "warm white eggshell paint",
    ),
    (None, "neutral LVP", "white eggshell paint"),
];

/// `finish_specifications(rooms, style)`: one floor finish per room, sized to the room's area,
/// from the style's preset (an unknown or absent style uses the fallback preset).
pub fn finish_specifications(rooms: &[RoomPolygon], style: Option<&str>) -> Vec<FinishSpecification> {
    let preset = STYLE_PRESETS
        .iter()
        .find(|(s, _, _)| *s == style)
        .or_else(|| STYLE_PRESETS.iter().find(|(s, _, _)| s.is_none()));
    let floor = preset.map_or("neutral LVP", |(_, f, _)| *f);
    rooms
        .iter()
        .map(|r| FinishSpecification {
            room_id: Some(r.room_id.clone()),
            surface: "floor".into(),
            material: floor.into(),
            area_sqm: r.area_sqm,
        })
        .collect()
}
