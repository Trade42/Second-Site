"""Business-type requirement templates -> InfrastructureRequirement lists (deterministic).

Owner: agents-finance. Values are planning heuristics, not code requirements.
"""

from __future__ import annotations

from app.contracts import BusinessExpansionProfile, InfrastructureRequirement, RequirementCategory, Severity

SQM_PER_ROOM = {"treatment": 11.0, "reception": 14.0, "storage": 5.0, "restroom": 5.0, "office": 9.0}
CIRCULATION_FACTOR = 1.3


def min_area_sqm(profile: BusinessExpansionProfile) -> float:
    rooms = profile.space_requirements.required_rooms
    computed = sum(SQM_PER_ROOM.get(k, 9.0) * v for k, v in rooms.items()) * CIRCULATION_FACTOR
    return round(max(computed, profile.space_requirements.min_area_sqm or 0.0), 1)


def requirements_for(profile: BusinessExpansionProfile) -> list[InfrastructureRequirement]:
    bt = profile.business_type
    rooms = profile.space_requirements.required_rooms
    treatment = rooms.get("treatment", 0)
    sinks = treatment * profile.infrastructure_requirements.sinks_per_treatment_room
    reqs = [
        InfrastructureRequirement(
            requirement_id=f"{bt}.space.min_area",
            business_type=bt,
            requirement_category=RequirementCategory.SPACE,
            description="Usable area for required rooms plus circulation",
            required_value=min_area_sqm(profile),
            required_unit="m2",
            severity=Severity.HARD,
            verification_method="Measured floor plan / site visit",
        ),
        InfrastructureRequirement(
            requirement_id=f"{bt}.space.enclosed_rooms",
            business_type=bt,
            requirement_category=RequirementCategory.SPACE,
            description="Enclosed rooms matching the program",
            required_value=sum(rooms.values()),
            required_unit="rooms",
            severity=Severity.SOFT,
            verification_method="Floor plan review",
        ),
        InfrastructureRequirement(
            requirement_id=f"{bt}.electrical.tenant_service",
            business_type=bt,
            requirement_category=RequirementCategory.ELECTRICAL,
            description="Electrical service available to the tenant space for treatment equipment and lighting",
            required_value=profile.infrastructure_requirements.min_electrical_service_amps or 100,
            required_unit="A",
            severity=Severity.SOFT,
            verification_method="Licensed electrician inspects tenant subpanel rating and load",
        ),
    ]
    if sinks:
        reqs.append(
            InfrastructureRequirement(
                requirement_id=f"{bt}.plumbing.treatment_sinks",
                business_type=bt,
                requirement_category=RequirementCategory.PLUMBING,
                description="Hand/treatment sink in each treatment room (supply + drain)",
                required_value=sinks,
                required_unit="sinks",
                severity=Severity.SOFT,
                verification_method="Plumber confirms supply/drain locations and capacity",
            )
        )
        reqs.append(
            InfrastructureRequirement(
                requirement_id=f"{bt}.plumbing.water_service",
                business_type=bt,
                requirement_category=RequirementCategory.PLUMBING,
                description="Domestic water service to the building",
                required_value=True,
                severity=Severity.SOFT,
                verification_method="Plumber / utility records",
            )
        )
    if bt in {"facial_studio", "med_spa", "salon"}:
        reqs.append(
            InfrastructureRequirement(
                requirement_id=f"{bt}.hvac.ventilation",
                business_type=bt,
                requirement_category=RequirementCategory.HVAC,
                description="Conditioned air and ventilation zoned to treatment rooms",
                required_value=True,
                severity=Severity.SOFT,
                verification_method="Mechanical contractor review of supply/return per room",
            )
        )
    return reqs
