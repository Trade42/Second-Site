"""Generate ss-domain part 4 golden vectors (communications) from the Python implementation, which
is the specification.

Run from the repository root:
    (cd apps/api && .venv/bin/python -B ../backend/tests-golden/domain/gen_domain4.py)

Pure functions only: no database, no network. Fresh uuid4 ids are seeded, so two runs are
byte-identical. Reads part2_pipeline.json (the part 2 pipeline's
recorded assessments, reused as build_script inputs) and writes:
- part4_comms.json: every CallState and HandoffStatus pair, build_script, extract and describe,
  detect_escalation, client_state, call_payload with payload_hash, build_session_update;
- part4_signatures.json: Standard Webhooks and Telnyx signature checks;
- part4_unicode.json: CPython's `re` classes for str patterns (\\w, \\s, \\d), its re.I equivalents of
  ASCII letters, and the code points assigned in its Unicode version, for the Rust regex layer;
- part4_base64.json: base64.b64decode (validate=False) on well-formed and malformed input.

Every answer and utterance is synthetic test text. Keys used for signatures are fixed test keys
(never real credentials): an HMAC secret built from a fixed ASCII phrase and an Ed25519 key from a
fixed seed. Headers go through starlette.datastructures.Headers, as in a real request.
"""

from __future__ import annotations

import base64
import json
import re
import sys
import time
import unicodedata
from pathlib import Path
from uuid import UUID

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
sys.path.insert(0, str(REPO / "apps" / "api"))

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey  # noqa: E402
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat  # noqa: E402
from starlette.datastructures import Headers  # noqa: E402

import random  # noqa: E402

import app.contracts.common as _common  # noqa: E402

# Fresh ids (CallSession.call_id, assessment ids) come from a seeded generator so that the vectors
# are byte-identical on every run.
_RNG = random.Random(0x1AFEB98F)
_common.uuid4 = lambda: UUID(int=_RNG.getrandbits(128), version=4)

from app.contracts import (  # noqa: E402
    AuthorityScope,
    CallQuestion,
    CallSession,
    CallState,
    HandoffStatus,
    InfrastructureAssessment,
    OwnerHandoffRequest,
    StructuredFinding,
)
from app.contracts.approvals import payload_hash  # noqa: E402
from app.contracts.state import InvalidStateTransition  # noqa: E402
from app.core.errors import InvalidSignatureError  # noqa: E402
from app.domains.communications.call_script import AVAILABILITY, build_script, describe, extract  # noqa: E402
from app.domains.communications.escalation import detect_escalation  # noqa: E402
from app.integrations.telnyx import decode_client_state, encode_client_state  # noqa: E402
from app.integrations.webhook_signatures import (  # noqa: E402
    standard_webhooks_sign,
    verify_standard_webhook,
    verify_telnyx,
)
from app.integrations.xai_voice import build_session_update  # noqa: E402
from app.services.call_orchestration import call_payload  # noqa: E402

P = UUID("1afeb98f-0000-4000-8000-000000000001")
O = UUID("1afeb98f-0000-4000-8000-000000000002")
A = UUID("1afeb98f-0000-4000-8000-000000000003")


def jd(model):
    return json.loads(model.model_dump_json())


# ---- state machines ---------------------------------------------------------------------------


def state_pairs():
    calls = []
    for a in CallState:
        for b in CallState:
            c = CallSession(project_id=P, owner_id=O, recipient={"phone": "+15555550100"}, approval_id=A, state=a)
            try:
                c.transition(b)
                calls.append({"from": a.value, "to": b.value, "ok": True})
            except InvalidStateTransition as e:
                calls.append({"from": a.value, "to": b.value, "ok": False, "error": str(e)})
    handoffs = []
    for a in HandoffStatus:
        for b in HandoffStatus:
            h = OwnerHandoffRequest(
                call_id=A,
                project_id=P,
                reason="FINANCIAL_DETAILS",
                question="q",
                owner_id=O,
                expires_at="2026-11-14T12:00:00Z",
                status=a,
            )
            try:
                h.transition(b)
                handoffs.append({"from": a.value, "to": b.value, "ok": True})
            except InvalidStateTransition as e:
                handoffs.append({"from": a.value, "to": b.value, "ok": False, "error": str(e)})
    return {"call": calls, "handoff": handoffs}


# ---- build_script -----------------------------------------------------------------------------


def assessment(rid, status):
    return InfrastructureAssessment(
        project_id=P,
        property_id=O,
        requirement_id=rid,
        status=status,
        uncertainty="x",
        verification_questions=["?"],
        evidence_ids=[A] if status == "MEETS" else [],
    )


def denormalize(v):
    """part2_pipeline.json labels fresh ids `<id:N>` and timestamps `<timestamp>`; give them fixed values."""
    if isinstance(v, dict):
        return {k: denormalize(x) for k, x in v.items()}
    if isinstance(v, list):
        return [denormalize(x) for x in v]
    if isinstance(v, str) and v.startswith("<id:") and v.endswith(">"):
        return str(UUID(int=0x1AFEB98F_0000_4000_8000_00000000_0000 + int(v[4:-1])))
    if v == "<timestamp>":
        return "2026-11-14T12:00:00Z"
    return v


def scripts():
    out = []
    pipeline = json.loads((HERE / "part2_pipeline.json").read_text())
    for case in pipeline["cases"]:
        for addr, prop in case["output"]["properties"].items():
            assessments = [InfrastructureAssessment.model_validate(denormalize(a)) for a in prop["assessments"]]
            out.append(
                {
                    "name": f"pipeline/{case['name']}/{addr}",
                    "assessments": [jd(a) for a in assessments],
                    "script": [jd(q) for q in build_script(assessments)],
                }
            )
    S = ["UNKNOWN", "MAY_REQUIRE_WORK", "FAILS", "MEETS"]
    synthetic = {
        "empty": [],
        "all_open": [
            ("fs.electrical.tenant_service", "UNKNOWN"),
            ("fs.plumbing.treatment_sinks", "UNKNOWN"),
            ("fs.hvac.ventilation", "MAY_REQUIRE_WORK"),
            ("fs.space.min_area", "UNKNOWN"),
        ],
        "reverse_order": [
            ("fs.space.min_area", "UNKNOWN"),
            ("fs.hvac.ventilation", "UNKNOWN"),
            ("fs.plumbing.treatment_sinks", "UNKNOWN"),
        ],
        "closed_only": [(f"fs.{s}", st) for s in ("electrical.tenant_service", "space.min_area") for st in ("FAILS", "MEETS")],
        "duplicate_suffix_last_wins": [
            ("a.electrical.tenant_service", "UNKNOWN"),
            ("b.electrical.tenant_service", "MAY_REQUIRE_WORK"),
            ("c.electrical.tenant_service", "FAILS"),
        ],
        "no_dot_and_one_dot": [
            ("electrical", "UNKNOWN"),
            ("electrical.tenant_service", "UNKNOWN"),
            ("x.plumbing.treatment_sinks", "UNKNOWN"),
        ],
        "deep_prefix": [("a.b.c.hvac.ventilation", "UNKNOWN"), ("a.hvac.ventilation", "UNKNOWN")],
        "unknown_suffix": [("fs.zoning.use", "UNKNOWN"), ("fs.space.enclosed_rooms", "UNKNOWN")],
        "case_sensitive": [("fs.Electrical.tenant_service", "UNKNOWN"), ("fs.space.min_area", "UNKNOWN")],
    }
    for name, items in synthetic.items():
        assessments = [assessment(r, s) for r, s in items]
        out.append({"name": f"synthetic/{name}", "assessments": [jd(a) for a in assessments], "script": [jd(q) for q in build_script(assessments)]})
    for mask in range(16):
        items = [(f"p.{suffix}", S[(mask >> i) & 1]) for i, suffix in enumerate(["electrical.tenant_service", "plumbing.treatment_sinks", "hvac.ventilation", "space.min_area"])]
        assessments = [assessment(r, s) for r, s in items]
        out.append({"name": f"mask/{mask}", "assessments": [jd(a) for a in assessments], "script": [jd(q) for q in build_script(assessments)]})
    return out


# ---- extract / describe -----------------------------------------------------------------------

QUESTIONS = {
    "amps": CallQuestion(question="How many amps?", claim_type="tenant_subpanel_amps", requirement_id="fs.electrical.tenant_service"),
    "plumbing": CallQuestion(question="Plumbing?", claim_type="in_unit_plumbing"),
    "hvac": CallQuestion(question="HVAC?", claim_type="hvac_zonable"),
    "sqft": CallQuestion(question="Square footage?", claim_type="usable_area_sqft"),
    "available": AVAILABILITY,
    "other": CallQuestion(question="Anything else?", claim_type="permitted_use"),
}

ANSWERS = {
    "amps": [
        "It's a 400 amp service, 3 phase", "400 amps", "400amps", "400-amp panel", "400 - amp", "400 -amp",
        "200A", "200 A", "200a service", "about 100a.", "100ab", "200 amperes", "200 ampere", "200 AMPS",
        "12345 amps", "1 amp", "100 and 200 amps", "the panel is 60 amps, service 400 amps", "9999 amp",
        "99 A", "a 150 a", "150 a b", "150 apples", "150 ampsx", "150 amp.", "150\tamp", "150\namps",
        "150\x1camps", "150\xa0amps", "150  amps", "150 \u2003amps", "\u0664\u0660\u0660 amps", "\uff14\uff10\uff10 amps",
        "4\u0660\u0660 amps", "Not sure, maybe 200 amps? I'd have to check", "No idea, 200 amps maybe",
        "I'll find out but it's 200 amps", "ill find out", "unsure", "I don't know, 100 amps?", "dont know",
        "we would have to check, 200 amps", "no amps listed", "", "   ", "two hundred amps", "200 \u0410",
        "200 \u212amps", "200 amp\u0301", "200 a\u0301", "\u00bd200 amps", "x200 amps", "_200 amps",
        "200\u2010amp", "mostly 225 amp service; about 80 amps in use",
    ],
    "sqft": [
        "1,200 sq ft", "1200 square feet", "1,200 usable sq. ft", "1200sf", "1200 SF", "12 sq ft", "120 sq ft",
        "1,2 sq ft", "1,,,, sq ft", "about 950 square feet usable", "950 sq. feet", "950 sqft", "950 sq.ft.",
        "950 square ft", "1234567890 sf", "950 sfx", "950 s.f.", "950sq   ft", "950 usable   sq ft",
        "950 usablesq ft", "it's 1,128 SF per CoStar", "1 128 sq ft", "\u0669\u0665\u0660 sq ft",
        "950 SQ FT", "950 Sq. Ft.", "not sure, 950 sq ft", "have to check", "", "nine hundred fifty",
        "950 sq\u00a0ft", "950 sq\x1cft", "950 \u017ff", "950 sf\u0301", "950,000 square feet", "9,500,000 sf",
        "between 900 and 1,000 sq ft", "roughly 1.2k sq ft", "1.200 sq ft",
    ],
    "plumbing": [
        "Yes, there's a sink in back", "No, it's all open", "yes", "Yeah", "yep!", "there is a sink",
        "there's a drain", "it has a sink", "it does", "we have a floor drain", "definitely", "absolutely",
        "correct", "sure", "no", "nope", "none", "not really", "there isn't", "there isnt", "doesn't",
        "doesnt", "don't have one", "dont have", "it's all open", "no plumbing", "yes and no", "No, there's nothing",
        "not sure", "I don't know", "Yesterday it was fine", "nobody knows", "yesss", "nonexistent",
        "", "maybe", "YES", "NO", "N/A", "Sure, no problem", "Absolutely not", "s\u0130nk? yes",
        "ye\u017f", "yes\u0301", "\u00bdyes", "yes\u203f", "yes\u200d", "y\u0435s (cyrillic e)", "there\u2019s a sink",
        "I'll find out, yes", "There is no plumbing", "It does not", "correct, none",
    ],
    "hvac": [
        "yes it can be zoned", "no, single zone", "absolutely", "nope", "we have VAV boxes", "doesn't matter, yes",
        "not really", "I would have to check", "correct", "it's all open plan", "sure thing", "none", "",
    ],
    "available": [
        "January 1st", "  March  ", "", "   ", "Immediately", "\u00a0Q2 2027\u00a0", "x" * 250, "\u00e9" * 201,
        "not sure", "I'll find out", "available now, no deposit needed", "\x1cJune\x1f", "\U0001f600 soon",
    ],
    "other": ["medical spa is allowed", "", "have to check", "ok"],
}

# Commission rust-domain-5 (extraction truth fix): negations never read as yes, cut numbers never
# reported. Appended after ANSWERS so the earlier vectors keep their order. Includes the canned
# answers of both call simulators and the answers of the Python tests.
TRUTH_ANSWERS = {
    "amps": [
        # simulator, Python tests, plain readings that must keep their value
        "The suite has its own 200 amp panel, I'd guess about a third of it is in use.",
        "400a", "400 a", "400A", "400 A", "400amp", "400 amp", "400-amp", "400 Amps", "400 AMPERES",
        "400 ampere", "400 amps.", "(400 amps)", "~400 amps", "\u201c400 amps\u201d", "panel: 400 amps",
        "Panel:400 amps", "a 120/208 volt, 400 amp service", "3 phase 400 amp", "200 amps and 3 phase",
        "400\u00a0amps", "400\u2009amps", "\u00a0400 amps", "400 amps, yes 400 amps", "400A (400 amps)",
        "200 amp panel, about 80 in use", "200 amp, 200 amp", "2 x 200 amps", "It's 400 amps or so",
        "about 200A", "x" * 3000 + " 400 amps", "400 amps " + "and so on " * 300,
        # cut, joined or malformed numbers
        "1,200 amps", "1.200 amps", "1 200 amps", "1\u00a0200 amps", "4\u202f000 amps", "123456 amps",
        "12345 amp", "2x200 amps", "x400 amps", "_400 amps", "-400 amps", "+400 amps", "400.5 amps",
        "400.50 amps", "approx.400 amps", "#400 amps", "2,000 a month", "\u2018400 amps\u2019",
        "1'200 amps", "1\u2019200 amps",
        # several values and ranges
        "the panel is 60 amps, service 400 amps", "200 to 400 amps", "200-400 amps", "200 - 400 amps",
        "200\u2013400 amps", "200\u2014400 amps", "200 or 400 amps", "200/400 amps", "200~400 amps",
        "between 200 and 400 amps", "from 200 to 400 amps", "200 amps to 400 amps", "200 amps or 400",
        "200 amps to 400", "200, 400 amps", "200 & 400 amps", "200 a 300 amps",
        "mostly 225 amp service; about 80 amps in use", "60 amps in use of a 200 amp panel",
        # zero and decimals
        "0 amps", "00 amps", "000 amps", "0000 amps", "00 A",
        # Unicode digits and spaces
        "\u0664\u0660\u0660 amps", "\u0664\u0660\u0660 amps or 400 amps", "\u0663\u0664\u0665 4 amps",
        "\u0661\u0662\u066345 amps", "4\u0660\u0660 amps", "\uff14\uff10\uff10 amps", "400\u3000amps",
        "\u096a\u0966\u0966 amps",
        # hedges with typographic apostrophes
        "I don\u2019t know, 100 amps?", "I\u2019ll find out, maybe 200 amps", "I don\u02bct know, 200 amps",
        "200 amps or 400 amps", "400 amps or 400 amps", "200A service, 100A in use", "About 400. Amps",
        "400 amp service", "1, 200 amps",
        # known limitations (reported, not fixed): bounds and a unitless second value read as the value
        "from 200 up to 400 amps", "up to 400 amps", "at least 200 amps", "200 amps or more", "2000 a month",
        "The panel is 200 amps, and we use about 80 of them",
    ],
    "sqft": [
        "It's about 1,250 usable square feet.", "about 1,128 square feet", "1,128 sf", "1,128SF",
        "1128 sq. ft.", "1128 square ft", "1128 sq feet", "1128 usable sq ft", "1128 usable square feet",
        "1128 SQUARE FEET", "1128sqft", "1128 sq. feet", "1,128 sq ft.", "12,345 sq ft", "123,456 sq ft",
        "1234567 sf", "100 sq ft", "approximately 1,128 sf", "~1,128 sf", "(1,128 sf)", "\u00a01,128 sf",
        "1,128\u00a0sf", "1,128 sf, I mean 1,128 sf", "1,128 sf or 1128 sf",
        # cut, joined or malformed numbers
        "9,500,000 sf", "1234567890 sf", "1.200 sq ft", "1 128 sq ft", "1\u202f128 sq ft", "1\u2009128 sq ft",
        "1'128 sq ft", "1\u2019128 sq ft", "11,28 sf", "1,1280 sf", "1,128,000 sf", "1,000,000 sq ft",
        "1234,567 sf", "12345678 sf", "1,128, sq ft", ",128 sq ft", "1,,128 sf", "#1,128 sf", "$1,128 sf",
        "1128.5 sf", "1,128.5 sq ft", "1128.50 sf", "1128.500 sf", "1-128 sf",
        # several values and ranges
        "between 1,000 and 1,200 sf", "1,000 to 1,200 sf", "1,000-1,200 sf", "1,000 - 1,200 sq ft",
        "1,000\u20131,200 sf", "1,128 sf per CoStar, 1,100 sf measured",
        "The suite is 1,128 sf; the building is 20,000 sf", "1,128 sf or 1,200 sf", "1,128 sf to 1,200",
        "1,000 or 1,200 square feet",
        # zero
        "000 sf", "0,000 sf", "0000 sq ft",
        # Unicode digits
        "\u0661\u0661\u0662\u0668 sf", "1\u066128 sf", "\uff11\uff11\uff12\uff18 sf", "1,128 \u0665 sf",
        "\u0665 1,128 sf",
        "1,128 sf or 1,128 sf", "1,128 sf to 1,128", "1,128 sq ft and 1,128 sq ft", "1, 128 sq ft",
        "1,128 square feet, roughly",
        # known limitations (reported, not fixed)
        "It's about 1,128 square feet, maybe 1,200", "1,128 sq ft (about 105 m2)",
    ],
    "plumbing": [
        # simulator, Python tests
        "Yes, there's a sink in the back room and a restroom inside the suite.", "Yes, there's a sink in back",
        "No, it's all open",
        # plain yes
        "Yes", "yes.", "Yes!", "Yeah, there is.", "Yep, there's a sink.", "Yes, there is a floor drain.",
        "There is a sink by the back door.", "There's a floor drain in the storage room.",
        "It has a small kitchenette sink.", "It does.", "We have a sink and a floor drain.", "Definitely.",
        "Absolutely, yes.", "That's correct.", "Sure.", "Sure thing, there's a sink.", "YES THERE IS",
        "there\u2019s a sink", "that\u2018s correct", "Yes,\u2003there is a sink", "ye\u017f",
        # plain no
        "No", "no.", "No!", "Nope.", "None.", "Not really.", "There isn't.", "There isnt any", "It doesn't.",
        "It doesnt", "We don't have one.", "We dont have that", "It's all open.", "No plumbing at all.",
        "No, there isn't.", "No, we don't.", "No, never had any.", "No, nothing in the suite.", "Nope, none.",
        "Not really, no.", "no no no", "No it's not", "We don't, no.", "I don't think so, no.",
        "There isn\u2019t any plumbing.", "It doesn\u2019t.", "We don\u2019t have one.", "No, it\u2019s all open.",
        "don\u02bct have one", "It doesn\u2018t", "no\u00a0plumbing", "there\u00a0is no sink",
        # negated affirmations: never yes
        "No, it does not.", "There is no plumbing in the suite.", "There's no sink.", "There\u2019s no sink.",
        "It has no plumbing.", "We have no plumbing.", "Absolutely not.", "Absolutely not!", "Definitely not.",
        "It does not.", "It does not have plumbing.", "There is not a sink.", "There's not a sink.",
        "There\u2019s not a sink.", "We have not had plumbing there.", "Not correct.", "That's not correct.",
        "Not definitely.", "Not absolutely.", "Not yes.", "it does NOT", "IT DOES NOT", "it does\u00a0not",
        "It has never had a sink.", "We have nothing like that.", "There is nothing there.", "it does n't",
        "That isn\u2019t correct.", "Absolutely not, it's all open.", "Definitely not, no plumbing.",
        "No, absolutely not.", "Definitely never.", "Correct, there is no sink.", "There is hardly any plumbing.",
        "We have barely a drain.", "Absolutely none.", "Definitely none.", "Absolutely no plumbing.",
        "\u0131t doe\u017f not", "absolutely not \u2013 it\u2019s all open",
        # double negatives
        "It's not that there isn't a sink.", "I wouldn't say there's no plumbing.", "There isn't no sink.",
        "Never had none.", "It's not like we don't have one.", "I can't say there's no drain.",
        "There is not no sink.", "Not no.", "I don\u2019t think there\u2019s no sink",
        # hedges
        "I'm not sure, I'd have to check.", "I don\u2019t know.", "No idea.", "I\u2019ll find out.",
        "I\u2019ll find out, yes", "Unsure, maybe.", "Maybe yes.", "Probably.", "I think so.",
        "I don't think so.", "Yes but I don\u2019t know where",
        # mixed and idioms
        "Yes and no.", "Yes, no.", "yes no", "Yes, but no drain.", "No, yes there is.", "Sure, no problem.",
        "No problem.", "We have no problem with that.", "There's no issue, it has a sink.",
        "Yes, it doesn't leak.", "Yes there's a sink, no drain.", "Absolutely, no doubt.", "no yes",
        "No, but there is a sink.", "Yes, without a doubt.", "Yes, nothing unusual.", "Yes, not a problem.",
        "Yes, neither a sink nor a drain is missing.", "Yes, it cannot leak.", "No, not a problem.",
        "We have no reason to think it can't.",
        # empty and long
        "", "   ", "yes " * 300, "no " * 300, "Yes, " + "there is a sink and " * 50 + "it does not drain",
    ],
    "hvac": [
        "Yes, the landlord's mechanical contractor said it can be split into zones.",
        "Absolutely, it can be zoned.", "It can't be zoned, no.", "It cannot be zoned.", "It can be zoned.",
        "No, it's a single zone.", "Definitely not.", "Absolutely not.", "It does not.", "No, it does not.",
        "No problem.", "We have no problem zoning it.", "Sure, no problem.", "We have VAV boxes, yes.",
        "Not correct, it's one zone.", "Yes.", "No.",
        "It can be zoned, absolutely.", "It does not, no.", "It does not have one, no.",
        "Not really, it doesn\u2019t", "Yes it does", "Yes, it does.", "No, it doesn't.", "Yes, it has.",
        "Yes we have", "Nah", "Nah, yes",
        # known limitations (reported, not fixed): hedges other than UNSURE's are not unclear
        "Probably yes", "I think so, yes", "There is no sink but there is a drain", "No sink, but there is a floor drain",
    ],
    "available": [
        "It's available December first.", "I\u2019ll find out", "I don\u2019t know yet", "January 1st",
    ],
}


def finding_cases():
    out = []
    for key, answers in [*ANSWERS.items(), *TRUTH_ANSWERS.items()]:
        q = QUESTIONS[key]
        for ans in answers:
            f = extract(q, ans)
            out.append({"question": jd(q), "answer": ans, "finding": jd(f), "describe": describe(f)})
    return out


def describe_cases():
    out = []
    base = dict(question="q", answer="a")
    for kw in [
        dict(claim_type="x", value=None),
        dict(claim_type="x", value=True),
        dict(claim_type="x", value=False),
        dict(claim_type="x", value=400, unit="A"),
        dict(claim_type="x", value=400, unit=""),
        dict(claim_type="x", value=2.5),
        dict(claim_type="x", value=1e16, unit="sqft"),
        dict(claim_type="x", value=0.1 + 0.2),
        dict(claim_type="x", value="text", unit="u"),
        dict(claim_type="x", value=""),
        dict(claim_type=None, value=None),
        dict(claim_type=None, value=5),
        dict(claim_type="x", value=[1, "a", None, True]),
        dict(claim_type="x", value={"k": "v"}),
        dict(claim_type="x", value=0),
        dict(claim_type="x", value=-3, unit="A"),
    ]:
        f = StructuredFinding(**base, **kw)
        out.append({"finding": jd(f), "describe": describe(f)})
    return out


# ---- escalation -------------------------------------------------------------------------------

UTTERANCES = [
    # The Python test's four.
    "Will you need a guarantor?", "We need first month and deposit by Friday", "Can I talk to the owner?",
    "Is there a sink in the back room?",
    # Guarantor.
    "guarantor", "GUARANTOR required", "a guarantee", "guaranteed rent", "guarantors", "is it guaranteed?",
    "personal guarantee please", "guarant", "unguarantee", "the guarantee's terms", "guarantee-backed",
    "guarant\u0435e", "guarantor\u0301", "\u00bdguarantor", "guarantor\u203f", "guarantor\u200d", "guarantor\u24b6",
    # Financial.
    "deposit", "deposits", "a security deposit", "first month", "first  month", "first-month", "firstmonth",
    "wire it", "wired", "wire transfer", "payment", "payments", "make a payment", "bank", "banking", "bank account",
    "credit check", "credit score", "credit scores", "credit  check", "credit\u00a0check", "credit\x1ccheck",
    "ban\u212a details", "de\u017fposit", "depo\u017fit", "DEPOSIT", "Deposit?", "first Month", "PAYMENT",
    "deposition", "redeposit", "wireless", "rewire", "banks", "bankrupt",
    # Binding commitment.
    "sign", "sign the lease", "signature", "signal", "design", "can you sign today", "signing", "commit",
    "commitment", "committed", "lock it in", "lock this in", "lock that in", "lock it  in", "agree to terms",
    "agree to", "agreed to", "agreement", "LOI", "loi", "an loi", "LOIs", "loin", "letter of intent",
    "letter of intents", "letter  of intent", "s\u0130gn here", "s\u0131gn here", "\u017fign here",
    "sign\u0301", "\u00bdsign", "sign\u203f", "sign\u200d", "sign\u24b6", "\u24b6sign", "\u0301sign",
    "\u2460sign", "sign\u2460", "sign_", "_sign", "sign1", "1sign", "sign\u0663", "\u0663sign", "sign\u00b2",
    "co-sign", "re-sign", "SIGN", "Sign", "s i g n", "\uff53\uff49\uff47\uff4e", "sig\u0301n",
    # Sensitive personal info.
    "ssn", "SSN please", "ssns", "social security", "social security number", "social  security",
    "date of birth", "DOB", "driver's license", "drivers license", "driver\u2019s license", "driver''s license",
    "drivers licence", "driver license", "s\u017fn", "\u017f\u017fn",
    # Counterparty asks for the owner.
    "speak to the owner", "talk with the owner", "talk to owner", "speak with your boss", "talk to your client",
    "talk to you boss", "speak to the your boss", "speak to the owners", "speaking to the owner",
    "talk  to the owner", "talk to  the owner", "Talk To The Owner", "can i SPEAK WITH YOUR CLIENT", "talk to the boss",
    "talk to your bosses", "talk to youboss", "talk to yourr boss", "talk to the you client", "speak to the owner\u0301",
    "talk with you client", "talk with ur boss", "talk to the owner's agent",
    # Several rules: the first in RULES order wins.
    "sign and pay the deposit", "deposit and guarantor", "talk to the owner about the deposit",
    "your ssn and a signature", "lock it in with a wire", "the owner must sign", "credit check then sign",
    "agree to a guarantee", "letter of intent and first month", "ssn, guarantor",
    # No rule.
    "", " ", "Hello?", "Is the space still available?", "What's the electrical service?", "How many sinks?",
    "We can show it Tuesday.", "The HVAC was replaced in 2019.", "permitted use is retail", "no", "yes",
    "it's a 400 amp service", "parking is shared", "the landlord pays CAM", "the roof is new",
    "We would need financials.", "send me your info", "email me", "call me back", "okay", "Thanks!",
    "guarded", "depot", "wiring", "paymaster", "banquet", "credible", "signage", "commute", "lockbox",
    "agreeable", "lion", "social", "birthday", "license", "speaker", "talking points", "owner", "boss",
    "client", "\u0001\u0002", "\U0001f600", "\u4e00\u4e8c\u4e09", "\u0441\u0438\u0433\u043d", "x" * 300,
    "first\u2003month", "credit\u2028check", "lock\tit in", "talk\nto the owner",
]


def escalation_cases():
    out = []
    for u in UTTERANCES:
        r = detect_escalation(u)
        out.append({"utterance": u, "reason": r.value if r else None})
    return out


# ---- client_state -----------------------------------------------------------------------------


def client_state_cases():
    enc = []
    for state in [
        {"call_id": "c1", "role": "counterparty"},
        {"call_id": "1afeb98f-0000-4000-8000-000000000003", "role": "owner"},
        {"role": "owner", "call_id": "x"},
        {"call_id": "caf\u00e9 \U0001f600", "role": "r\"\\\n\t\x01"},
        {"call_id": 12, "role": None, "n": 1.5, "big": 1e16, "small": 1e-05, "t": True, "list": [1, "a"]},
        {"nested": {"b": 1, "a": 2}},
        {},
    ]:
        # Pairs keep Python's insertion order (the vector file itself is written with sorted keys).
        enc.append({"state_pairs": list(state.items()), "encoded": encode_client_state(state)})
    dec = []
    good = encode_client_state({"call_id": "c1", "role": "counterparty"})
    for value in [
        None, "", good, good.rstrip("="), good + "=", good + "==", "  " + good + "\n", good[:4] + "!!" + good[4:],
        good + "garbage", "not base64 at all!", "WzEsMl0=", "MQ==", "bnVsbA==", "InN0ciI=", "e30=", "e30",
        "eyJhIjoxLCJhIjoyfQ==", "eyJiIjoxLCJhIjoyfQ==", "\u00e9", "====", "=", "a", "ab", "abc", "abcd",
        "eyJjYWxsX2lkIjoiYzEiLCJyb2xlIjoiY291bnRlcnBhcnR5In0", "-_-_", "e30=e30=",
        base64.b64encode(b"\xef\xbb\xbf{\"k\": 1}").decode(), base64.b64encode(b"{\"k\": NaN}").decode(),
        base64.b64encode(b"\xff\xfe{}").decode(), base64.b64encode(b"{\"k\": \"\\ud800\"}").decode(),
        base64.b64encode(b"  {\"k\": [1, 2]}  ").decode(), base64.b64encode(b"{\"k\": 1} x").decode(),
        base64.b64encode("{\"k\": \"\u00e9\"}".encode()).decode(),
    ]:
        try:
            out = decode_client_state(value)
            text = json.dumps(out)
            if "NaN" in text or "Infinity" in text or re.search(r"\\ud[89a-f]", text):
                # Python's json accepts NaN/Infinity literals and lone surrogates; a Rust string
                # and strict JSON cannot carry the result.
                dec.append({"value": value, "decoded_unrepresentable": text})
            else:
                dec.append({"value": value, "decoded_pairs": list(out.items())})
        except Exception as e:  # noqa: BLE001
            dec.append({"value": value, "raises": type(e).__name__})
    return {"encode": enc, "decode": dec}


# ---- call_payload -----------------------------------------------------------------------------


def call_payload_cases():
    out = []
    recipients = [
        {"role": "broker", "name": "B", "phone": "+15555550100"},
        {"role": "broker", "name": "Dana Broker", "phone": "+15555550100"},
        {"phone": "+15555550100"},
        {},
        {"name": "Zo\u00eb \u00c5ngstr\u00f6m", "phone": "+44 20 7946 0958", "role": "listing agent"},
        {"z": "1", "a": "2", "m": "3"},
        {"role": "broker", "name": "\U0001f600 Emoji", "email": "x@example.com"},
        {"name": "quote \" and backslash \\", "phone": "+1\n555"},
        {"phone": "+15555550142", "role": "owner"},
        {"role": "broker", "name": "\u4e2d\u6587\u540d\u5b57"},
        {"role": "Broker", "name": "  spaced  ", "phone": ""},
        {"role": "broker", "name": "Tab\tName"},
    ]
    props = [None, UUID("1afeb98f-0000-4000-8000-00000000c001"), UUID(int=0), UUID("ffffffff-ffff-4fff-bfff-ffffffffffff")]
    projects = [P, UUID("00000000-0000-4000-8000-000000000001")]
    i = 0
    for r in recipients:
        for prop in props:
            project = projects[i % 2]
            i += 1
            payload = call_payload(project, r, prop)
            out.append(
                {
                    "project_id": str(project),
                    "recipient": r,
                    "property_id": str(prop) if prop is not None else None,
                    "payload": payload,
                    "payload_hash": payload_hash(payload),
                    "canonical": json.dumps(payload, sort_keys=True, separators=(",", ":"), default=str),
                }
            )
    return out


# ---- build_session_update ---------------------------------------------------------------------


def session_update_cases():
    out = []
    base = dict(project_id=P, owner_id=O, recipient={"phone": "+15555550100"}, approval_id=A)
    calls = [
        ("default", CallSession(**base)),
        ("custom_scope", CallSession(**base, authority_scope=AuthorityScope(may_ask_about=["availability"], may_not=[]))),
        ("empty_scope", CallSession(**base, authority_scope=AuthorityScope(may_ask_about=[], may_not=[]))),
        ("custom_disclosure", CallSession(**base, automation_disclosure="Automated assistant \u2014 caf\u00e9.")),
    ]
    for name, call in calls:
        for summary, questions in [
            ("6922 W Hollywood Blvd #302, Los Angeles", ["How many amps?", "Is there plumbing?"]),
            ("", []),
            ("\U0001f600 unit", ["one"]),
        ]:
            out.append({"name": name, "call": jd(call), "property_summary": summary, "questions": questions,
                        "session_update": build_session_update(call, summary, questions)})
    return out


# ---- signatures -------------------------------------------------------------------------------

NOW = 1763150000.0
SECRET = "whsec_" + base64.b64encode(b"part4-fixture-secret-32-bytes!!!").decode()
OTHER_SECRET = "whsec_" + base64.b64encode(b"some-other-secret-32-bytes-long!").decode()
SEED = bytes(range(1, 33))
OTHER_SEED = bytes(range(101, 133))
PRIV = Ed25519PrivateKey.from_private_bytes(SEED)
OTHER_PRIV = Ed25519PrivateKey.from_private_bytes(OTHER_SEED)


def pub_b64(priv):
    return base64.b64encode(priv.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)).decode()


def starlette(headers):
    return Headers(raw=[(k.lower().encode("latin-1"), v.encode("latin-1")) for k, v in headers])


def outcome(fn):
    try:
        v = fn()
        return {"ok": {"provider": v.provider, "event_id": v.event_id, "mode": v.mode}}
    except InvalidSignatureError as e:
        return {"err": "INVALID_SIGNATURE", "message": e.message}
    except Exception as e:  # noqa: BLE001
        return {"err": "UNHANDLED", "type": type(e).__name__}


def b64(b: bytes) -> str:
    return base64.b64encode(b).decode()


def standard_cases():
    out = []
    body = b'{"type":"message.received","data":{"id":"msg_1","text":"the fourth"}}'
    ts = int(NOW)

    def sig(secret=SECRET, msg_id="evt_1", t=ts, b=body):
        return standard_webhooks_sign(secret, msg_id, t, b)

    good = [("webhook-id", "evt_1"), ("webhook-timestamp", str(ts)), ("webhook-signature", sig())]
    cases = [
        ("valid", SECRET, good, body, "fixture"),
        ("valid_live_mode", SECRET, good, body, "live"),
        ("no_secret", None, good, body, "fixture"),
        ("empty_secret", "", good, body, "live"),
        ("wrong_secret", OTHER_SECRET, good, body, "fixture"),
        ("secret_without_prefix", SECRET.removeprefix("whsec_"), good, body, "fixture"),
        ("secret_not_base64", "whsec_abc", good, body, "fixture"),
        ("secret_lenient_base64", "whsec_" + SECRET.removeprefix("whsec_")[:10] + "!!" + SECRET.removeprefix("whsec_")[10:], good, body, "fixture"),
        ("tampered_body", SECRET, good, body.replace(b"fourth", b"fifth"), "fixture"),
        ("body_plus_space", SECRET, good, body + b" ", "fixture"),
        ("empty_body_signed", SECRET, [("webhook-id", "evt_1"), ("webhook-timestamp", str(ts)), ("webhook-signature", sig(b=b""))], b"", "fixture"),
        ("stale", SECRET, [("webhook-id", "evt_1"), ("webhook-timestamp", str(ts - 3600)), ("webhook-signature", sig(t=ts - 3600))], body, "fixture"),
        ("future", SECRET, [("webhook-id", "evt_1"), ("webhook-timestamp", str(ts + 301)), ("webhook-signature", sig(t=ts + 301))], body, "fixture"),
        ("edge_300", SECRET, [("webhook-id", "evt_1"), ("webhook-timestamp", str(ts - 300)), ("webhook-signature", sig(t=ts - 300))], body, "fixture"),
        ("edge_301", SECRET, [("webhook-id", "evt_1"), ("webhook-timestamp", str(ts - 301)), ("webhook-signature", sig(t=ts - 301))], body, "fixture"),
        ("missing_id", SECRET, good[1:], body, "fixture"),
        ("missing_ts", SECRET, [good[0], good[2]], body, "fixture"),
        ("missing_sig", SECRET, good[:2], body, "fixture"),
        ("empty_id", SECRET, [("webhook-id", ""), good[1], good[2]], body, "fixture"),
        ("no_headers", SECRET, [], body, "fixture"),
        ("bad_ts", SECRET, [good[0], ("webhook-timestamp", "abc"), good[2]], body, "fixture"),
        ("float_ts", SECRET, [good[0], ("webhook-timestamp", f"{ts}.0"), good[2]], body, "fixture"),
        ("ts_spaces_plus_zeros", SECRET, [good[0], ("webhook-timestamp", f" +00{ts} "), good[2]], body, "fixture"),
        ("ts_underscores", SECRET, [good[0], ("webhook-timestamp", f"{str(ts)[:4]}_{str(ts)[4:]}"), good[2]], body, "fixture"),
        ("ts_double_underscore", SECRET, [good[0], ("webhook-timestamp", f"{str(ts)[:4]}__{str(ts)[4:]}"), good[2]], body, "fixture"),
        ("ts_nbsp", SECRET, [good[0], ("webhook-timestamp", f"\u00a0{ts}\u00a0"), good[2]], body, "fixture"),
        ("ts_huge", SECRET, [good[0], ("webhook-timestamp", "9" * 30), good[2]], body, "fixture"),
        ("ts_too_large_for_float", SECRET, [good[0], ("webhook-timestamp", "9" * 400), good[2]], body, "fixture"),
        ("ts_negative", SECRET, [good[0], ("webhook-timestamp", "-5"), good[2]], body, "fixture"),
        ("header_case", SECRET, [("Webhook-Id", "evt_1"), ("WEBHOOK-TIMESTAMP", str(ts)), ("Webhook-Signature", sig())], body, "fixture"),
        ("duplicate_header_first_wins", SECRET, [("webhook-id", "evt_1"), ("webhook-id", "evt_2"), good[1], good[2]], body, "fixture"),
        ("duplicate_header_first_wrong", SECRET, [("webhook-id", "evt_2"), ("webhook-id", "evt_1"), good[1], good[2]], body, "fixture"),
        ("several_signatures_last_good", SECRET, [good[0], good[1], ("webhook-signature", "v1,AAAA v1,BBBB " + sig())], body, "fixture"),
        ("several_signatures_first_good", SECRET, [good[0], good[1], ("webhook-signature", sig() + " v1,AAAA")], body, "fixture"),
        ("v2_only", SECRET, [good[0], good[1], ("webhook-signature", "v2," + sig().split(",", 1)[1])], body, "fixture"),
        ("v1a_and_good", SECRET, [good[0], good[1], ("webhook-signature", "v1a,x\t" + sig())], body, "fixture"),
        ("tab_and_newline_separators", SECRET, [good[0], good[1], ("webhook-signature", "v1,AAAA\t\n" + sig())], body, "fixture"),
        ("no_comma_entry", SECRET, [good[0], good[1], ("webhook-signature", "v1 " + sig())], body, "fixture"),
        ("signature_without_padding", SECRET, [good[0], good[1], ("webhook-signature", sig().rstrip("="))], body, "fixture"),
        ("signature_bad_length", SECRET, [good[0], good[1], ("webhook-signature", "v1," + b64(b"x" * 32))], body, "fixture"),
        ("non_ascii_v1_signature", SECRET, [good[0], good[1], ("webhook-signature", "v1,\u00e9\u00e9 " + sig())], body, "fixture"),
        ("non_ascii_v2_then_good", SECRET, [good[0], good[1], ("webhook-signature", "v2,\u00e9 " + sig())], body, "fixture"),
        ("id_mismatch", SECRET, [("webhook-id", "evt_other"), good[1], good[2]], body, "fixture"),
        ("non_ascii_id_signed", SECRET, [("webhook-id", "evt_\u00e9"), good[1], ("webhook-signature", sig(msg_id="evt_\u00e9"))], body, "fixture"),
        ("binary_body", SECRET, [good[0], good[1], ("webhook-signature", sig(b=bytes(range(256))))], bytes(range(256)), "fixture"),
    ]
    for name, secret, headers, b, mode in cases:
        out.append({
            "name": name, "provider": "photon", "secret": secret, "headers": headers, "body_b64": b64(b), "mode": mode, "now": NOW,
            "result": outcome(lambda: verify_standard_webhook("photon", secret, starlette(headers), b, mode, now=NOW)),
        })
    signs = []
    for secret, msg_id, t, b in [
        (SECRET, "evt_1", ts, body), (SECRET.removeprefix("whsec_"), "x", 0, b""), (SECRET, "id.with.dots", -7, b"\x00\xff"),
        ("whsec_", "e", 1, b"a"), ("whsec_whsec_" + SECRET.removeprefix("whsec_"), "e", 1, b"a"),
    ]:
        try:
            signs.append({"secret": secret, "msg_id": msg_id, "timestamp": t, "body_b64": b64(b), "signature": standard_webhooks_sign(secret, msg_id, t, b)})
        except Exception as e:  # noqa: BLE001
            signs.append({"secret": secret, "msg_id": msg_id, "timestamp": t, "body_b64": b64(b), "raises": type(e).__name__})
    for bad in ["whsec_abc", "whsec_a", "\u00e9"]:
        try:
            standard_webhooks_sign(bad, "e", 1, b"")
            signs.append({"secret": bad, "msg_id": "e", "timestamp": 1, "body_b64": "", "signature": "?"})
        except Exception as e:  # noqa: BLE001
            signs.append({"secret": bad, "msg_id": "e", "timestamp": 1, "body_b64": "", "raises": type(e).__name__})
    return out, signs


def telnyx_cases():
    out = []
    body = json.dumps({"data": {"id": "tx-evt-1", "event_type": "call.answered", "payload": {}}}).encode()
    ts = str(int(NOW))
    key = pub_b64(PRIV)

    def sig(priv=PRIV, t=ts, b=body):
        return b64(priv.sign(f"{t}|".encode() + b))

    good = [("telnyx-signature-ed25519", sig()), ("telnyx-timestamp", ts)]
    bodies = {
        "int_id": json.dumps({"data": {"id": 12345}}).encode(),
        "float_id": json.dumps({"data": {"id": 1.5}}).encode(),
        "null_id": json.dumps({"data": {"id": None}}).encode(),
        "bool_id": json.dumps({"data": {"id": True}}).encode(),
        "list_id": json.dumps({"data": {"id": [1, "a"]}}).encode(),
        "no_id": json.dumps({"data": {"event_type": "x"}}).encode(),
        "no_data": json.dumps({"id": "x"}).encode(),
        "data_list": json.dumps({"data": ["id"]}).encode(),
        "data_str": json.dumps({"data": "id"}).encode(),
        "not_json": b"not json",
        "json_list": b"[1,2]",
        "bom": b"\xef\xbb\xbf" + body,
        "duplicate_keys": b'{"data": {"id": "a", "id": "b"}}',
        "unicode_id": json.dumps({"data": {"id": "evt-\u00e9-\U0001f600"}}).encode(),
        "big_int_id": b'{"data": {"id": 12345678901234567890}}',
    }
    cases = [
        ("valid", key, good, body, "fixture"),
        ("valid_live", key, good, body, "live"),
        ("no_key", None, good, body, "fixture"),
        ("empty_key", "", good, body, "live"),
        ("wrong_key", pub_b64(OTHER_PRIV), good, body, "fixture"),
        ("signed_by_other_key", key, [("telnyx-signature-ed25519", sig(priv=OTHER_PRIV)), ("telnyx-timestamp", ts)], body, "fixture"),
        ("key_not_base64", "abc", good, body, "fixture"),
        ("key_wrong_length", b64(b"x" * 31), good, body, "fixture"),
        ("key_lenient_base64", key[:8] + "!" + key[8:], good, body, "fixture"),
        ("tampered_body", key, good, body + b" ", "fixture"),
        ("stale", key, [("telnyx-signature-ed25519", sig(t=str(int(NOW) - 400))), ("telnyx-timestamp", str(int(NOW) - 400))], body, "fixture"),
        ("future", key, [("telnyx-signature-ed25519", sig(t=str(int(NOW) + 400))), ("telnyx-timestamp", str(int(NOW) + 400))], body, "fixture"),
        ("missing_sig", key, good[1:], body, "fixture"),
        ("missing_ts", key, good[:1], body, "fixture"),
        ("empty_sig", key, [("telnyx-signature-ed25519", ""), good[1]], body, "fixture"),
        ("bad_ts", key, [good[0], ("telnyx-timestamp", "abc")], body, "fixture"),
        ("ts_huge_float_overflow", key, [good[0], ("telnyx-timestamp", "9" * 400)], body, "fixture"),
        ("ts_raw_text_signed", key, [("telnyx-signature-ed25519", sig(t=" +0" + ts)), ("telnyx-timestamp", " +0" + ts)], body, "fixture"),
        ("ts_raw_text_canonical_signed", key, [("telnyx-signature-ed25519", sig()), ("telnyx-timestamp", " +0" + ts)], body, "fixture"),
        ("sig_not_base64", key, [("telnyx-signature-ed25519", "@@@"), good[1]], body, "fixture"),
        ("sig_wrong_length", key, [("telnyx-signature-ed25519", b64(b"x" * 63)), good[1]], body, "fixture"),
        ("sig_non_ascii", key, [("telnyx-signature-ed25519", "\u00e9"), good[1]], body, "fixture"),
        ("sig_lenient", key, [("telnyx-signature-ed25519", sig()[:5] + "!*" + sig()[5:]), good[1]], body, "fixture"),
        ("sig_high_s", key, [("telnyx-signature-ed25519", b64(base64.b64decode(sig())[:32] + bytes([0xFF] * 32))), good[1]], body, "fixture"),
        ("header_case", key, [("Telnyx-Signature-Ed25519", sig()), ("TELNYX-TIMESTAMP", ts)], body, "fixture"),
        ("duplicate_sig_first_wins", key, [("telnyx-signature-ed25519", sig()), ("telnyx-signature-ed25519", "x"), good[1]], body, "fixture"),
        ("duplicate_sig_first_bad", key, [("telnyx-signature-ed25519", "x"), ("telnyx-signature-ed25519", sig()), good[1]], body, "fixture"),
    ]
    for name, b in bodies.items():
        cases.append((f"body_{name}", key, [("telnyx-signature-ed25519", sig(b=b)), ("telnyx-timestamp", ts)], b, "fixture"))
    for name, k, headers, b, mode in cases:
        out.append({
            "name": name, "public_key": k, "headers": headers, "body_b64": b64(b), "mode": mode, "now": NOW,
            "result": outcome(lambda: verify_telnyx(k, starlette(headers), b, mode, now=NOW)),
        })
    return out, {"public_key": key, "seed_b64": b64(SEED), "other_seed_b64": b64(OTHER_SEED)}


# ---- unicode tables and base64 ----------------------------------------------------------------


def ranges(pred):
    out, start = [], None
    for cp in range(0x110000):
        v = pred(chr(cp))
        if v and start is None:
            start = cp
        if not v and start is not None:
            out.append([start, cp - 1])
            start = None
    if start is not None:
        out.append([start, 0x10FFFF])
    return out


def unicode_tables():
    w, s, d = re.compile(r"\w"), re.compile(r"\s"), re.compile(r"\d")
    extras = {}
    for x in "abcdefghijklmnopqrstuvwxyz":
        r = re.compile(x, re.I)
        extras[x] = [cp for cp in range(0x80, 0x110000) if r.fullmatch(chr(cp))]
    return {
        "python": sys.version.split()[0],
        "unicode": unicodedata.unidata_version,
        "word": ranges(lambda c: bool(w.fullmatch(c))),
        "space": ranges(lambda c: bool(s.fullmatch(c))),
        "digit": ranges(lambda c: bool(d.fullmatch(c))),
        "assigned": ranges(lambda c: unicodedata.category(c) != "Cn"),
        "ignorecase_non_ascii": extras,
    }


def base64_cases():
    out = []
    for s in ["", "QQ==", "QQ=", "QQ", "Q", "QUI=", "QUJD", "QUJDRA==", "QU JD", "QU\nJD", "QUJD====", "QUJD=QUJD",
              "QQ==QUJD", "=QUJD", "Q=Q=", "QU=JD", "-_-_", "QUJD!", "\u00e9", "QUJ", "QUJDR", "QUJDRA", "QUJDRA=",
              "QUJDRA===", "Q===", "QQ=A", "Zm9vYmFy", "Zm9vYg==", "Zm9vYmE=", "Zm9v YmFy", "@@@@", "Zm9=v"]:
        try:
            out.append({"input": s, "bytes_b64": b64(base64.b64decode(s))})
        except Exception as e:  # noqa: BLE001
            out.append({"input": s, "raises": type(e).__name__, "message": str(e)})
    return out


def main() -> None:
    t0 = time.time()
    std, signs = standard_cases()
    tel, keys = telnyx_cases()
    comms = {
        "_meta": {"generator": "apps/backend/tests-golden/domain/gen_domain4.py", "note": "Synthetic test text; Python results are the specification."},
        "state_pairs": state_pairs(),
        "scripts": scripts(),
        "findings": finding_cases(),
        "describe": describe_cases(),
        "escalation": escalation_cases(),
        "client_state": client_state_cases(),
        "call_payload": call_payload_cases(),
        "session_update": session_update_cases(),
    }
    sigs = {
        "_meta": {"generator": "apps/backend/tests-golden/domain/gen_domain4.py", "note": "Fixed test keys only; never real credentials.", "now": NOW, "secret": SECRET, **keys},
        "standard_webhooks": std,
        "standard_webhooks_sign": signs,
        "telnyx": tel,
    }
    for name, doc in [("part4_comms.json", comms), ("part4_signatures.json", sigs), ("part4_unicode.json", unicode_tables()), ("part4_base64.json", base64_cases())]:
        (HERE / name).write_text(json.dumps(doc, indent=1, sort_keys=True, ensure_ascii=True) + "\n")
    print(f"done in {time.time() - t0:.1f}s")


if __name__ == "__main__":
    main()
