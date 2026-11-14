"""Generate ss-domain part 2 golden vectors (feasibility, finance, ranking, evidence validator)
from the Python implementation, which is the specification.

Run from the repository root:
    (cd apps/api && .venv/bin/python -B ../backend/tests-golden/domain/gen_domain2.py)

Pure functions and file-based fixture providers only: no database, no network. Reads fixtures/
(never copies them) and writes apps/backend/tests-golden/domain/part2_*.json.

Three kinds of vectors:
- part2_pipeline.json: the full pipeline (search -> permits -> feasibility -> cost scenario at
  36 and 120 months -> ranking -> validator) over the repository fixtures
  (fixtures/projects/facial_studio.json, fixtures/properties/la_candidates.json,
  fixtures/permits/la_permits_snapshot.json) for several business-profile variants.
- part2_cases.json: engine-level cases whose inputs are recorded in the file (profile,
  requirements, property, evidence, scenarios). They start from the fixture pipeline output and
  add SYNTHETIC broker answers, listing variants and ranking set-ups to reach every branch. All
  synthetic evidence says so in its limitations and uses data_origin FIXTURE_SYNTHETIC.
- part2_units.json: CPython numeric semantics the engines rely on (rounding, sum, division).

Normalization (identical in crates/domain/tests/common/labels.rs): freshly generated UUIDs
(assessment_id, work_item_id, scenario_id, ranking_id, evidence_id, permit_record_id and every
reference to them, including inside strings such as "work:<uuid>") become "<id:N>", numbered in
order of first appearance while walking the document with dict keys sorted; UUIDs fixed by the
inputs are kept. Non-null calculated_at / retrieved_at / last_analyzed_at become "<timestamp>".

Reproducibility: the generator replaces uuid4() behind app.contracts.common.new_id with a seeded
generator and the clock behind utcnow() / property.service with a fixed instant, so the recorded
inputs are byte-identical on every run. Engine behaviour is untouched: these values are fresh ids
and timestamps, which the comparison normalizes anyway.

Size: part2_cases.json stores the shared inputs once under "bases"; a case input value
{"$base": name} is that base, {"$base": name, "$set": {...}} replaces top-level keys of it, and
{"$base": name, "$extra": [...]} appends to a base list (expand() in crates/domain/tests/
golden_engines.rs).
"""

from __future__ import annotations

import json
import random
import re
import sys
from decimal import Decimal
from pathlib import Path
from uuid import UUID

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
sys.path.insert(0, str(REPO / "apps" / "api"))

from pydantic import ValidationError  # noqa: E402

from app.contracts import (  # noqa: E402
    BusinessExpansionProfile,
    CostScenario,
    InfrastructureAssessment,
    InfrastructureRequirement,
    PropertyDigitalProfile,
    PropertySearchRequest,
    RenovationWorkItem,
    SourceEvidence,
)
from app.domains.evidence.validator import validate_property  # noqa: E402
from app.domains.feasibility.engine import FeasibilityEngine  # noqa: E402
from app.domains.finance.calculator import FinancialCalculationService, apply_bp, work_item_totals  # noqa: E402
from app.domains.permits.service import PermitIntelligenceService  # noqa: E402
from app.domains.property.service import PropertyScoutService, attach_permits  # noqa: E402
from app.domains.ranking.engine import RankingEngine, _usd  # noqa: E402
from app.domains.requirements.templates import requirements_for  # noqa: E402
from app.integrations.costar import FixtureCoStarProvider  # noqa: E402
from app.integrations.la_permits import FixtureLAPermitProvider  # noqa: E402

import app.contracts.common as _common  # noqa: E402
import app.domains.property.service as _property_service  # noqa: E402
from datetime import UTC, datetime  # noqa: E402

_RNG = random.Random(0x45224EBD)
_common.uuid4 = lambda: UUID(int=_RNG.getrandbits(128), version=4)


class _FixedClock:
    @staticmethod
    def now(tz=None):
        return datetime(2026, 11, 14, 20, 0, 0, tzinfo=UTC)


_common.datetime = _FixedClock
_property_service.datetime = _FixedClock

OUT = HERE
PROJECT_ID = UUID("45224ebd-0000-4000-8000-0000000000a1")
PROFILE_ID = UUID("45224ebd-0000-4000-8000-0000000000a2")
# Property ids replacing the fresh ones from the search, in search order (Alhambra, Hollywood).
PROPERTY_IDS = [UUID("45224ebd-0000-4000-8000-00000000c001"), UUID("45224ebd-0000-4000-8000-00000000c002")]
HORIZONS = [36, 120]
TS_KEYS = {"calculated_at", "retrieved_at", "last_analyzed_at"}
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
SYNTHETIC = "SYNTHETIC test evidence for golden vectors: not a real broker answer."


def uid(n: int) -> UUID:
    return UUID(f"45224ebd-0000-4000-8000-{n:012x}")


def dump(name: str, value, indent: int | None = 1) -> None:
    (OUT / name).write_text(json.dumps(value, indent=indent, ensure_ascii=False, sort_keys=False) + "\n")


def uuids_in(doc) -> set[str]:
    return set(UUID_RE.findall(json.dumps(doc)))


def normalize(doc, fixed: set[str]):
    """Mirror of normalize_labels() in crates/domain/tests/common/labels.rs."""
    labels: dict[str, str] = {}

    def sub(s: str) -> str:
        def one(m):
            u = m.group(0)
            if u in fixed:
                return u
            if u not in labels:
                labels[u] = f"<id:{len(labels)}>"
            return labels[u]

        return UUID_RE.sub(one, s)

    def walk(v, key=None):
        if key in TS_KEYS and v is not None:
            return "<timestamp>"
        if isinstance(v, dict):
            return {sub(k): walk(v[k], k) for k in sorted(v)}
        if isinstance(v, list):
            return [walk(x) for x in v]
        if isinstance(v, str):
            return sub(v)
        return v

    return walk(doc)


def j(model) -> dict:
    return json.loads(model.model_dump_json())


def jl(models) -> list:
    return [j(m) for m in models]


def demo_profile(**overrides) -> BusinessExpansionProfile:
    fx = json.loads((REPO / "fixtures" / "projects" / "facial_studio.json").read_text())["business_profile"]
    fx.update(overrides)
    doc = {"project_id": str(PROJECT_ID), "profile_id": str(PROFILE_ID), **fx}
    return BusinessExpansionProfile.model_validate_json(json.dumps(doc))


def error_of(exc: Exception) -> dict:
    out = {"error": type(exc).__name__, "message": str(exc).splitlines()[0][:300]}
    if isinstance(exc, ValidationError):
        out["title"] = exc.title
        out["errors"] = [{"type": e["type"], "loc": list(e["loc"])} for e in exc.errors()]
    return out


def pipeline(profile: BusinessExpansionProfile):
    """analyze_in_memory (apps/api/tests/helpers.py) with property ids fixed after the search."""
    props = PropertyScoutService(FixtureCoStarProvider()).search(profile.project_id, PropertySearchRequest())
    assert len(props) == len(PROPERTY_IDS), len(props)
    permits = PermitIntelligenceService(FixtureLAPermitProvider())
    reqs = requirements_for(profile)
    rows = []
    for p, pid in zip(props, PROPERTY_IDS):
        old = p.property_id
        p = p.model_copy(
            update={"property_id": pid, "property_identity": p.property_identity.model_copy(update={"property_id": pid})}
        )
        assert str(old) not in p.model_dump_json()
        res = permits.fetch_and_match(p.property_identity, profile.project_id)
        p = attach_permits(p, res.permits, res.datasets, "FIXTURE")
        a, w = FeasibilityEngine().evaluate(profile, reqs, p, res.evidence)
        rows.append((p, res.evidence, a, w))
    return reqs, rows


def pipeline_case(name: str, overrides: dict) -> dict:
    profile = demo_profile(**overrides)
    _reqs, rows = pipeline(profile)
    out = {"properties": {}, "rankings": {}}
    scenarios_by_h = {}
    for p, ev, a, w in rows:
        sc = {str(h): FinancialCalculationService().scenario(profile, p, w, h) for h in HORIZONS}
        sc["profile"] = FinancialCalculationService().scenario(profile, p, w)
        for h, s in sc.items():
            scenarios_by_h.setdefault(h, {})[p.property_id] = s
        out["properties"][p.property_identity.street_address] = {
            "property_id": str(p.property_id),
            "evidence": jl(ev),
            "assessments": jl(a),
            "work_items": jl(w),
            "scenarios": {h: j(s) for h, s in sc.items()},
            "problems": validate_property(p, ev, a),
        }
    props = [r[0] for r in rows]
    assessments = {r[0].property_id: r[2] for r in rows}
    for h, scs in scenarios_by_h.items():
        out["rankings"][h] = j(RankingEngine().rank(profile, props, scs, assessments, 1))
    fixed = {str(PROJECT_ID), str(PROFILE_ID), *map(str, PROPERTY_IDS)}
    return {
        "name": name,
        "profile_overrides": overrides,
        "fixed_property_ids": [str(x) for x in PROPERTY_IDS],
        "output": normalize(out, fixed),
    }


# ---- engine-level cases -------------------------------------------------------------------------


def evidence(prop: PropertyDigitalProfile, n: int, kind: str, value, cls="OWNER_REPORTED", **kw) -> dict:
    return {
        "evidence_id": str(uid(n)),
        "project_id": str(prop.project_id),
        "property_id": str(kw.pop("property_id", prop.property_id)),
        "provider": "broker_call",
        "source_record_id": str(uid(0xCA11)),
        "data_origin": "FIXTURE_SYNTHETIC",
        "retrieved_at": "2026-11-14T20:00:00Z",
        "observation_type": kind,
        "extracted_value": value,
        "unit": kw.pop("unit", None),
        "evidence_classification": cls,
        "limitations": [SYNTHETIC, "not independently verified."],
        **kw,
    }


def with_listing(prop: PropertyDigitalProfile, **changes) -> dict:
    d = j(prop)
    if changes.pop("_no_listing", False):
        d["commercial_listing"] = None
    else:
        d["commercial_listing"].update(changes)
    return d


def run_feasibility_case(name, profile_overrides, prop_doc, evidence_docs, req_edit=None, note=""):
    profile = demo_profile(**profile_overrides)
    reqs = [j(r) for r in requirements_for(profile)]
    if req_edit:
        reqs = req_edit(reqs)
    inputs = {
        "profile_overrides": profile_overrides,
        "requirements": reqs,
        "property": prop_doc,
        "evidence": evidence_docs,
    }
    # Parse exactly as the Rust side does: from the recorded JSON.
    prop = PropertyDigitalProfile.model_validate_json(json.dumps(prop_doc))
    ev = [SourceEvidence.model_validate_json(json.dumps(e)) for e in evidence_docs]
    rq = [InfrastructureRequirement.model_validate_json(json.dumps(r)) for r in reqs]
    out: dict = {}
    try:
        a, w = FeasibilityEngine().evaluate(profile, rq, prop, ev)
    except Exception as exc:  # noqa: BLE001 - the error kind is the vector
        out["error"] = error_of(exc)
    else:
        out["assessments"] = jl(a)
        out["work_items"] = jl(w)
        out["scenarios"] = {str(h): j(FinancialCalculationService().scenario(profile, prop, w, h)) for h in HORIZONS}
        out["problems"] = validate_property(prop, ev, a)
    fixed = uuids_in(inputs) | {str(PROJECT_ID), str(PROFILE_ID)}
    return {"name": name, "note": note, "kind": "feasibility", "input": inputs, "output": normalize(out, fixed)}


def run_finance_case(name, profile_overrides, prop_doc, work_docs, horizon, design_version_id=None, note=""):
    profile = demo_profile(**profile_overrides)
    prop = PropertyDigitalProfile.model_validate_json(json.dumps(prop_doc))
    work = [RenovationWorkItem.model_validate_json(json.dumps(w)) for w in work_docs]
    inputs = {
        "profile_overrides": profile_overrides,
        "property": prop_doc,
        "work_items": work_docs,
        "horizon": horizon,
        "design_version_id": design_version_id,
    }
    out: dict = {}
    try:
        s = FinancialCalculationService().scenario(
            profile, prop, work, horizon, UUID(design_version_id) if design_version_id else None
        )
    except Exception as exc:  # noqa: BLE001
        out["error"] = error_of(exc)
    else:
        out["scenario"] = j(s)
    fixed = uuids_in(inputs) | {str(PROJECT_ID), str(PROFILE_ID)}
    return {"name": name, "note": note, "kind": "finance", "input": inputs, "output": normalize(out, fixed)}


def run_ranking_case(name, profile_overrides, prop_docs, scenario_docs, assessment_docs, version=1, note=""):
    """scenario_docs / assessment_docs: lists of [property_id, value] pairs (order kept)."""
    profile = demo_profile(**profile_overrides)
    props = [PropertyDigitalProfile.model_validate_json(json.dumps(p)) for p in prop_docs]
    scenarios = {UUID(k): CostScenario.model_validate_json(json.dumps(v)) for k, v in scenario_docs}
    assessments = {
        UUID(k): [InfrastructureAssessment.model_validate_json(json.dumps(a)) for a in v] for k, v in assessment_docs
    }
    inputs = {
        "profile_overrides": profile_overrides,
        "properties": prop_docs,
        "scenarios": scenario_docs,
        "assessments": assessment_docs,
        "ranking_version": version,
    }
    out: dict = {}
    try:
        out["ranking"] = j(RankingEngine().rank(profile, props, scenarios, assessments, version))
    except Exception as exc:  # noqa: BLE001
        out["error"] = error_of(exc)
    fixed = uuids_in(inputs) | {str(PROJECT_ID), str(PROFILE_ID)}
    return {"name": name, "note": note, "kind": "ranking", "input": inputs, "output": normalize(out, fixed)}


def run_validator_case(name, prop_doc, evidence_docs, assessment_docs, note=""):
    prop = PropertyDigitalProfile.model_validate_json(json.dumps(prop_doc))
    ev = [SourceEvidence.model_validate_json(json.dumps(e)) for e in evidence_docs]
    a = [InfrastructureAssessment.model_validate_json(json.dumps(x)) for x in assessment_docs]
    inputs = {"property": prop_doc, "evidence": evidence_docs, "assessments": assessment_docs}
    return {"name": name, "note": note, "kind": "validator", "input": inputs, "output": {"problems": validate_property(prop, ev, a)}}


def compress(v, bases: dict):
    """Replace values equal (or close) to a base by a reference; see the module docstring."""
    for name, b in bases.items():
        if v == b:
            return {"$base": name}
    for name, b in bases.items():
        if isinstance(v, dict) and isinstance(b, dict) and v.keys() == b.keys():
            diff = {k: v[k] for k in v if v[k] != b[k]}
            if len(diff) <= 3:
                return {"$base": name, "$set": diff}
        if isinstance(v, list) and isinstance(b, list) and len(v) > len(b) and v[: len(b)] == b:
            return {"$base": name, "$extra": v[len(b):]}
    if isinstance(v, dict):
        return {k: compress(x, bases) for k, x in v.items()}
    if isinstance(v, list):
        return [compress(x, bases) for x in v]
    return v


def expand(v, bases: dict):
    if isinstance(v, dict) and "$base" in v:
        b = json.loads(json.dumps(bases[v["$base"]]))
        if "$set" in v:
            b.update(v["$set"])
        if "$extra" in v:
            b = b + v["$extra"]
        return b
    if isinstance(v, dict):
        return {k: expand(x, bases) for k, x in v.items()}
    if isinstance(v, list):
        return [expand(x, bases) for x in v]
    return v


def cases() -> tuple[list[dict], dict]:
    profile = demo_profile()
    _reqs, rows = pipeline(profile)
    (alh, alh_ev, alh_a, alh_w), (hw, hw_ev, hw_a, hw_w) = rows
    alh_doc, hw_doc = j(alh), j(hw)
    alh_evd, hw_evd = jl(alh_ev), jl(hw_ev)
    out = []

    def f(name, prop_doc, base_ev, extra, overrides=None, req_edit=None, note=""):
        out.append(run_feasibility_case(name, overrides or {}, prop_doc, base_ev + extra, req_edit, note))

    # Broker-reported evidence (OWNER_REPORTED, synthetic).
    f("alh_broker_200A", alh_doc, alh_evd, [evidence(alh, 1, "tenant_subpanel_amps", 200, unit="A")],
      note="reported >= required: MEETS with an explicit 'not verified' limitation")
    f("alh_broker_60A", alh_doc, alh_evd, [evidence(alh, 1, "tenant_subpanel_amps", 60, unit="A")])
    f("alh_broker_amps_string_float_tie", alh_doc, alh_evd, [
        evidence(alh, 1, "tenant_subpanel_amps", "150", unit="A"),
        evidence(alh, 2, "tenant_subpanel_amps", 150.0, unit="A"),
        evidence(alh, 3, "tenant_subpanel_amps", 120, unit="A"),
    ], note="max() keeps the first of equal values; float('150') parses")
    f("alh_broker_plumbing_yes_then_no", alh_doc, alh_evd, [
        evidence(alh, 1, "in_unit_plumbing", True), evidence(alh, 2, "in_unit_plumbing", False)],
      note="the last answer wins; new runs + owner preference limitation")
    f("alh_broker_plumbing_yes_hvac_yes", alh_doc, alh_evd, [
        evidence(alh, 1, "in_unit_plumbing", True), evidence(alh, 2, "hvac_zonable", True)])
    f("alh_broker_plumbing_no_hvac_no_minimize_off", alh_doc, alh_evd, [
        evidence(alh, 1, "in_unit_plumbing", "no"), evidence(alh, 2, "hvac_zonable", 0)],
      overrides={"infrastructure_requirements": {"minimize_plumbing_changes": False, "sinks_per_treatment_room": 1}},
      note="truthiness: 'no' is truthy, 0 is falsy")
    f("hw_broker_ignored_where_unit_permits_exist", hw_doc, hw_evd, [
        evidence(hw, 1, "in_unit_plumbing", False), evidence(hw, 2, "hvac_zonable", False),
        evidence(hw, 3, "tenant_subpanel_amps", 90, unit="A")])
    f("alh_foreign_and_unusable_evidence", alh_doc, alh_evd, [
        evidence(alh, 1, "tenant_subpanel_amps", 400, property_id=uid(0xBEEF)),
        evidence(alh, 2, "tenant_subpanel_amps", 400, cls="UNKNOWN"),
        evidence(alh, 3, "tenant_subpanel_amps", 400, cls="DERIVED_ESTIMATE"),
        evidence(alh, 4, "tenant_subpanel_amps", 400, cls="CONFLICTING"),
        {**evidence(alh, 5, "tenant_subpanel_amps", 400), "property_id": None},
    ], note="other property, null property and non-USABLE classifications are ignored")
    no_permits = {**alh_doc, "permit_records": []}
    f("alh_no_permits_other_classifications", no_permits, [], [
        evidence(alh, 1, "water_service_diameter", "3/4", cls="PROFESSIONALLY_VERIFIED", unit="in"),
        evidence(alh, 2, "sink_installed", 2, cls="VISUALLY_OBSERVED"),
        evidence(alh, 3, "electrical_service_amps", 400.0, cls="PROFESSIONALLY_VERIFIED", unit="A"),
        evidence(alh, 4, "electrical_service_amps", 400, cls="GEOMETRY_MEASURED", unit="A"),
        evidence(alh, 5, "vav_boxes", 3, cls="DOCUMENTED_HISTORICAL"),
    ])
    f("alh_no_evidence_at_all", no_permits, [], [])
    hw_unit_sinks = [e for e in hw_evd]
    f("hw_unit_sinks_summed", hw_doc, hw_unit_sinks, [], note="baseline Hollywood: in-unit sink sum")
    # Listing variants.
    f("listing_area_none", with_listing(alh, available_area_sqm=None), alh_evd, [])
    f("listing_area_tiny_fails", with_listing(alh, available_area_sqm=50.0), alh_evd, [])
    f("listing_area_round_half", with_listing(alh, available_area_sqm=148.65), alh_evd, [])
    f("listing_area_round_to_whole", with_listing(alh, available_area_sqm=139.96), alh_evd, [])
    f("listing_area_rounds_to_zero_error", with_listing(alh, available_area_sqm=0.04), alh_evd, [],
      note="round(0.04, 1) == 0.0 -> quantity 0 -> Pydantic rejects the finishes work item")
    f("listing_unknowns", with_listing(hw, asking_rent_monthly=None, nnn_charges_monthly=None, security_deposit=None,
                                       existing_buildout=None), hw_evd, [])
    f("listing_nnn_estimate_empty_buildout", with_listing(hw, nnn_is_estimate=True, security_deposit=0,
                                                          existing_buildout=""), hw_evd, [])
    f("no_listing", with_listing(alh, _no_listing=True), alh_evd, [])
    # Requirement variants.

    def odd_reqs(reqs):
        for r in reqs:
            if r["requirement_id"].endswith("space.enclosed_rooms"):
                r["required_value"] = " 4 "
            if r["requirement_id"].endswith("space.min_area"):
                r["required_value"] = "100.5"
        reqs.append({
            "requirement_id": "facial_studio.zoning.use_permitted", "business_type": "facial_studio",
            "requirement_category": "ZONING", "description": "Zoning permits a personal-services use",
            "required_value": None, "required_unit": None, "severity": "HARD",
            "verification_method": "Zoning administrator or land-use attorney confirms the use",
        })
        reqs.append({
            "requirement_id": "prefix.space.min_area", "business_type": "facial_studio",
            "requirement_category": "SPACE", "description": "suffix match", "required_value": 200,
            "required_unit": "m2", "severity": "SOFT", "verification_method": "x",
        })
        return reqs

    f("requirements_odd_values_and_unknown_rule", alh_doc, alh_evd, [], req_edit=odd_reqs)

    def no_value(reqs):
        for r in reqs:
            if r["requirement_id"].endswith("electrical.tenant_service"):
                r["required_value"] = None
        return reqs

    f("error_required_value_none_with_broker_amps", alh_doc, alh_evd,
      [evidence(alh, 1, "tenant_subpanel_amps", 200)], req_edit=no_value)
    f("required_value_none_without_broker_amps", alh_doc, alh_evd, [], req_edit=no_value,
      note="float(None) is only evaluated when broker amps exist")
    f("error_bad_broker_amps", alh_doc, alh_evd, [evidence(alh, 1, "tenant_subpanel_amps", "lots")])
    f("error_null_broker_amps", alh_doc, alh_evd, [evidence(alh, 1, "tenant_subpanel_amps", None)])
    f("error_zero_rooms", alh_doc, alh_evd, [], overrides={"space_requirements": {"required_rooms": {"treatment": 0}}})
    f("retail_profile", alh_doc, alh_evd, [], overrides={
        "business_type": "retail", "space_requirements": {"required_rooms": {"office": 3}, "min_area_sqm": 40.0}})
    f("facial_studio_without_treatment_rooms", hw_doc, hw_evd, [], overrides={
        "space_requirements": {"required_rooms": {"office": 2, "storage": 1}},
        "infrastructure_requirements": {"min_electrical_service_amps": 800, "sinks_per_treatment_room": 2}})
    f("med_spa_two_sinks_per_room", hw_doc, hw_evd, [evidence(hw, 1, "tenant_subpanel_amps", 800.0)], overrides={
        "business_type": "med_spa",
        "space_requirements": {"required_rooms": {"treatment": 4, "reception": 1}},
        "infrastructure_requirements": {"min_electrical_service_amps": 800, "sinks_per_treatment_room": 2}})

    # Finance-only cases (work items recorded as inputs).
    alh_work = jl(alh_w)
    out.append(run_finance_case("duplicate_work_item", {}, alh_doc, alh_work + alh_work[:1], None))
    out.append(run_finance_case("horizon_zero_uses_profile", {}, alh_doc, alh_work, 0))
    out.append(run_finance_case("horizon_negative_error", {}, alh_doc, alh_work, -1))
    out.append(run_finance_case("horizon_240_with_design_version", {}, hw_doc, jl(hw_w), 240,
                                design_version_id=str(uid(0xD1))))
    out.append(run_finance_case("no_work_items_no_listing", {"space_requirements": {"required_rooms": {"office": 1}}},
                                with_listing(alh, _no_listing=True), [], 12))
    odd_work = []
    for n, (q, lo, ex, hi) in enumerate([
        ("0.5", 1, 1, 1), ("2.5", 1, 3, 5), ("1.5", 1, 3, 5), ("139.4", 9001, 15001, 25001),
        ("12.345", 99, 101, 1001), ("0.001", 499, 500, 1500), ("1000000.5", 1, 3, 7), ("3", 0, 0, 0),
    ]):
        odd_work.append({
            "work_item_id": str(uid(0x100 + n)), "property_id": str(alh.property_id), "category": "OTHER",
            "description": f"rounding probe {q}", "quantity": q, "unit": "u", "unit_cost_low": lo,
            "unit_cost_expected": ex, "unit_cost_high": hi,
            "evidence_ids": [str(uid(0x200 + n % 3))],
        })
    out.append(run_finance_case("rounding_probes_and_evidence_dedupe", {}, alh_doc, odd_work, 36))

    # Ranking set-ups.
    s36 = {r[0].property_id: FinancialCalculationService().scenario(profile, r[0], r[3], 36) for r in rows}
    hw_copy = {**hw_doc, "property_id": str(uid(0xC000)),
               "property_identity": {**hw_doc["property_identity"], "property_id": str(uid(0xC000))}}
    sc_copy = {**j(s36[hw.property_id]), "property_id": str(uid(0xC000)), "scenario_id": str(uid(0x5C0))}
    asmt = [[str(alh.property_id), jl(alh_a)], [str(hw.property_id), jl(hw_a)]]
    scs = [[str(alh.property_id), j(s36[alh.property_id])], [str(hw.property_id), j(s36[hw.property_id])]]
    out.append(run_ranking_case("tie_break_by_property_id", {}, [hw_doc, hw_copy, alh_doc],
                                scs + [[str(uid(0xC000)), sc_copy]], asmt + [[str(uid(0xC000)), jl(hw_a)]],
                                note="equal totals are ordered by str(property_id)"))
    zero_sc = {**j(s36[alh.property_id]), "opening_cost_low": 0, "opening_cost_expected": 0, "opening_cost_high": 0,
               "upfront_cash_expected": 0, "monthly_operating_cost": 0, "long_term_cost": 0, "contingency": 0}
    out.append(run_ranking_case("zero_cost_and_missing_scenario", {}, [alh_doc, hw_doc],
                                [[str(alh.property_id), zero_sc]], asmt,
                                note="long_term_cost 0 is excluded from cost scores; a property without a scenario"))
    out.append(run_ranking_case("no_limits_equal_weights", {"monthly_rent_limit": None, "opening_capital": None,
                                "ranking_priorities": {"total_cost": 1, "infrastructure_fit": 1,
                                                       "evidence_certainty": 1, "comparison_horizon_months": 36}},
                                [alh_doc, hw_doc], scs, asmt))
    out.append(run_ranking_case("skewed_weights_rent_limit", {"monthly_rent_limit": 500000,
                                "ranking_priorities": {"total_cost": 0.1, "infrastructure_fit": 0.7,
                                                       "evidence_certainty": 0.25, "comparison_horizon_months": 60}},
                                [alh_doc, hw_doc], scs, asmt))
    out.append(run_ranking_case("assessments_missing", {}, [alh_doc, hw_doc], scs, [[str(alh.property_id), jl(alh_a)]],
                                version=7))
    cheap = {**alh_doc, "property_id": str(uid(0xC003)),
             "property_identity": {**alh_doc["property_identity"], "property_id": str(uid(0xC003)),
                                   "street_address": "1 SYNTHETIC TEST ST",
                                   "normalized_address": "1 SYNTHETIC TEST ST, LOS ANGELES, CA 90032"},
             "commercial_listing": {**alh_doc["commercial_listing"], "asking_rent_monthly": 100000}}
    cheap_sc = {**j(s36[alh.property_id]), "property_id": str(uid(0xC003)), "scenario_id": str(uid(0x5C3)),
                "long_term_cost": 99000000}
    out.append(run_ranking_case("lower_rent_than_two_others", {"opening_capital": None},
                                [alh_doc, hw_doc, cheap], scs + [[str(uid(0xC003)), cheap_sc]],
                                asmt + [[str(uid(0xC003)), jl(alh_a)]]))
    failing = [dict(a) for a in jl(alh_a)]
    failing[0] = {**failing[0], "status": "FAILS"}
    unknown_hard = [dict(a) for a in jl(hw_a)]
    unknown_hard[0] = {**unknown_hard[0], "status": "UNKNOWN", "observed_value": None,
                       "uncertainty": "Available area not provided by any source."}
    out.append(run_ranking_case("hard_fail_and_unknown", {}, [alh_doc, hw_doc], scs,
                                [[str(alh.property_id), failing], [str(hw.property_id), unknown_hard]]))
    out.append(run_ranking_case("error_zero_priorities", {"ranking_priorities": {
        "total_cost": 0, "infrastructure_fit": 0, "evidence_certainty": 0, "comparison_horizon_months": 36}},
        [alh_doc, hw_doc], scs, asmt))
    out.append(run_ranking_case("error_version_zero", {}, [alh_doc, hw_doc], scs, asmt, version=0))
    out.append(run_ranking_case("empty", {}, [], [], []))

    # Evidence validator.
    bad_a = jl(alh_a)
    bad_a[0]["evidence_ids"] = [str(uid(0xDEAD)), str(uid(0xBEEF))]
    water = next(a for a in bad_a if a["requirement_id"].endswith("plumbing.water_service"))
    water["limitations"] = []
    costar_listing = with_listing(alh, provider="CoStar", data_origin="FIXTURE_REAL")
    out.append(run_validator_case("validator_all_problems", costar_listing, alh_evd, bad_a))
    out.append(run_validator_case("validator_live_costar_ok", with_listing(alh, provider="costar", data_origin="LIVE"),
                                  alh_evd, jl(alh_a)))
    mixed = jl(hw_a)
    w2 = next(a for a in mixed if a["requirement_id"].endswith("plumbing.water_service"))
    w2["limitations"] = []
    w2["evidence_ids"] = w2["evidence_ids"] + [str(uid(1))]
    out.append(run_validator_case("validator_meets_mixed_evidence", hw_doc,
                                  hw_evd + [evidence(hw, 1, "sink_installed", 1)], mixed,
                                  note="MEETS is flagged only when every evidence id is historical"))
    bases = {
        "alhambra_property": alh_doc, "hollywood_property": hw_doc,
        "alhambra_evidence": alh_evd, "hollywood_evidence": hw_evd,
        "alhambra_assessments": jl(alh_a), "hollywood_assessments": jl(hw_a),
        "alhambra_work_items": alh_work, "hollywood_work_items": jl(hw_w),
        "alhambra_scenario_36": j(s36[alh.property_id]), "hollywood_scenario_36": j(s36[hw.property_id]),
    }
    for c in out:
        full = c["input"]
        c["input"] = compress(full, bases)
        assert expand(c["input"], bases) == full, c["name"]
    return out, bases


def units() -> dict:
    rnd = random.Random(45224)
    sums = []
    for _ in range(300):
        n = rnd.randint(1, 6)
        xs = [rnd.choice([rnd.uniform(-1, 1), rnd.uniform(0, 1e6), rnd.randint(0, 9) * 0.05, rnd.random() * 1e-9,
                          0.1, 0.3, 0.6, 1e16, -1e16]) for _ in range(n)]
        sums.append({"xs": xs, "sum": sum(xs)})
    divs = []
    for a, b in [(1, 3), (2, 3), (35609650, 37738350), (10769650, 20818350), (9007199254740993, 3),
                 (9223372036854775807, 7), (-9007199254740993, 2), (1, 9223372036854775807), (3, 6), (0, 5),
                 (123456789012345678, 987654321), (2 ** 53 + 1, 2 ** 53 + 3)]:
        divs.append({"a": a, "b": b, "q": a / b})
    for _ in range(200):
        a, b = rnd.randint(1, 10 ** 12), rnd.randint(1, 10 ** 12)
        divs.append({"a": a, "b": b, "q": a / b})
    for _ in range(100):
        a, b = rnd.randint(1, 2 ** 63 - 1), rnd.randint(1, 2 ** 63 - 1)
        divs.append({"a": a, "b": b, "q": a / b})
    rounds = []
    for x in [0.5 / 1.0, 0.3 / 1.1, 0.2 / 0.9, 1 / 3, 2 / 3, 0.6 * 4 / 6, 1 - 2 / 6, 0.0000005, 0.0000015,
              0.0000025, 0.1234565, 0.786667, 0.6451300000000001, -0.0000005]:
        rounds.append({"x": x, "round6": round(x, 6), "round0": round(x)})
    for _ in range(200):
        x = rnd.uniform(-2, 2)
        rounds.append({"x": x, "round6": round(x, 6), "round0": round(x)})
    totals = []
    for q in ["1", "3", "5", "139.4", "148.6", "0.5", "2.5", "12.345", "0.001", "1000000.5", "7.25", "0.125"]:
        for lo, ex, hi in [(0, 0, 0), (1, 1, 1), (1, 3, 5), (9001, 15001, 25001), (250000, 450000, 800000),
                           (0, 600000, 1800000), (99, 101, 1001)]:
            w = RenovationWorkItem(property_id=uid(1), category="OTHER", description="x", quantity=Decimal(q),
                                   unit="u", unit_cost_low=lo, unit_cost_expected=ex, unit_cost_high=hi)
            totals.append({"quantity": q, "costs": [lo, ex, hi], "totals": list(work_item_totals(w))})
    bps = []
    for amount in list(range(-120, 121, 10)) + [0, 1, 7, 29, 30, 50, 70, 1234567, 20818350, 10 ** 15 + 30,
                                                9223372036854775807 // 1500, -9223372036854775807 // 1500]:
        for bp in [1500, 1, 5000, 9999, 2500, 10000]:
            bps.append({"amount": amount, "bp": bp, "result": apply_bp(amount, bp)})
    usd = [{"cents": c, "usd": _usd(c)} for c in
           [0, 1, 49, 50, 51, 99, 100, 149, 150, 250, 350, -1, -49, -50, -51, -150, 99999949, 99999950,
            123456789, -123456789, 37738350, 9223372036854775807, -9223372036854775807]]
    areas = []
    # 1e15 + 0.25 is deliberately absent: its repr is an exact tie between two 17-digit strings,
    # which part 1's py::float_repr breaks differently from CPython (part 1 review R11, owed).
    for a in [148.6, 139.4, 148.65, 139.96, 0.04, 0.05, 0.15, 100.0, 1e-5, 12345.675, 2.675, 98765.4321]:
        r = round(a, 1)
        areas.append({"area": a, "round1": r, "decimal": str(Decimal(str(r)))})
    return {"sum": sums, "true_div": divs, "round": rounds, "work_item_totals": totals, "apply_bp": bps, "usd": usd,
            "area_quantity": areas}


def main() -> None:
    pipeline_cases = [
        pipeline_case("demo", {}),
        pipeline_case("no_capital", {"opening_capital": None}),
        pipeline_case("capital_15m", {"opening_capital": 15000000}),
        pipeline_case("rent_limit_5k", {"monthly_rent_limit": 500000}),
        pipeline_case("two_rooms_two_sinks_200A", {
            "space_requirements": {"required_rooms": {"treatment": 2, "office": 1}},
            "infrastructure_requirements": {"minimize_plumbing_changes": False, "sinks_per_treatment_room": 2,
                                            "min_electrical_service_amps": 200}}),
        pipeline_case("retail_equal_weights", {
            "business_type": "retail", "space_requirements": {"required_rooms": {"office": 2}},
            "ranking_priorities": {"total_cost": 1, "infrastructure_fit": 1, "evidence_certainty": 1,
                                   "comparison_horizon_months": 60}}),
    ]
    dump("part2_pipeline.json", {
        "_meta": {
            "generator": "apps/backend/tests-golden/domain/gen_domain2.py",
            "fixtures": ["fixtures/projects/facial_studio.json", "fixtures/properties/la_candidates.json",
                         "fixtures/permits/la_permits_snapshot.json"],
            "note": "Listings are FIXTURE_SYNTHETIC demo values (not CoStar records); permits are a real LADBS "
                    "snapshot (FIXTURE_REAL). Property ids are fixed after the search; see fixed_property_ids.",
            "horizons": HORIZONS + ["profile"],
            "project_id": str(PROJECT_ID),
            "profile_id": str(PROFILE_ID),
        },
        "cases": pipeline_cases,
    })
    case_list, bases = cases()
    dump("part2_cases.json", {
        "_meta": {
            "generator": "apps/backend/tests-golden/domain/gen_domain2.py",
            "note": "Engine-level cases. Inputs start from the fixture pipeline; added broker evidence is SYNTHETIC "
                    "(data_origin FIXTURE_SYNTHETIC, limitations say so).",
            "project_id": str(PROJECT_ID),
            "profile_id": str(PROFILE_ID),
        },
        "bases": bases,
        "cases": case_list,
    })
    dump("part2_units.json", units())


if __name__ == "__main__":
    main()
