"""DesignService: SiteGeometry + room program -> versioned DesignVersions; revisions never overwrite.

Interface owned by the foundation; solver internals by property-spatial; interior/finishes by frontend-product.
"""

from __future__ import annotations

from uuid import UUID

from sqlalchemy import func, select
from sqlalchemy.orm import Session

from app.contracts import (
    ActorType,
    DataOrigin,
    DesignApprovalStatus,
    DesignRevisionRequest,
    DesignVersion,
    EventType,
    GeometrySource,
    LayoutGenerationRequest,
    RevisionStatus,
    RoomRequirement,
    SiteGeometry,
)
from app.core.container import Container
from app.core.errors import ConflictError, NotFoundError
from app.db import models as m
from app.domains.design.geometry.framing import normalize_plan
from app.domains.design.geometry.solver import generate_candidates
from app.domains.design.interior.finishes import finish_specifications
from app.domains.requirements.templates import SQM_PER_ROOM
from app.repositories import registry as repos

ROOM_MIN_WIDTH = {"treatment": 2.8, "reception": 3.0, "storage": 1.5}


def room_requirements_from_profile(
    rooms: dict[str, int], plumbing_rooms: set[str] | None = None
) -> list[RoomRequirement]:
    plumbing_rooms = plumbing_rooms or {"treatment"}
    return [
        RoomRequirement(
            room_type=k,
            count=v,
            min_area_sqm=round(SQM_PER_ROOM.get(k, 9.0) * 0.9, 1),
            min_width_m=ROOM_MIN_WIDTH.get(k, 2.4),
            needs_plumbing=k in plumbing_rooms,
        )
        for k, v in rooms.items()
        if v > 0
    ]


class DesignService:
    def __init__(self, c: Container):
        self.c = c

    def site_geometry(self, db: Session, property_id: UUID) -> SiteGeometry:
        rows = repos.geometries.list(db, property_id=property_id)
        if rows:
            return rows[-1]
        prop = repos.properties.require(db, property_id)
        for asset in prop.interior_assets:
            if asset.has_measured_geometry:
                doc = self.c.matterport.get_floor_geometry(asset.asset_id)
                if doc:
                    live = doc.get("data_origin") == "LIVE"
                    if live:
                        doc = normalize_plan(doc)
                    geo = SiteGeometry(
                        property_id=property_id,
                        source_id=asset.asset_id,
                        geometry_source=GeometrySource(doc.get("geometry_source", "FIXTURE")),
                        data_origin=DataOrigin.LIVE if live else DataOrigin.FIXTURE_SYNTHETIC,
                        coordinate_reference=doc.get("coordinate_reference", "local_floor_origin"),
                        floor_boundary=doc["floor_boundary"],
                        fixed_obstacles=doc.get("fixed_obstacles", []),
                        fixed_walls=[tuple(tuple(p) for p in w) for w in doc.get("fixed_walls", [])],
                        entrances=doc.get("entrances", []),
                        exits=doc.get("exits", []),
                        windows=doc.get("windows", []),
                        fixture_locations=doc.get("fixture_locations", []),
                        scale_verified=bool(doc.get("scale_verified", False)),
                        measurement_uncertainty_m=doc.get("measurement_uncertainty_m"),
                    )
                    repos.geometries.add(db, geo)
                    prop = prop.model_copy(update={"geometry_reference": geo.site_geometry_id})
                    repos.properties.upsert(db, prop)
                    return geo
        raise NotFoundError(
            "no measured geometry available for this property", error_code="GEOMETRY_UNAVAILABLE"
        )

    def _next_version(self, db: Session, property_id: UUID) -> int:
        return (
            db.scalar(
                select(func.max(m.DesignVersionRow.version_number)).where(
                    m.DesignVersionRow.property_id == property_id
                )
            )
            or 0
        ) + 1

    def _persist(self, db: Session, dv: DesignVersion) -> None:
        repos.designs.add(db, dv)
        for r in dv.room_polygons:
            db.add(
                m.Room(
                    id=__import__("uuid").uuid4(),
                    design_version_id=dv.design_version_id,
                    room_key=r.room_id,
                    room_type=r.room_type,
                    area_sqm=r.area_sqm,
                    polygon=[list(p) for p in r.polygon],
                )
            )
        db.flush()

    def generate(
        self,
        db: Session,
        project_id: UUID,
        req: LayoutGenerationRequest,
        correlation_id: UUID,
        parent: DesignVersion | None = None,
        revision_id: UUID | None = None,
    ) -> list[DesignVersion]:
        site = self.site_geometry(db, req.property_id)
        cands = generate_candidates(site, req.room_requirements, req.preferences, req.candidate_count)
        created: list[DesignVersion] = []
        for cand in cands:
            if not cand.report.valid:
                self.c.events.emit(
                    db,
                    event_type=EventType.LAYOUT_VALIDATION_FAILED,
                    project_id=project_id,
                    actor_type=ActorType.AGENT,
                    actor_id="floor-plan-architect",
                    correlation_id=correlation_id,
                    idempotency_key=f"layout-failed:{req.property_id}:{correlation_id}:{cand.name}",
                    payload={
                        "design_version_id": None,
                        "property_id": str(req.property_id),
                        "valid": False,
                        "issue_codes": [i.code for i in cand.report.issues],
                    },
                )
                continue
            dv = DesignVersion(
                project_id=project_id,
                property_id=req.property_id,
                parent_version_id=parent.design_version_id if parent else None,
                version_number=self._next_version(db, req.property_id),
                geometry_id=site.site_geometry_id,
                room_polygons=cand.rooms,
                finish_specifications=finish_specifications(cand.rooms, req.preferences.style),
                validation_report=cand.report,
                created_from_revision_request=revision_id,
                approval_status=DesignApprovalStatus.PROPOSED,
            )
            self._persist(db, dv)
            created.append(dv)
            self.c.events.emit(
                db,
                event_type=EventType.LAYOUT_VERSION_CREATED,
                project_id=project_id,
                actor_type=ActorType.AGENT,
                actor_id="floor-plan-architect",
                correlation_id=correlation_id,
                idempotency_key=f"layout:{dv.design_version_id}",
                payload={
                    "design_version_id": str(dv.design_version_id),
                    "property_id": str(req.property_id),
                    "valid": True,
                    "issue_codes": [],
                },
            )
        return created

    def revise(
        self, db: Session, rev: DesignRevisionRequest, correlation_id: UUID
    ) -> tuple[DesignRevisionRequest, list[DesignVersion]]:
        base = repos.designs.require(db, rev.base_design_version_id)
        if base.approval_status == DesignApprovalStatus.SUPERSEDED:
            raise ConflictError(
                "base design version is stale (superseded)", error_code="STALE_DESIGN_VERSION"
            )
        if not rev.room_requirement_overrides:
            raise ConflictError(
                "revision needs room_requirement_overrides in the foundation build",
                error_code="REVISION_NOT_STRUCTURED",
            )
        req = LayoutGenerationRequest(
            property_id=base.property_id,
            site_geometry_id=base.geometry_id,
            room_requirements=rev.room_requirement_overrides,
            candidate_count=1,
        )
        created = self.generate(
            db, rev.project_id, req, correlation_id, parent=base, revision_id=rev.revision_request_id
        )
        status = RevisionStatus.APPLIED if created else RevisionStatus.INFEASIBLE
        # Infeasible revisions leave `base` untouched — the last valid design stays current.
        return rev.model_copy(update={"status": status}), created
