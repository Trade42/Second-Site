from __future__ import annotations

import json
import uuid

from app.contracts import BusinessExpansionProfile, PropertySearchRequest
from app.domains.feasibility.engine import FeasibilityEngine
from app.domains.finance.calculator import FinancialCalculationService
from app.domains.permits.service import PermitIntelligenceService
from app.domains.property.service import PropertyScoutService, attach_permits
from app.domains.ranking.engine import RankingEngine
from app.domains.requirements.templates import requirements_for
from app.integrations.costar import FixtureCoStarProvider
from app.integrations.la_permits import FixtureLAPermitProvider

from .conftest import FIXTURES


def demo_profile(**overrides) -> BusinessExpansionProfile:
    fx = json.loads((FIXTURES / "projects" / "facial_studio.json").read_text())["business_profile"]
    fx.update(overrides)
    return BusinessExpansionProfile(project_id=uuid.uuid4(), **fx)


def analyze_in_memory(profile: BusinessExpansionProfile, horizon: int | None = None):
    props = PropertyScoutService(FixtureCoStarProvider()).search(profile.project_id, PropertySearchRequest())
    permits = PermitIntelligenceService(FixtureLAPermitProvider())
    reqs = requirements_for(profile)
    scenarios, assessments, work_items, evidence = {}, {}, {}, {}
    out_props = []
    for p in props:
        res = permits.fetch_and_match(p.property_identity, profile.project_id)
        p = attach_permits(p, res.permits, res.datasets, "FIXTURE")
        a, w = FeasibilityEngine().evaluate(profile, reqs, p, res.evidence)
        assessments[p.property_id], work_items[p.property_id], evidence[p.property_id] = a, w, res.evidence
        scenarios[p.property_id] = FinancialCalculationService().scenario(profile, p, w, horizon)
        out_props.append(p)
    ranking = RankingEngine().rank(profile, out_props, scenarios, assessments, 1)
    by_addr = {p.property_identity.street_address: p for p in out_props}
    return by_addr, scenarios, assessments, work_items, evidence, ranking
