"""Deterministic financial model + ranking acceptance scenarios."""

from __future__ import annotations

import uuid
from decimal import Decimal

import pytest

from app.contracts import AssessmentStatus, CostKind, RenovationWorkItem, WorkCategory
from app.domains.finance.calculator import FinancialCalculationService

from .helpers import analyze_in_memory, demo_profile

ALH, HW = "5380 E ALHAMBRA AVE", "6922 W HOLLYWOOD BLVD"


def test_cheaper_rent_property_ranks_lower_over_36_months():
    """Acceptance (agents-finance): lower rent but more renovation -> more expensive over 36 months."""
    profile = demo_profile(opening_capital=None)  # isolate the cost tradeoff from the capital constraint
    props, sc, _a, _w, _e, ranking = analyze_in_memory(profile)
    alh, hw = props[ALH], props[HW]
    assert alh.commercial_listing.asking_rent_monthly < hw.commercial_listing.asking_rent_monthly
    assert sc[alh.property_id].long_term_cost > sc[hw.property_id].long_term_cost
    assert ranking.ranked_properties[0].property_id == hw.property_id
    assert not any(r.disqualified for r in ranking.ranked_properties)
    assert "Lower rent than" in ranking.explanations[str(alh.property_id)]


def test_tradeoff_flips_over_a_long_horizon():
    profile = demo_profile(opening_capital=None)
    props, sc, *_ = analyze_in_memory(profile, horizon=120)
    assert sc[props[ALH].property_id].long_term_cost < sc[props[HW].property_id].long_term_cost


def test_calculation_is_deterministic():
    p = demo_profile(opening_capital=None)
    a = analyze_in_memory(p)[1]
    b = analyze_in_memory(p)[1]
    assert sorted(s.long_term_cost for s in a.values()) == sorted(s.long_term_cost for s in b.values())


def test_unknown_infrastructure_stays_unknown():
    props, _sc, assessments, *_ = analyze_in_memory(demo_profile())
    for p in props.values():
        elec = next(
            a for a in assessments[p.property_id] if a.requirement_id.endswith("electrical.tenant_service")
        )
        assert elec.status == AssessmentStatus.UNKNOWN  # historical 800A service != available capacity
        assert elec.verification_questions
    alh_elec = next(
        a
        for a in assessments[props[ALH].property_id]
        if a.requirement_id.endswith("electrical.tenant_service")
    )
    assert "800A" in str(alh_elec.observed_value) and alh_elec.evidence_ids


def test_capital_constraint_disqualifies():
    props, sc, _a, _w, _e, ranking = analyze_in_memory(demo_profile(opening_capital=15000000))
    alh = props[ALH]
    assert sc[alh.property_id].upfront_cash_expected > 15000000
    r = next(x for x in ranking.ranked_properties if x.property_id == alh.property_id)
    assert r.disqualified and ranking.ranked_properties[-1].property_id == alh.property_id


def test_no_double_counting():
    profile = demo_profile()
    props, sc, _a, work, *_ = analyze_in_memory(profile)
    hw = props[HW]
    s = sc[hw.property_id]
    one_time = [li for li in s.line_items if li.kind == CostKind.ONE_TIME]
    assert s.opening_cost_expected == sum(li.expected for li in one_time)
    deposit = next(li for li in s.line_items if li.kind == CostKind.REFUNDABLE_DEPOSIT)
    assert (
        s.long_term_cost == s.opening_cost_expected + s.monthly_operating_cost * s.comparison_horizon_months
    )
    assert (
        s.upfront_cash_expected == s.opening_cost_expected + deposit.expected
    )  # deposit counted once, not in LT
    reno = sum(li.expected for li in s.line_items if li.line_item_id.startswith("work:"))
    contingency = next(li for li in s.line_items if li.line_item_id == "contingency:renovation")
    assert contingency.expected == round(reno * 0.15)  # contingency on renovation only, not rent/FF&E
    monthly = [li for li in s.line_items if li.kind == CostKind.MONTHLY]
    assert sum(li.expected for li in monthly) == s.monthly_operating_cost
    with pytest.raises(ValueError, match="counted twice"):
        FinancialCalculationService().scenario(profile, hw, work[hw.property_id] + work[hw.property_id][:1])


def test_money_precision_uses_integer_cents():
    p = demo_profile()
    props, *_ = analyze_in_memory(p)
    w = RenovationWorkItem(
        property_id=uuid.uuid4(),
        category=WorkCategory.FINISHES,
        description="x",
        quantity=Decimal("139.4"),
        unit="m2",
        unit_cost_low=9001,
        unit_cost_expected=15001,
        unit_cost_high=25001,
    )
    s = FinancialCalculationService().scenario(p, props[HW], [w])
    line = next(li for li in s.line_items if li.line_item_id.startswith("work:"))
    assert line.expected == 2091139  # 139.4 * 15001 = 2091139.4 -> half-up to cents
    assert isinstance(s.long_term_cost, int)
