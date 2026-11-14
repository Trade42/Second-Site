"""Property ranking: hard constraints + weighted normalized criteria + explicit uncertainty penalty.

Fully deterministic and explainable; no LLM involvement. Owner: agents-finance.
"""

from __future__ import annotations

from decimal import ROUND_HALF_UP, Decimal
from uuid import UUID

from app.contracts import (
    AssessmentStatus,
    BusinessExpansionProfile,
    CostScenario,
    HardConstraintResult,
    InfrastructureAssessment,
    PropertyDigitalProfile,
    PropertyRankingResult,
    RankedProperty,
    Severity,
)
from app.domains.requirements.templates import requirements_for

UNCERTAINTY_PENALTY_PER_UNKNOWN = 0.05


def _usd(cents: int) -> str:
    dollars = (Decimal(cents) / 100).quantize(Decimal("1"), rounding=ROUND_HALF_UP)  # match UI rounding
    return f"${dollars:,}"


class RankingEngine:
    def rank(
        self,
        profile: BusinessExpansionProfile,
        properties: list[PropertyDigitalProfile],
        scenarios: dict[UUID, CostScenario],
        assessments: dict[UUID, list[InfrastructureAssessment]],
        ranking_version: int,
    ) -> PropertyRankingResult:
        pr = profile.ranking_priorities
        raw = {
            "total_cost": pr.total_cost,
            "infrastructure_fit": pr.infrastructure_fit,
            "evidence_certainty": pr.evidence_certainty,
        }
        total_w = sum(raw.values())
        if total_w <= 0:
            raise ValueError("ranking priorities must not all be zero")
        weights = {k: round(v / total_w, 6) for k, v in raw.items()}
        hard_reqs = {r.requirement_id for r in requirements_for(profile) if r.severity == Severity.HARD}

        constraints: list[HardConstraintResult] = []
        disq: set[UUID] = set()
        for p in properties:
            pid = p.property_id
            for a in assessments.get(pid, []):
                if a.requirement_id in hard_reqs:
                    passed = {AssessmentStatus.FAILS: False, AssessmentStatus.UNKNOWN: None}.get(
                        a.status, True
                    )
                    constraints.append(
                        HardConstraintResult(
                            property_id=pid,
                            constraint=a.requirement_id,
                            passed=passed,
                            detail=f"{a.status.value}: {a.uncertainty}",
                        )
                    )
                    if passed is False:
                        disq.add(pid)
            rent = p.commercial_listing.asking_rent_monthly if p.commercial_listing else None
            if profile.monthly_rent_limit is not None and rent is not None:
                ok = rent <= profile.monthly_rent_limit
                constraints.append(
                    HardConstraintResult(
                        property_id=pid,
                        constraint="monthly_rent_limit",
                        passed=ok,
                        detail=f"rent {_usd(rent)} vs limit {_usd(profile.monthly_rent_limit)}",
                    )
                )
                if not ok:
                    disq.add(pid)
            sc = scenarios.get(pid)
            if profile.opening_capital is not None and sc is not None:
                ok = sc.upfront_cash_expected <= profile.opening_capital
                constraints.append(
                    HardConstraintResult(
                        property_id=pid,
                        constraint="opening_capital",
                        passed=ok,
                        detail=f"expected upfront cash {_usd(sc.upfront_cash_expected)} vs capital {_usd(profile.opening_capital)}",
                    )
                )
                if not ok:
                    disq.add(pid)

        costs = {pid: s.long_term_cost for pid, s in scenarios.items() if s.long_term_cost > 0}
        min_cost = min(costs.values()) if costs else None
        scores: dict[str, dict[str, float]] = {}
        penalties: dict[str, float] = {}
        totals: dict[UUID, float] = {}
        unknown_counts: dict[UUID, int] = {}
        for p in properties:
            pid = p.property_id
            al = assessments.get(pid, [])
            n = len(al) or 1
            meets = sum(1 for a in al if a.status == AssessmentStatus.MEETS)
            work = sum(1 for a in al if a.status == AssessmentStatus.MAY_REQUIRE_WORK)
            fails = sum(1 for a in al if a.status == AssessmentStatus.FAILS)
            unknown = sum(1 for a in al if a.status == AssessmentStatus.UNKNOWN)
            unknown_counts[pid] = unknown
            cost_score = round(min_cost / costs[pid], 6) if (min_cost and pid in costs) else 0.0
            fit = round((meets + 0.6 * work) / n * (0 if fails else 1), 6)
            certainty = round(1 - unknown / n, 6)
            s = {"total_cost": cost_score, "infrastructure_fit": fit, "evidence_certainty": certainty}
            scores[str(pid)] = s
            penalties[str(pid)] = round(min(1.0, unknown * UNCERTAINTY_PENALTY_PER_UNKNOWN), 6)
            totals[pid] = round(sum(weights[k] * s[k] for k in weights) - penalties[str(pid)], 6)

        order = sorted(
            properties, key=lambda p: (p.property_id in disq, -totals[p.property_id], str(p.property_id))
        )
        ranked = [
            RankedProperty(
                rank=i + 1,
                property_id=p.property_id,
                total_score=totals[p.property_id],
                disqualified=p.property_id in disq,
                long_term_cost=scenarios[p.property_id].long_term_cost
                if p.property_id in scenarios
                else None,
                unknown_requirement_count=unknown_counts[p.property_id],
            )
            for i, p in enumerate(order)
        ]
        explanations = self._explain(order, scenarios, unknown_counts, disq)
        return PropertyRankingResult(
            project_id=profile.project_id,
            ranking_version=ranking_version,
            criteria_weights=weights,
            hard_constraint_results=constraints,
            ranked_properties=ranked,
            normalized_scores=scores,
            uncertainty_penalties=penalties,
            cost_scenario_ids=[s.scenario_id for s in scenarios.values()],
            explanations=explanations,
        )

    def _explain(self, order, scenarios, unknowns, disq) -> dict[str, str]:
        out: dict[str, str] = {}
        rents = {
            p.property_id: (p.commercial_listing.asking_rent_monthly if p.commercial_listing else None)
            for p in order
        }
        for p in order:
            pid = p.property_id
            sc = scenarios.get(pid)
            parts = [p.property_identity.normalized_address + ":"]
            if pid in disq:
                parts.append("disqualified by a hard constraint.")
            if sc:
                parts.append(
                    f"expected opening cost {_usd(sc.opening_cost_expected)} "
                    f"(range {_usd(sc.opening_cost_low)}–{_usd(sc.opening_cost_high)}), "
                    f"{_usd(sc.monthly_operating_cost)}/mo occupancy, "
                    f"{sc.comparison_horizon_months}-month total {_usd(sc.long_term_cost)}."
                )
                for q in order:
                    other = scenarios.get(q.property_id)
                    if (
                        q.property_id == pid
                        or other is None
                        or rents[pid] is None
                        or rents[q.property_id] is None
                    ):
                        continue
                    if rents[pid] < rents[q.property_id] and sc.long_term_cost > other.long_term_cost:
                        parts.append(
                            f"Lower rent than {q.property_identity.street_address} but "
                            f"{_usd(sc.long_term_cost - other.long_term_cost)} more over "
                            f"{sc.comparison_horizon_months} months because of higher renovation cost."
                        )
            if unknowns.get(pid):
                parts.append(f"{unknowns[pid]} requirement(s) remain UNKNOWN and need verification.")
            out[str(pid)] = " ".join(parts)
        return out
