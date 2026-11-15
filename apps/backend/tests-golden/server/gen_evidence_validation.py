"""Golden vectors for the evidence validation route: Python's `validate_property` over constructed
cases, so the Rust route is compared on problems Python actually reports (the demo data has none).

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_evidence_validation.py)
Needs: property_slice.json (from gen_property_slice.py); pure functions only, no database, no
       network.

Every case starts from the Python-written Hollywood property and its evidence records in
property_slice.json ("property 6922 W HOLLYWOOD BLVD", "evidence 6922 W HOLLYWOOD BLVD") and changes
one thing. Ids are remapped per case (uuid5 of the case name and the old id), so the cases can be
stored side by side; references between assessments and evidence keep pointing at the same
records. `problems` is exactly what the supervisor tool `validate_evidence` computes for that
data: validate_property(prop, evidence, prop.infrastructure_assessments).

Output: evidence_validation.json, {"cases": [{name, property, evidence, problems}]}.
"""

from __future__ import annotations

import copy
import json
import sys
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
API = HERE.parents[2] / "api"
sys.path.insert(0, str(API))

from app.contracts import PropertyDigitalProfile, SourceEvidence  # noqa: E402
from app.domains.evidence.validator import validate_property  # noqa: E402

UNKNOWN_A = "00000000-0000-4000-8000-0000000000aa"
UNKNOWN_B = "00000000-0000-4000-8000-0000000000bb"


def load() -> tuple[dict, list[dict]]:
    steps = json.loads((HERE / "property_slice.json").read_text())
    by = {s["name"]: s["response"] for s in steps}
    return by["property 6922 W HOLLYWOOD BLVD"], by["evidence 6922 W HOLLYWOOD BLVD"]


def remap(case: str, prop: dict, evidence: list[dict]) -> tuple[dict, list[dict]]:
    ns = uuid.uuid5(uuid.NAMESPACE_URL, f"second-site/evidence-validation/{case}")
    ids: dict[str, str] = {}

    def new(old: str) -> str:
        if old in (UNKNOWN_A, UNKNOWN_B):
            return old
        return ids.setdefault(old, str(uuid.uuid5(ns, old)))

    prop = copy.deepcopy(prop)
    evidence = copy.deepcopy(evidence)
    prop["property_id"] = new(prop["property_id"])
    for a in prop["infrastructure_assessments"]:
        a["assessment_id"] = new(a["assessment_id"])
        a["property_id"] = prop["property_id"]
        a["evidence_ids"] = [new(i) for i in a["evidence_ids"]]
    for e in evidence:
        e["evidence_id"] = new(e["evidence_id"])
        e["property_id"] = prop["property_id"]
    return prop, evidence


def cases(prop: dict, evidence: list[dict]):
    hist = [e["evidence_id"] for e in evidence if e["evidence_classification"] == "DOCUMENTED_HISTORICAL"]
    unknown = [e["evidence_id"] for e in evidence if e["evidence_classification"] == "UNKNOWN"]

    def variant(f):
        p, ev = copy.deepcopy(prop), copy.deepcopy(evidence)
        f(p, ev)
        return p, ev

    def assessment(p, i):
        return p["infrastructure_assessments"][i]

    yield "as_stored", variant(lambda p, ev: None)

    def unknown_refs(p, ev):
        assessment(p, 0)["evidence_ids"] += [UNKNOWN_A]
        assessment(p, 3)["evidence_ids"] += [UNKNOWN_A, UNKNOWN_B]

    yield "unknown_evidence_references", variant(unknown_refs)

    def meets_on_history(p, ev):
        a = assessment(p, 2)
        a["status"] = "MEETS"
        a["evidence_ids"] = hist[:2]
        a["limitations"] = []

    yield "meets_on_historical_evidence_without_limitations", variant(meets_on_history)

    def meets_with_limits(p, ev):
        a = assessment(p, 2)
        a["status"] = "MEETS"
        a["evidence_ids"] = hist[:2]
        a["limitations"] = ["Historical permit only."]

    yield "meets_on_historical_evidence_with_limitations", variant(meets_with_limits)

    def meets_mixed(p, ev):
        a = assessment(p, 2)
        a["status"] = "MEETS"
        a["evidence_ids"] = hist[:1] + unknown[:1]
        a["limitations"] = []

    yield "meets_on_mixed_evidence", variant(meets_mixed)

    def costar_label(p, ev):
        p["commercial_listing"]["provider"] = "CoStar"

    yield "listing_labeled_costar_not_live", variant(costar_label)

    def costar_upper(p, ev):
        p["commercial_listing"]["provider"] = "COSTAR"

    yield "listing_labeled_costar_upper_case", variant(costar_upper)

    def costar_live(p, ev):
        p["commercial_listing"]["provider"] = "costar"
        p["commercial_listing"]["data_origin"] = "LIVE"

    yield "listing_from_costar_live", variant(costar_live)

    def everything(p, ev):
        unknown_refs(p, ev)
        a = assessment(p, 1)
        a["status"] = "MEETS"
        a["evidence_ids"] = hist[2:4]
        a["limitations"] = []
        costar_label(p, ev)

    yield "everything_at_once", variant(everything)

    def no_evidence(p, ev):
        ev.clear()

    yield "no_evidence_records", variant(no_evidence)


def main() -> None:
    prop, evidence = load()
    out = []
    for name, (p, ev) in cases(prop, evidence):
        p, ev = remap(name, p, ev)
        model = PropertyDigitalProfile.model_validate(p)
        records = [SourceEvidence.model_validate(e) for e in ev]
        problems = validate_property(model, records, model.infrastructure_assessments)
        out.append({
            "name": name,
            "property": model.model_dump(mode="json"),
            "evidence": [r.model_dump(mode="json") for r in records],
            "problems": problems,
        })
    path = HERE / "evidence_validation.json"
    path.write_text(json.dumps({"cases": out}, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {path.name}: " + ", ".join(f"{c['name']} {len(c['problems'])}" for c in out))


if __name__ == "__main__":
    main()
