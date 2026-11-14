"""Generate ss-core golden vectors from the Python contracts (the specification).

Run from the repository root:
    (cd apps/api && .venv/bin/python ../backend/tests-golden/gen_core.py)

Pure functions only: no database, no network. Output: apps/backend/tests-golden/core/*.json
"""

from __future__ import annotations

import copy
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
sys.path.insert(0, str(REPO / "apps" / "api"))

from pydantic import ValidationError  # noqa: E402

from app.contracts import (  # noqa: E402
    BusinessExpansionProfile,
    ConfirmationStatus,
    DataOrigin,
    EvidenceClassification,
    IntegrationStatus,
    MatchStatus,
    ProjectStatus,
    VerificationStatus,
    Measurement,
    Money,
    Point2D,
    ProjectContext,
    SourceReference,
)

OUT = HERE / "core"
PROJECT_ID = "6b1727f7-0000-4000-8000-000000000001"
PROFILE_ID = "6b1727f7-0000-4000-8000-000000000002"
OWNER_ID = "6b1727f7-0000-4000-8000-000000000003"


def dump(name: str, value) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / name).write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def errors(model, data) -> list[dict] | None:
    try:
        model.model_validate_json(json.dumps(data))
    except ValidationError as exc:
        return [
            {"type": e["type"], "loc": list(e["loc"]), "msg": e["msg"]}
            for e in exc.errors(include_url=False, include_context=False)
        ]
    return None


def main() -> None:
    fixture = json.loads((REPO / "fixtures" / "projects" / "facial_studio.json").read_text())
    bp = dict(fixture["business_profile"], project_id=PROJECT_ID, profile_id=PROFILE_ID)
    profile = BusinessExpansionProfile.model_validate_json(json.dumps(bp))
    dump(
        "business_profile_facial_studio.json",
        {
            "source": "fixtures/projects/facial_studio.json business_profile + project_id + profile_id",
            "input": bp,
            "expected": profile.model_dump(mode="json"),
        },
    )

    project_inputs = [
        {"owner_id": OWNER_ID, "project_id": PROJECT_ID, "project_name": "x",
         "market": {"city": "Los Angeles", "state": "CA"},
         "created_at": "2026-11-14T12:00:00Z", "updated_at": "2026-11-14T12:00:00.5Z"},
        {"owner_id": OWNER_ID, "project_id": PROJECT_ID, "project_name": fixture["project_name"],
         "market": fixture["market"], "project_status": "COMPARING",
         "business_profile_id": PROFILE_ID, "current_budget_revision": 3, "record_version": 7,
         "created_at": "2026-01-01T00:00:00.123+02:00", "updated_at": "2026-11-14T18:53:19.128932-08:00"},
    ]
    dump(
        "project_context_roundtrip.json",
        [
            {"input": i, "expected": ProjectContext.model_validate_json(json.dumps(i)).model_dump(mode="json")}
            for i in project_inputs
        ],
    )

    pc = project_inputs[0]
    good_bp = bp
    cases: list[dict] = []

    def case(model, name: str, data) -> None:
        errs = errors(model, data)
        assert errs, f"{name}: expected a validation error"
        cases.append({"model": model.__name__, "name": name, "input": data, "errors": errs})

    def without(d, key):
        d = copy.deepcopy(d)
        d.pop(key)
        return d

    def with_(d, path, value):
        d = copy.deepcopy(d)
        cur = d
        for k in path[:-1]:
            cur = cur[k]
        cur[path[-1]] = value
        return d

    case(ProjectContext, "owner_id missing", without(pc, "owner_id"))
    case(ProjectContext, "extra field", with_(pc, ["surprise"], 1))
    case(ProjectContext, "naive created_at", with_(pc, ["created_at"], "2026-11-14T12:00:00"))
    case(ProjectContext, "empty project_name", with_(pc, ["project_name"], ""))
    case(ProjectContext, "project_name too long", with_(pc, ["project_name"], "x" * 201))
    case(ProjectContext, "market state too short", with_(pc, ["market", "state"], "C"))
    case(ProjectContext, "market state too long", with_(pc, ["market", "state"], "CAL"))
    case(ProjectContext, "market extra field", with_(pc, ["market", "zip"], "90028"))
    case(ProjectContext, "record_version zero", with_(pc, ["record_version"], 0))
    case(ProjectContext, "negative budget revision", with_(pc, ["current_budget_revision"], -1))
    case(ProjectContext, "bad project_status", with_(pc, ["project_status"], "DONE"))
    case(ProjectContext, "bad owner uuid", with_(pc, ["owner_id"], "not-a-uuid"))
    case(Money, "lowercase currency", {"amount_minor": 1, "currency": "usd"})
    case(Money, "two-letter currency", {"amount_minor": 1, "currency": "US"})
    case(Money, "digit in currency", {"amount_minor": 1, "currency": "U5D"})
    case(Money, "fractional minor units", {"amount_minor": 1.5})
    case(Money, "amount missing", {"currency": "USD"})
    case(Measurement, "unit missing", {"value": 1.0})
    case(Point2D, "y missing", {"x": 1.0})
    case(Point2D, "extra field", {"x": 1.0, "y": 2.0, "z": 3.0})
    case(SourceReference, "description missing", {"url": "https://example.invalid"})
    case(SourceReference, "bad evidence uuid", {"description": "d", "evidence_id": "nope"})
    case(BusinessExpansionProfile, "empty business_type", with_(good_bp, ["business_type"], ""))
    case(BusinessExpansionProfile, "negative location count", with_(good_bp, ["current_location_count"], -1))
    case(BusinessExpansionProfile, "two field errors",
         with_(with_(good_bp, ["business_type"], ""), ["current_location_count"], -1))
    case(BusinessExpansionProfile, "negative opening capital", with_(good_bp, ["opening_capital"], -5))
    case(BusinessExpansionProfile, "negative rent limit", with_(good_bp, ["monthly_rent_limit"], -1))
    case(BusinessExpansionProfile, "lowercase currency", with_(good_bp, ["currency"], "usd"))
    case(BusinessExpansionProfile, "space_requirements missing", without(good_bp, "space_requirements"))
    case(BusinessExpansionProfile, "zero min area", with_(good_bp, ["space_requirements", "min_area_sqm"], 0))
    case(BusinessExpansionProfile, "negative max area", with_(good_bp, ["space_requirements", "max_area_sqm"], -1))
    case(BusinessExpansionProfile, "horizon too long",
         with_(good_bp, ["ranking_priorities", "comparison_horizon_months"], 241))
    case(BusinessExpansionProfile, "horizon zero",
         with_(good_bp, ["ranking_priorities", "comparison_horizon_months"], 0))
    case(BusinessExpansionProfile, "negative weight", with_(good_bp, ["ranking_priorities", "total_cost"], -0.1))
    case(BusinessExpansionProfile, "negative sinks",
         with_(good_bp, ["infrastructure_requirements", "sinks_per_treatment_room"], -1))
    case(BusinessExpansionProfile, "zero amps",
         with_(good_bp, ["infrastructure_requirements", "min_electrical_service_amps"], 0))
    case(BusinessExpansionProfile, "negative staff",
         with_(good_bp, ["operational_requirements"], {"staff_count": -1}))
    case(BusinessExpansionProfile, "provenance status missing",
         with_(good_bp, ["source_references"], [{"field": "*", "source": "s"}]))
    case(BusinessExpansionProfile, "bad confirmation status", with_(good_bp, ["confirmation_status"], "MAYBE"))
    case(BusinessExpansionProfile, "nested extra field", with_(good_bp, ["design_preferences", "colour"], "red"))
    case(BusinessExpansionProfile, "non-string market value", with_(good_bp, ["target_market", "city"], 5))
    dump("validation_failures.json", cases)
    dump(
        "enums.json",
        {
            e.__name__: [m.value for m in e]
            for e in (
                EvidenceClassification,
                VerificationStatus,
                MatchStatus,
                DataOrigin,
                IntegrationStatus,
                ProjectStatus,
                ConfirmationStatus,
            )
        },
    )
    print(f"wrote {len(cases)} validation cases and 2 golden documents to {OUT}")


if __name__ == "__main__":
    main()
