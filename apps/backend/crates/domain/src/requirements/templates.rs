//! Port of `apps/api/app/domains/requirements/templates.py`: business-type requirement templates
//! -> `InfrastructureRequirement` lists (deterministic). Values are planning heuristics, not code
//! requirements.

use ss_core::BusinessExpansionProfile;

use crate::contracts::{InfrastructureRequirement, RequirementCategory, ScalarValue, Severity};
use crate::py;

pub const CIRCULATION_FACTOR: f64 = 1.3;

/// `SQM_PER_ROOM.get(room, 9.0)`.
pub fn sqm_per_room(room: &str) -> f64 {
    match room {
        "treatment" => 11.0,
        "reception" => 14.0,
        "storage" => 5.0,
        "restroom" => 5.0,
        "office" => 9.0,
        _ => 9.0,
    }
}

/// Business types that get the treatment-room ventilation requirement.
pub const VENTILATED_TYPES: &[&str] = &["facial_studio", "med_spa", "salon"];

/// `min_area_sqm`: rooms x area per room x circulation, or the profile's own minimum if larger.
pub fn min_area_sqm(profile: &BusinessExpansionProfile) -> f64 {
    let rooms = &profile.space_requirements.required_rooms;
    let mut sum = 0.0;
    for (k, v) in rooms {
        sum += sqm_per_room(k) * (*v as f64);
    }
    let computed = sum * CIRCULATION_FACTOR;
    let own = match profile.space_requirements.min_area_sqm {
        Some(m) if m != 0.0 => m,
        _ => 0.0,
    };
    // Python's max(computed, own) keeps the first argument unless the second is larger.
    py::round(if own > computed { own } else { computed }, 1)
}

/// `requirements_for`.
pub fn requirements_for(profile: &BusinessExpansionProfile) -> Vec<InfrastructureRequirement> {
    let bt = profile.business_type.as_str();
    let rooms = &profile.space_requirements.required_rooms;
    let treatment = rooms.get("treatment").copied().unwrap_or(0);
    let sinks =
        treatment.saturating_mul(profile.infrastructure_requirements.sinks_per_treatment_room);
    let req = |id: &str,
               category: RequirementCategory,
               description: &str,
               value: Option<ScalarValue>,
               unit: Option<&str>,
               severity: Severity,
               method: &str| InfrastructureRequirement {
        requirement_id: format!("{bt}.{id}"),
        business_type: bt.to_owned(),
        requirement_category: category,
        description: description.to_owned(),
        required_value: value,
        required_unit: unit.map(str::to_owned),
        severity,
        verification_method: method.to_owned(),
    };
    let amps = match profile
        .infrastructure_requirements
        .min_electrical_service_amps
    {
        Some(a) if a != 0 => a,
        _ => 100,
    };
    let mut reqs =
        vec![
        req(
            "space.min_area",
            RequirementCategory::Space,
            "Usable area for required rooms plus circulation",
            Some(min_area_sqm(profile).into()),
            Some("m2"),
            Severity::Hard,
            "Measured floor plan / site visit",
        ),
        req(
            "space.enclosed_rooms",
            RequirementCategory::Space,
            "Enclosed rooms matching the program",
            Some(rooms.values().fold(0i64, |a, v| a.saturating_add(*v)).into()),
            Some("rooms"),
            Severity::Soft,
            "Floor plan review",
        ),
        req(
            "electrical.tenant_service",
            RequirementCategory::Electrical,
            "Electrical service available to the tenant space for treatment equipment and lighting",
            Some(amps.into()),
            Some("A"),
            Severity::Soft,
            "Licensed electrician inspects tenant subpanel rating and load",
        ),
    ];
    if sinks != 0 {
        reqs.push(req(
            "plumbing.treatment_sinks",
            RequirementCategory::Plumbing,
            "Hand/treatment sink in each treatment room (supply + drain)",
            Some(sinks.into()),
            Some("sinks"),
            Severity::Soft,
            "Plumber confirms supply/drain locations and capacity",
        ));
        reqs.push(req(
            "plumbing.water_service",
            RequirementCategory::Plumbing,
            "Domestic water service to the building",
            Some(true.into()),
            None,
            Severity::Soft,
            "Plumber / utility records",
        ));
    }
    if VENTILATED_TYPES.contains(&bt) {
        reqs.push(req(
            "hvac.ventilation",
            RequirementCategory::Hvac,
            "Conditioned air and ventilation zoned to treatment rooms",
            Some(true.into()),
            None,
            Severity::Soft,
            "Mechanical contractor review of supply/return per room",
        ));
    }
    reqs
}
