"""extract() never reads a negation as yes and never reports a cut number (commission rust-domain-5).

The Rust port has the same cases in apps/backend/crates/domain/tests/call_script_truth.rs; the golden vectors
(apps/backend/tests-golden/domain/gen_domain4.py, TRUTH_ANSWERS) compare the two on every answer.
"""

from __future__ import annotations

import re

import pytest

from app.contracts import CallQuestion
from app.domains.communications.call_script import extract
from app.services.call_orchestration.simulator import DEFAULT_ANSWERS

AMPS = CallQuestion(question="How many amps?", claim_type="tenant_subpanel_amps")
SQFT = CallQuestion(question="Square footage?", claim_type="usable_area_sqft")
PLUMBING = CallQuestion(question="Plumbing?", claim_type="in_unit_plumbing")
HVAC = CallQuestion(question="HVAC?", claim_type="hvac_zonable")
AVAILABLE = CallQuestion(question="When?", claim_type="available_from")

# (answer, value): a negated affirmation is never True.
NEGATED = [
    ("Absolutely not", None),
    ("Absolutely not.", None),
    ("Definitely not.", None),
    ("It does not", None),
    ("it does NOT", None),
    ("it does not", None),
    ("it does n't", None),
    ("There is not a sink.", None),
    ("There's not a sink.", None),
    ("There’s not a sink.", None),
    ("We have not had plumbing there.", None),
    ("Not correct.", None),
    ("That's not correct.", None),
    ("That isn’t correct.", None),
    ("Not definitely.", None),
    ("Not absolutely.", None),
    ("Not yes.", None),
    ("Definitely never.", None),
    ("It has never had a sink.", None),
    ("We have nothing like that.", None),
    ("There is hardly any plumbing.", None),
    ("ıt doeſ not", None),
    ("No, it does not.", False),
    ("No, absolutely not.", False),
    ("Absolutely not, it's all open.", False),
    ("absolutely not – it’s all open", False),
    ("There is no plumbing", False),
    ("There's no sink.", False),
    ("There’s no sink.", False),
    ("It has no plumbing.", False),
    ("We have no plumbing.", False),
    ("No, there's nothing", False),
]

# Plain answers keep the value they had before the fix.
PLAIN = [
    ("Yes, there's a sink in back", True),
    ("No, it's all open", False),
    ("yes", True),
    ("Yeah, there is.", True),
    ("It does.", True),
    ("We have a sink and a floor drain.", True),
    ("That's correct.", True),
    ("Sure thing, there's a sink.", True),
    ("YES THERE IS", True),
    ("yeſ", True),
    ("no", False),
    ("Nope, none.", False),
    ("Not really.", False),
    ("There isn't.", False),
    ("It doesn't.", False),
    ("We don't have one.", False),
    ("No, we don't.", False),
    ("No, never had any.", False),
    ("We don't, no.", False),
    ("no no no", False),
    ("yes " * 300, True),
    ("no " * 300, False),
]

# Mixed, hedged, idiomatic or double-negated answers are unclear.
UNCLEAR = [
    "Yes and no.",
    "yes no",
    "Sure, no problem.",
    "No problem.",
    "No, not a problem.",
    "We have no problem with that.",
    "Yes, it doesn't leak.",
    "Yes, without a doubt.",
    "Yes, it cannot leak.",
    "correct, none",
    "It's not that there isn't a sink.",
    "I wouldn't say there's no plumbing.",
    "It's not like we don't have one.",
    "There is not no sink.",
    "Not no.",
    "I’ll find out, yes",
    "I don’t know.",
    "",
    "Yes, " + "there is a sink and " * 50 + "it does not drain",
]

AMPS_KEEP = [
    ("It's a 400 amp service, 3 phase", 400),
    ("400a", 400),
    ("400 amps", 400),
    ("400-amp panel", 400),
    ("200 AMPS", 200),
    ("about 100a.", 100),
    ("(400 amps)", 400),
    ("Panel:400 amps", 400),
    ("a 120/208 volt, 400 amp service", 400),
    ("150 amps", 150),
    (" 400 amps", 400),
    ("400 amps, yes 400 amps", 400),
    ("400 amps or 400 amps", 400),
    ("200 amp panel, about 80 in use", 200),
    ("It's 400 amps or so", 400),
    ("x" * 3000 + " 400 amps", 400),
]
SQFT_KEEP = [
    ("about 1,128 square feet", 1128),
    ("1,200 sq ft", 1200),
    ("1200sf", 1200),
    ("it's 1,128 SF per CoStar", 1128),
    ("950,000 square feet", 950000),
    ("12,345 sq ft", 12345),
    ("1234567 sf", 1234567),
    ("~1,128 sf", 1128),
    ("1,128 sf", 1128),
    ("1,128 sf, I mean 1,128 sf", 1128),
    ("1,128 sf or 1128 sf", 1128),
]
AMPS_UNCLEAR = [
    "12345 amps",
    "123456 amps",
    "1,200 amps",
    "1.200 amps",
    "1 200 amps",
    "1 200 amps",
    "1, 200 amps",
    "400.50 amps",
    "x400 amps",
    "-400 amps",
    "the panel is 60 amps, service 400 amps",
    "mostly 225 amp service; about 80 amps in use",
    "100 and 200 amps",
    "200 to 400 amps",
    "200-400 amps",
    "200 - 400 amps",
    "200–400 amps",
    "200 or 400 amps",
    "200/400 amps",
    "between 200 and 400 amps",
    "200 amps or 400",
    "200 amps to 400 amps",
    "0 amps",
    "00 amps",
    "0000 amps",
    "٤٠٠ amps",
    "4٠٠ amps",
    "４００ amps",
    "١٢٣45 amps",
    "I don’t know, 100 amps?",
]
SQFT_UNCLEAR = [
    "9,500,000 sf",
    "1234567890 sf",
    "1.200 sq ft",
    "1 128 sq ft",
    "1 128 sq ft",
    "1'128 sq ft",
    "1’128 sq ft",
    "11,28 sf",
    "1,1280 sf",
    "1,,128 sf",
    "1,2 sq ft",
    "1,128,000 sf",
    "1128.500 sf",
    "1128.5 sf",
    "between 1,000 and 1,200 sf",
    "between 900 and 1,000 sq ft",
    "1,000 to 1,200 sf",
    "1,000-1,200 sf",
    "1,128 sf per CoStar, 1,100 sf measured",
    "000 sf",
    "0,000 sf",
    "٩٥٠ sq ft",
    "1١28 sf",
]


@pytest.mark.parametrize(("answer", "value"), NEGATED)
@pytest.mark.parametrize("q", [PLUMBING, HVAC], ids=["plumbing", "hvac"])
def test_a_negated_affirmation_is_never_yes(q, answer, value):
    assert extract(q, answer).value is value


@pytest.mark.parametrize(("answer", "value"), PLAIN)
def test_plain_yes_and_no_keep_their_value(answer, value):
    assert extract(PLUMBING, answer).value is value
    assert extract(HVAC, answer).value is value


@pytest.mark.parametrize("answer", UNCLEAR)
def test_mixed_hedged_and_double_negated_answers_are_unclear(answer):
    assert extract(PLUMBING, answer).value is None


@pytest.mark.parametrize(("answer", "value"), AMPS_KEEP)
def test_amps_read_correctly_keep_their_value(answer, value):
    f = extract(AMPS, answer)
    assert (f.value, f.unit) == (value, "A")


@pytest.mark.parametrize(("answer", "value"), SQFT_KEEP)
def test_area_read_correctly_keeps_its_value(answer, value):
    f = extract(SQFT, answer)
    assert (f.value, f.unit) == (value, "sqft")


@pytest.mark.parametrize("answer", AMPS_UNCLEAR)
def test_cut_joined_or_several_amps_are_unclear(answer):
    f = extract(AMPS, answer)
    assert (f.value, f.unit) == (None, None)


@pytest.mark.parametrize("answer", SQFT_UNCLEAR)
def test_cut_joined_or_several_areas_are_unclear(answer):
    f = extract(SQFT, answer)
    assert (f.value, f.unit) == (None, None)


def test_a_reported_number_is_a_whole_token_of_the_answer():
    # Every number extract reports appears in the answer as a whole token, with or without its thousands commas.
    for q, answers in (
        (AMPS, AMPS_KEEP + [(a, None) for a in AMPS_UNCLEAR]),
        (SQFT, SQFT_KEEP + [(a, None) for a in SQFT_UNCLEAR]),
    ):
        for answer, _ in answers:
            v = extract(q, answer).value
            if v is not None:
                forms = {str(v), f"{v:,}"}
                tokens = set(re.findall(r"(?<![\w.,'’-])[0-9][0-9,]*(?![\w.,]*[0-9])", answer))
                assert forms & tokens, (answer, v)


def test_the_simulators_canned_answers_keep_their_values():
    # apps/api/app/services/call_orchestration/simulator.py, mirrored by the Rust simulator (call_simulator.rs).
    expected = {
        "tenant_subpanel_amps": 200,
        "in_unit_plumbing": True,
        "hvac_zonable": True,
        "usable_area_sqft": 1250,
        "available_from": "It's available December first.",
    }
    assert set(DEFAULT_ANSWERS) == set(expected)
    for claim, answer in DEFAULT_ANSWERS.items():
        assert extract(CallQuestion(question="q", claim_type=claim), answer).value == expected[claim], claim
    for fallback in ("I'm not sure, I'd have to check.", "Not sure, I'd have to check the plans."):
        for q in (AMPS, SQFT, PLUMBING, HVAC, AVAILABLE):
            assert extract(q, fallback).value is None


def test_typographic_apostrophes_read_like_ascii_ones():
    assert extract(PLUMBING, "there’s a sink").value is True
    assert extract(PLUMBING, "We don’t have one.").value is False
    assert extract(PLUMBING, "donʼt have one").value is False
    assert extract(PLUMBING, "It doesn‘t").value is False
    assert extract(AVAILABLE, "I’ll find out").value is None
    # the stored answer and a text value keep the original characters
    f = extract(AVAILABLE, "It’s free in May")
    assert f.answer == f.value == "It’s free in May"


@pytest.mark.parametrize(
    ("q", "answer", "value"),
    [
        (PLUMBING, "yes " * 5000, True),
        (PLUMBING, "not, no, " * 2500, False),
        (PLUMBING, "there is not " * 3000, None),
        (PLUMBING, "never " * 4000 + "no " * 4000, None),
        (PLUMBING, "not no " * 20000, None),
        (AMPS, "400 amps or " * 3000, 400),
    ],
    ids=["yes", "not-comma-no", "there-is-not", "never-then-no", "not-no", "amps-or"],
)
def test_long_answers_follow_the_same_rules(q, answer, value):
    # The helpers are linear (dicts and bisect, not nested scans): 140,000 characters take well under a second.
    assert extract(q, answer).value == value


# Review of rust-domain-5: answers the writer's fix misread (each value differs there), then answers whose value
# the review fixes keep. Mirrored in call_script_truth.rs.
REVIEW_FIXED = [
    (PLUMBING, "that isn\u00b4t correct", None),
    (PLUMBING, "That isn`t correct.", None),
    (PLUMBING, "it sure doesn\u2032t", None),
    (PLUMBING, "that isn\uff07t correct", None),
    (PLUMBING, "It doesn\u00b4t.", False),
    (PLUMBING, "There isn`t.", False),
    (PLUMBING, "We don\u2032t have one.", False),
    (AMPS, "I don\u00b4t know, 200 amps", None),
    (AVAILABLE, "I`ll find out", None),
    (PLUMBING, "There is zero plumbing.", None),
    (PLUMBING, "It has zero sinks.", None),
    (PLUMBING, "There's 0 plumbing.", None),
    (PLUMBING, "Nah, there's a sink in the other suite.", None),
    (HVAC, "Impossible, for sure.", None),
    (PLUMBING, "Sure, that's incorrect.", None),
    (PLUMBING, "Yes, nothing's there.", None),
    (PLUMBING, "It lacks plumbing, for sure.", None),
    (PLUMBING, "Negative, it has a sink next door.", None),
    (PLUMBING, "No, there is zero plumbing.", False),
    (PLUMBING, "It has been removed.", None),
    (PLUMBING, "It has to be added.", None),
    (PLUMBING, "We have to add one.", None),
    (PLUMBING, "We have yet to plumb it.", None),
    (HVAC, "It has to be redone.", None),
    (PLUMBING, "It has been removed, no.", False),
    (AMPS, "It's not 400 amps, it's 200", None),
    (AMPS, "It isn't 200 amps.", None),
    (AMPS, "Definitely not 400 amps.", None),
    (AMPS, "Not even 100 amps.", None),
    (AMPS, "Nowhere near 100 amps.", None),
    (SQFT, "It's not 1,500 sf, it's 1,128.", None),
    (SQFT, "Not 3,500 sq ft, more like 2,000.", None),
    (AMPS, "Less than 100 amps.", None),
    (AMPS, "Under 200 amps.", None),
    (AMPS, "No more than 200 amps", None),
    (AMPS, "At most 60 amps", None),
    (AMPS, "200 amps max", None),
    (AMPS, "200 amps plus", None),
    (SQFT, "Over 1,000 sf.", None),
    (SQFT, "Just under 1,250 sf", None),
    (SQFT, "1,250 sf or less", None),
    (SQFT, "2,000 of 10,000 sf", None),
    (SQFT, "1,200 of 10,000 square feet", None),
    (SQFT, "1,300 sf rentable, 1,128 usable", None),
    (SQFT, "1,300 sf gross", None),
    (SQFT, "1,300 square feet rentable", None),
    (AMPS, "It was upgraded in 2015 a while back.", None),
    (AMPS, "We pay 1500 a month for power.", None),
    (PLUMBING, "No plumbing issues.", None),
    (PLUMBING, "No plumbing problems at all.", None),
    (HVAC, "No zoning issues.", None),
    (PLUMBING, "Sure, I'd have  to check.", None),
    (PLUMBING, "Sure, I'd have\u00a0to check.", None),
    (AMPS, "I'm not  sure, maybe 200 amps", None),
    (AMPS, "I don't\u00a0know, 200 amps?", None),
    (PLUMBING, "It's all\u00a0open.", False),
    (PLUMBING, "Not\u2002really.", False),
]
REVIEW_KEPT = [
    (AMPS, "No, it's 200 amps.", 200),
    (AMPS, "200 amps, not 400.", 200),
    (AMPS, "It's 200 amps, nothing in use yet", 200),
    (AMPS, "It's 400 amps or so", 400),
    (AMPS, "200A service", 200),
    (AMPS, "200 A service", 200),
    (AMPS, "about 200 a.", 200),
    (AMPS, "It's 200 a", 200),
    (AMPS, "60% of 200 amps", 200),
    (AMPS, "About 50% of the 200 amps is in use", 200),
    (SQFT, "2,000 sf of 10,000", 2000),
    (SQFT, "1,128 usable square feet", 1128),
    (SQFT, "1,128 sf usable", 1128),
    (SQFT, "about 1,128 square feet", 1128),
    (PLUMBING, "No plumbing.", False),
    (PLUMBING, "There's no plumbing at all.", False),
    (PLUMBING, "it does\u00a0not", None),
    (PLUMBING, "Yes, there's a sink in back", True),
    (HVAC, "No, it's all open", False),
]


@pytest.mark.parametrize(("q", "answer", "value"), REVIEW_FIXED + REVIEW_KEPT)
def test_negated_bounded_hedged_and_respaced_answers(q, answer, value):
    # Look-alike apostrophes, runs of spaces, more negation words, auxiliaries ("it has to"), negated or bounded
    # numbers, "X of Y sf", rentable or gross areas, the article "a" after a number, "no plumbing issues".
    f = extract(q, answer)
    assert f.value == value and type(f.value) is type(value), (answer, f.value)
    unit = {"tenant_subpanel_amps": "A", "usable_area_sqft": "sqft"}.get(q.claim_type)
    assert f.unit == (unit if isinstance(value, int) and not isinstance(value, bool) else None)
