"""One repository per contract. Import from here; do not construct ad hoc."""

from __future__ import annotations

from app.contracts import (
    ApprovalRequest,
    BillOfMaterials,
    BusinessExpansionProfile,
    CallSession,
    CommunicationSession,
    CostScenario,
    DesignVersion,
    InfrastructureAssessment,
    OwnerHandoffRequest,
    PermitRecord,
    ProjectContext,
    PropertyDigitalProfile,
    PropertyRankingResult,
    RenovationWorkItem,
    SiteGeometry,
    SourceEvidence,
    SupplierOffer,
    TaskRecord,
)
from app.db import models as m

from .records import RecordRepository

projects = RecordRepository(
    m.Project,
    ProjectContext,
    "project_id",
    lambda c: {"owner_id": c.owner_id, "status": c.project_status.value},
)
profiles = RecordRepository(
    m.BusinessRequirement,
    BusinessExpansionProfile,
    "profile_id",
    lambda c: {"project_id": c.project_id, "confirmation_status": c.confirmation_status.value},
)
properties = RecordRepository(
    m.Property,
    PropertyDigitalProfile,
    "property_id",
    lambda c: {
        "project_id": c.project_id,
        "normalized_address": c.property_identity.normalized_address,
        "parcel_id": c.property_identity.parcel_id,
        "data_origin": c.commercial_listing.data_origin.value if c.commercial_listing else "DERIVED",
    },
)
permits = RecordRepository(
    m.PermitRecordRow,
    PermitRecord,
    "permit_record_id",
    lambda c: {
        "property_id": c.property_id,
        "permit_number": c.permit_number,
        "permit_category": c.permit_category.value,
        "match_status": c.match_status.value,
    },
)
evidence = RecordRepository(
    m.SourceEvidenceRow,
    SourceEvidence,
    "evidence_id",
    lambda c: {
        "project_id": c.project_id,
        "property_id": c.property_id,
        "provider": c.provider,
        "observation_type": c.observation_type,
        "evidence_classification": c.evidence_classification.value,
        "data_origin": c.data_origin.value,
    },
)
assessments = RecordRepository(
    m.InfrastructureAssessmentRow,
    InfrastructureAssessment,
    "assessment_id",
    lambda c: {
        "project_id": c.project_id,
        "property_id": c.property_id,
        "requirement_id": c.requirement_id,
        "status": c.status.value,
    },
)
work_items = RecordRepository(
    m.RenovationWorkItemRow,
    RenovationWorkItem,
    "work_item_id",
    lambda c: {"property_id": c.property_id, "category": c.category.value},
)
cost_scenarios = RecordRepository(
    m.CostScenarioRow,
    CostScenario,
    "scenario_id",
    lambda c: {"project_id": c.project_id, "property_id": c.property_id, "long_term_cost": c.long_term_cost},
)
rankings = RecordRepository(
    m.PropertyRanking,
    PropertyRankingResult,
    "ranking_id",
    lambda c: {"project_id": c.project_id, "ranking_version": c.ranking_version},
)
geometries = RecordRepository(
    m.SiteGeometryRow,
    SiteGeometry,
    "site_geometry_id",
    lambda c: {"property_id": c.property_id, "scale_verified": c.scale_verified},
)
designs = RecordRepository(
    m.DesignVersionRow,
    DesignVersion,
    "design_version_id",
    lambda c: {
        "project_id": c.project_id,
        "property_id": c.property_id,
        "parent_version_id": c.parent_version_id,
        "version_number": c.version_number,
        "approval_status": c.approval_status.value,
        "valid": c.validation_report.valid,
    },
)
boms = RecordRepository(
    m.BillOfMaterialsRow,
    BillOfMaterials,
    "bom_id",
    lambda c: {
        "project_id": c.project_id,
        "design_version_id": c.design_version_id,
        "status": c.status.value,
    },
)
offers = RecordRepository(
    m.SupplierOfferRow,
    SupplierOffer,
    "offer_id",
    lambda c: {"bom_item_id": c.bom_item_id, "supplier": c.supplier},
)
approvals = RecordRepository(
    m.Approval,
    ApprovalRequest,
    "approval_id",
    lambda c: {
        "project_id": c.project_id,
        "status": c.approval_status.value,
        "action_type": c.action_type.value,
        "idempotency_key": c.idempotency_key,
    },
)
communications = RecordRepository(
    m.Communication,
    CommunicationSession,
    "communication_id",
    lambda c: {"project_id": c.project_id, "channel": c.channel.value, "state": c.state.value},
)
calls = RecordRepository(
    m.CallSessionRow,
    CallSession,
    "call_id",
    lambda c: {"project_id": c.project_id, "provider_call_id": c.provider_call_id, "state": c.state.value},
)
handoffs = RecordRepository(
    m.OwnerHandoffRequestRow,
    OwnerHandoffRequest,
    "handoff_request_id",
    lambda c: {"project_id": c.project_id, "call_id": c.call_id, "status": c.status.value},
)
tasks = RecordRepository(
    m.Task,
    TaskRecord,
    "task_id",
    lambda c: {"project_id": c.project_id, "task_type": c.task_type.value, "status": c.status.value},
)
