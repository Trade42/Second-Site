"""FinancialCalculationService — deterministic, integer-cent arithmetic. No floats in totals.

Double-counting guards:
- Each work item contributes exactly one line (keyed by work_item_id); duplicates rejected.
- Contingency applies to the renovation subtotal only (never to rent, deposits or FF&E).
- Refundable security deposit counts toward upfront cash, NOT toward long-term cost.
- Monthly costs (rent + NNN) appear once as MONTHLY lines; long-term cost multiplies them once.
"""

from __future__ import annotations

from decimal import ROUND_HALF_UP, Decimal
from uuid import UUID

from app.contracts import (
    BusinessExpansionProfile,
    CostKind,
    CostLineItem,
    CostScenario,
    PropertyDigitalProfile,
    RenovationWorkItem,
)

from .cost_catalog import CONTINGENCY_RATE_BP, FF_E_PER_TREATMENT_ROOM


def _round_cents(d: Decimal) -> int:
    return int(d.quantize(Decimal("1"), rounding=ROUND_HALF_UP))


def work_item_totals(w: RenovationWorkItem) -> tuple[int, int, int]:
    q = Decimal(w.quantity)
    return (
        _round_cents(q * w.unit_cost_low),
        _round_cents(q * w.unit_cost_expected),
        _round_cents(q * w.unit_cost_high),
    )


def apply_bp(amount: int, bp: int) -> int:
    return _round_cents(Decimal(amount) * Decimal(bp) / Decimal(10000))


class FinancialCalculationService:
    def scenario(
        self,
        profile: BusinessExpansionProfile,
        prop: PropertyDigitalProfile,
        work_items: list[RenovationWorkItem],
        horizon_months: int | None = None,
        design_version_id: UUID | None = None,
    ) -> CostScenario:
        horizon = horizon_months or profile.ranking_priorities.comparison_horizon_months
        listing = prop.commercial_listing
        lines: list[CostLineItem] = []
        seen: set[UUID] = set()
        reno = [0, 0, 0]
        evidence: list[UUID] = []
        for w in work_items:
            if w.work_item_id in seen:
                raise ValueError(f"work item {w.work_item_id} counted twice")
            seen.add(w.work_item_id)
            lo, ex, hi = work_item_totals(w)
            reno = [reno[0] + lo, reno[1] + ex, reno[2] + hi]
            evidence += w.evidence_ids
            lines.append(
                CostLineItem(
                    line_item_id=f"work:{w.work_item_id}",
                    category=w.category.value,
                    description=w.description,
                    kind=CostKind.ONE_TIME,
                    low=lo,
                    expected=ex,
                    high=hi,
                    source_work_item_id=w.work_item_id,
                    evidence_ids=w.evidence_ids,
                )
            )
        cont = [apply_bp(x, CONTINGENCY_RATE_BP) for x in reno]
        lines.append(
            CostLineItem(
                line_item_id="contingency:renovation",
                category="CONTINGENCY",
                description=f"Contingency {CONTINGENCY_RATE_BP / 100:.0f}% of renovation subtotal",
                kind=CostKind.ONE_TIME,
                low=cont[0],
                expected=cont[1],
                high=cont[2],
            )
        )
        rooms = profile.space_requirements.required_rooms.get("treatment", 0)
        ffe = rooms * FF_E_PER_TREATMENT_ROOM
        if ffe:
            lines.append(
                CostLineItem(
                    line_item_id="ffe:treatment_rooms",
                    category="FF&E",
                    description=f"Furniture/equipment allowance x{rooms} treatment rooms",
                    kind=CostKind.ONE_TIME,
                    low=ffe,
                    expected=ffe,
                    high=ffe,
                )
            )
        rent = listing.asking_rent_monthly if listing and listing.asking_rent_monthly is not None else None
        nnn = listing.nnn_charges_monthly if listing and listing.nnn_charges_monthly is not None else 0
        excluded = [
            "Tenant improvement allowance (not provided)",
            "Free rent concessions",
            "Broker fees",
            "Business operating costs other than occupancy (payroll, supplies)",
        ]
        assumptions = [
            f"Comparison horizon {horizon} months",
            "Rent and NNN held flat (no escalations)",
            "Unit costs are illustrative planning ranges, not quotes",
        ]
        if listing and listing.nnn_is_estimate:
            assumptions.append(
                "NNN/CAM not quoted by the listing; derived estimate used (confirm with broker)"
            )
        elif listing and listing.nnn_charges_monthly is None:
            assumptions.append("NNN/CAM charges unknown — monthly cost may be understated")
        if rent is None:
            assumptions.append("Asking rent unknown — monthly cost understated")
            rent = 0
        lines.append(
            CostLineItem(
                line_item_id="occupancy:rent",
                category="RENT",
                description="Base rent",
                kind=CostKind.MONTHLY,
                low=rent,
                expected=rent,
                high=rent,
            )
        )
        lines.append(
            CostLineItem(
                line_item_id="occupancy:nnn",
                category="NNN",
                description="NNN / CAM charges"
                + (" (estimated; not quoted by the listing)" if listing and listing.nnn_is_estimate else ""),
                kind=CostKind.MONTHLY,
                low=nnn,
                expected=nnn,
                high=nnn,
            )
        )
        deposit = listing.security_deposit if listing and listing.security_deposit else 0
        if deposit:
            lines.append(
                CostLineItem(
                    line_item_id="deposit:security",
                    category="DEPOSIT",
                    description="Refundable security deposit (upfront cash; excluded from long-term cost)",
                    kind=CostKind.REFUNDABLE_DEPOSIT,
                    low=deposit,
                    expected=deposit,
                    high=deposit,
                )
            )

        one_time = [li for li in lines if li.kind == CostKind.ONE_TIME]
        opening = [
            sum(li.low for li in one_time),
            sum(li.expected for li in one_time),
            sum(li.high for li in one_time),
        ]
        monthly = sum(li.expected for li in lines if li.kind == CostKind.MONTHLY)
        return CostScenario(
            project_id=prop.project_id,
            property_id=prop.property_id,
            design_version_id=design_version_id,
            opening_cost_low=opening[0],
            opening_cost_expected=opening[1],
            opening_cost_high=opening[2],
            upfront_cash_expected=opening[1] + deposit,
            monthly_operating_cost=monthly,
            comparison_horizon_months=horizon,
            long_term_cost=opening[1] + monthly * horizon,
            contingency=cont[1],
            line_items=lines,
            assumptions=assumptions,
            excluded_costs=excluded,
            source_evidence_ids=list(dict.fromkeys(evidence)),
        )
