"""Infrastructure Feasibility Engine (deterministic rules over sourced evidence).

Inputs: BusinessExpansionProfile, InfrastructureRequirement[], PropertyDigitalProfile, SourceEvidence[].
Outputs: InfrastructureAssessment[] + RenovationWorkItem[].

Truthfulness rules:
- Historical permit facts never yield MEETS for current-capacity requirements; at most UNKNOWN or
  MAY_REQUIRE_WORK with explicit limitations and verification questions.
- UNKNOWN is never MEETS; unknowns carry a cost allowance range so ranking reflects the risk.
"""

from __future__ import annotations

from decimal import Decimal
from uuid import UUID

from app.contracts import (
    AssessmentStatus,
    BusinessExpansionProfile,
    EvidenceClassification,
    InfrastructureAssessment,
    InfrastructureRequirement,
    PermitScope,
    PropertyDigitalProfile,
    RenovationWorkItem,
    ReviewStatus,
    SourceEvidence,
    WorkCategory,
)
from app.domains.finance.cost_catalog import COSTS

USABLE = {
    EvidenceClassification.DOCUMENTED_HISTORICAL,
    EvidenceClassification.GEOMETRY_MEASURED,
    EvidenceClassification.OWNER_REPORTED,
    EvidenceClassification.PROFESSIONALLY_VERIFIED,
    EvidenceClassification.VISUALLY_OBSERVED,
}


def _work(
    property_id: UUID,
    key: str,
    category: WorkCategory,
    description: str,
    qty: float,
    assumptions: list[str],
    evidence_ids: list[UUID],
    assessment_id: UUID | None,
) -> RenovationWorkItem:
    unit, lo, ex, hi = COSTS[key]
    return RenovationWorkItem(
        property_id=property_id,
        category=category,
        description=description,
        quantity=Decimal(str(qty)),
        unit=unit,
        unit_cost_low=lo,
        unit_cost_expected=ex,
        unit_cost_high=hi,
        requires_professional_quote=True,
        assumptions=assumptions + [f"cost_key={key}"],
        evidence_ids=evidence_ids,
        source_assessment_id=assessment_id,
    )


class FeasibilityEngine:
    def evaluate(
        self,
        profile: BusinessExpansionProfile,
        requirements: list[InfrastructureRequirement],
        prop: PropertyDigitalProfile,
        evidence: list[SourceEvidence],
    ) -> tuple[list[InfrastructureAssessment], list[RenovationWorkItem]]:
        ev = [
            e for e in evidence if e.property_id == prop.property_id and e.evidence_classification in USABLE
        ]
        unit_permit_evidence = {
            eid for p in prop.permit_records if p.scope == PermitScope.UNIT for eid in p.source_evidence_ids
        }
        by_type: dict[str, list[SourceEvidence]] = {}
        for e in ev:
            by_type.setdefault(e.observation_type, []).append(e)
        listing = prop.commercial_listing
        assessments: list[InfrastructureAssessment] = []
        work: list[RenovationWorkItem] = []
        pid, prj = prop.property_id, prop.project_id
        open_shell = _buildout(prop) == "open_shell"

        for req in requirements:
            rid = req.requirement_id
            if rid.endswith("space.min_area"):
                area = listing.available_area_sqm if listing else None
                if area is None:
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.UNKNOWN,
                        required_value=req.required_value,
                        uncertainty="Available area not provided by any source.",
                        verification_questions=["What is the rentable and usable area?"],
                    )
                else:
                    ok = area >= float(req.required_value)
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.MAY_REQUIRE_WORK if ok else AssessmentStatus.FAILS,
                        observed_value=area,
                        required_value=req.required_value,
                        limitations=["Area comes from the listing (not measured); usable area may be lower."],
                        verification_questions=["Confirm usable (not rentable) square footage."],
                        uncertainty="Listing area, not a measured floor plan.",
                        review_status=ReviewStatus.NEEDS_REVIEW,
                    )
                assessments.append(a)

            elif rid.endswith("space.enclosed_rooms"):
                n = int(req.required_value)
                key = "partition_room_open_shell" if open_shell else "partition_room_office_buildout"
                a = InfrastructureAssessment(
                    project_id=prj,
                    property_id=pid,
                    requirement_id=rid,
                    status=AssessmentStatus.MAY_REQUIRE_WORK,
                    required_value=n,
                    observed_value=_buildout(prop) or "unknown",
                    proposed_work=[f"Build or adapt {n} enclosed rooms"],
                    verification_questions=["Can existing partitions be reused for treatment rooms?"],
                    uncertainty="Existing partition layout not measured.",
                )
                assessments.append(a)
                work.append(
                    _work(
                        pid,
                        key,
                        WorkCategory.PARTITIONS,
                        f"Partitions for {n} rooms ({_buildout(prop) or 'unknown build-out'})",
                        n,
                        ["Room count from business profile"],
                        [],
                        a.assessment_id,
                    )
                )

            elif rid.endswith("electrical.tenant_service"):
                amps = by_type.get("electrical_service_amps", [])
                reported = by_type.get("tenant_subpanel_amps", [])
                questions = [
                    "What amperage is allocated to the tenant space's subpanel, and how much is in use?"
                ]
                if reported:
                    best = max(reported, key=lambda e: float(e.extracted_value))
                    enough = float(best.extracted_value) >= float(req.required_value)
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.MEETS if enough else AssessmentStatus.MAY_REQUIRE_WORK,
                        observed_value=f"{int(float(best.extracted_value))}A tenant panel (broker-reported)",
                        required_value=req.required_value,
                        evidence_ids=[e.evidence_id for e in reported + amps],
                        limitations=list(best.limitations)
                        + ["Reported capacity is not verified load data; confirm with an electrician."],
                        verification_questions=[
                            "Can an electrician confirm the panel rating and current load?"
                        ],
                        uncertainty="Broker statement, not a load calculation or inspection.",
                        review_status=ReviewStatus.NEEDS_REVIEW,
                    )
                    assessments.append(a)
                    work.append(
                        _work(
                            pid,
                            "electrical_circuits_reported_capacity"
                            if enough
                            else "electrical_subpanel_allowance",
                            WorkCategory.ELECTRICAL,
                            "Dedicated circuits for treatment equipment"
                            if enough
                            else "Allowance: subpanel/circuit upgrade (reported capacity below requirement)",
                            1,
                            ["Based on broker-reported panel capacity"],
                            a.evidence_ids,
                            a.assessment_id,
                        )
                    )
                    continue
                if amps:
                    best = max(amps, key=lambda e: float(e.extracted_value))
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.UNKNOWN,
                        observed_value=f"{int(float(best.extracted_value))}A building service (historical permit)",
                        required_value=req.required_value,
                        evidence_ids=[e.evidence_id for e in amps],
                        limitations=list(best.limitations),
                        verification_questions=questions,
                        uncertainty="A documented building service does not establish capacity available to this unit.",
                        review_status=ReviewStatus.NEEDS_REVIEW,
                    )
                else:
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.UNKNOWN,
                        required_value=req.required_value,
                        verification_questions=questions,
                        uncertainty="No electrical service rating found in permits or listing.",
                    )
                assessments.append(a)
                work.append(
                    _work(
                        pid,
                        "electrical_subpanel_allowance",
                        WorkCategory.ELECTRICAL,
                        "Allowance: possible tenant subpanel/circuit upgrade (capacity unknown)",
                        1,
                        ["Unknown capacity carried as an allowance range"],
                        a.evidence_ids,
                        a.assessment_id,
                    )
                )

            elif rid.endswith("plumbing.treatment_sinks"):
                n = int(req.required_value)
                sinks = by_type.get("sink_installed", [])
                in_unit = [e for e in sinks if e.evidence_id in unit_permit_evidence]
                reported_plumbing = by_type.get("in_unit_plumbing", [])
                if reported_plumbing and not in_unit:
                    rp = reported_plumbing[-1]
                    has = bool(rp.extracted_value)
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.MAY_REQUIRE_WORK,
                        observed_value="Broker reports existing in-unit plumbing"
                        if has
                        else "Broker reports no plumbing in the unit",
                        required_value=n,
                        evidence_ids=[rp.evidence_id],
                        limitations=list(rp.limitations),
                        proposed_work=[
                            f"Extend supply/drain from existing in-unit plumbing to {n} treatment sinks"
                            if has
                            else f"New supply and drain runs for {n} treatment sinks"
                        ],
                        verification_questions=["Where exactly are the supply and drain lines?"],
                        uncertainty="Broker statement; line locations and capacity not inspected.",
                        review_status=ReviewStatus.NEEDS_REVIEW,
                    )
                    key = "sink_extend_from_existing" if has else "sink_new_run_slab"
                elif in_unit:
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.MAY_REQUIRE_WORK,
                        observed_value=f"{int(sum(float(e.extracted_value) for e in in_unit))} sink(s) documented in this unit",
                        required_value=n,
                        evidence_ids=[e.evidence_id for e in in_unit],
                        limitations=list(in_unit[0].limitations),
                        proposed_work=[
                            f"Extend supply/drain from existing in-unit plumbing to {n} treatment sinks"
                        ],
                        verification_questions=[
                            "Where are the existing supply and drain lines and can they serve additional sinks?"
                        ],
                        uncertainty="An existing sink suggests nearby plumbing but does not prove capacity for more fixtures.",
                        review_status=ReviewStatus.NEEDS_REVIEW,
                    )
                    key = "sink_extend_from_existing"
                else:
                    ws = by_type.get("water_service_diameter", [])
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.MAY_REQUIRE_WORK,
                        required_value=n,
                        evidence_ids=[e.evidence_id for e in ws],
                        observed_value="No in-unit plumbing documented"
                        + (
                            f"; building has {ws[0].extracted_value}in water service (historical)"
                            if ws
                            else ""
                        ),
                        limitations=list(ws[0].limitations)
                        if ws
                        else ["No plumbing records found for this unit."],
                        proposed_work=[f"New supply and drain runs for {n} treatment sinks"],
                        verification_questions=[
                            "Where are the nearest drain and supply lines? Is slab cutting required?"
                        ],
                        uncertainty="Drain locations unknown; new runs may require slab work.",
                        review_status=ReviewStatus.NEEDS_REVIEW,
                    )
                    key = "sink_new_run_slab"
                assessments.append(a)
                if (
                    profile.infrastructure_requirements.minimize_plumbing_changes
                    and key == "sink_new_run_slab"
                ):
                    a.limitations.append(
                        "Owner prefers minimal plumbing changes; this property needs new runs."
                    )
                work.append(
                    _work(
                        pid,
                        key,
                        WorkCategory.PLUMBING,
                        f"Treatment sinks x{n}",
                        n,
                        ["One sink per treatment room"],
                        a.evidence_ids,
                        a.assessment_id,
                    )
                )

            elif rid.endswith("plumbing.water_service"):
                ws = by_type.get("water_service_diameter", []) + by_type.get("water_and_waste_work", [])
                sinks = by_type.get("sink_installed", [])
                support = ws + sinks
                if support:
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.MEETS,
                        observed_value="Domestic water documented (historical permits)",
                        required_value=True,
                        evidence_ids=[e.evidence_id for e in support],
                        limitations=list(support[0].limitations),
                        verification_questions=["Confirm service is active and adequate for added fixtures."],
                        uncertainty="Presence documented historically; adequacy for added fixtures not verified.",
                        review_status=ReviewStatus.NEEDS_REVIEW,
                    )
                else:
                    a = InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.UNKNOWN,
                        required_value=True,
                        verification_questions=["Is domestic water available to the space?"],
                        uncertainty="No water service evidence found.",
                    )
                assessments.append(a)

            elif rid.endswith("hvac.ventilation"):
                hv = by_type.get("hvac_tenant_improvement", []) + by_type.get("vav_boxes", [])
                unit_hv = [e for e in hv if e.evidence_id in unit_permit_evidence]
                n = profile.space_requirements.required_rooms.get("treatment", 0)
                zon = by_type.get("hvac_zonable", [])
                if zon and not unit_hv:
                    z = zon[-1]
                    key = "hvac_rezone_existing" if z.extracted_value else "hvac_new_distribution"
                    obs = (
                        "Broker reports existing HVAC can be zoned per room"
                        if z.extracted_value
                        else "Broker reports HVAC cannot be zoned per room"
                    )
                    status, ev_ids = AssessmentStatus.MAY_REQUIRE_WORK, [z.evidence_id]
                elif unit_hv:
                    key, obs = "hvac_rezone_existing", "HVAC tenant improvement documented in this unit"
                    status, ev_ids = AssessmentStatus.MAY_REQUIRE_WORK, [e.evidence_id for e in unit_hv]
                else:
                    key, obs = "hvac_new_distribution", "No unit-level HVAC work documented"
                    status, ev_ids = AssessmentStatus.UNKNOWN, [e.evidence_id for e in hv]
                a = InfrastructureAssessment(
                    project_id=prj,
                    property_id=pid,
                    requirement_id=rid,
                    status=status,
                    observed_value=obs,
                    required_value=True,
                    evidence_ids=ev_ids,
                    limitations=["Historical HVAC permits do not establish current capacity or zoning."],
                    proposed_work=[f"Supply/return to {n} treatment rooms"],
                    verification_questions=[
                        "Can existing HVAC distribution be zoned to each treatment room?"
                    ],
                    uncertainty="Zoning and capacity not verified.",
                    review_status=ReviewStatus.NEEDS_REVIEW,
                )
                assessments.append(a)
                work.append(
                    _work(
                        pid,
                        key,
                        WorkCategory.HVAC,
                        f"HVAC distribution for {n} treatment rooms",
                        n or 1,
                        [obs],
                        ev_ids,
                        a.assessment_id,
                    )
                )

            else:
                assessments.append(
                    InfrastructureAssessment(
                        project_id=prj,
                        property_id=pid,
                        requirement_id=rid,
                        status=AssessmentStatus.UNKNOWN,
                        required_value=req.required_value,
                        verification_questions=[req.verification_method],
                        uncertainty="No rule implemented for this requirement yet.",
                    )
                )

        area = listing.available_area_sqm if listing and listing.available_area_sqm else 0
        if area:
            work.append(
                _work(
                    pid,
                    "finishes",
                    WorkCategory.FINISHES,
                    "Finishes (floor/wall/ceiling refresh)",
                    round(area, 1),
                    ["Listing area"],
                    [],
                    None,
                )
            )
        work.append(
            _work(
                pid, "permits_fees", WorkCategory.PERMITS_FEES, "Permits and plan check fees", 1, [], [], None
            )
        )
        work.append(
            _work(
                pid,
                "design_professional",
                WorkCategory.DESIGN_PROFESSIONAL,
                "Architect/engineer for TI drawings",
                1,
                [],
                [],
                None,
            )
        )
        return assessments, work


def _buildout(prop: PropertyDigitalProfile) -> str | None:
    lst = prop.commercial_listing
    return lst.existing_buildout if lst else None
