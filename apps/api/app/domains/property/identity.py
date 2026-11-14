"""PropertyIdentityService: canonical identity + permit/provider record matching.

Rules (docs/DECISIONS.md D-07):
- Never match on street name alone: house number + street name + street type must agree.
- Conflicting street directions (E vs W) are a hard non-match.
- Different unit numbers are a non-match (a neighboring suite's permit is not ours).
- Building-level records (no unit) attach at BUILDING scope; floor-level records attach at FLOOR
  scope with reduced confidence and NEEDS_REVIEW.
"""

from __future__ import annotations

from dataclasses import dataclass

from app.contracts import MatchStatus, PermitScope, PropertyIdentity

from .address import ParsedAddress, parse_street


@dataclass(frozen=True)
class MatchResult:
    matched: bool
    status: MatchStatus
    confidence: float
    scope: PermitScope
    explanation: str


def build_identity(
    street_address: str,
    unit: str | None,
    city: str,
    state: str,
    postal_code: str | None,
    parcel_id: str | None = None,
    latitude: float | None = None,
    longitude: float | None = None,
    provider_ids: dict[str, str] | None = None,
) -> PropertyIdentity:
    p = parse_street(street_address, unit, city, state, postal_code)
    return PropertyIdentity(
        provider_property_ids=provider_ids or {},
        normalized_address=p.normalized(),
        street_address=p.street_line,
        unit_or_suite=p.unit,
        city=city,
        state=state.upper(),
        postal_code=postal_code,
        parcel_id=parcel_id,
        latitude=latitude,
        longitude=longitude,
        identity_match_status=MatchStatus.PROBABLE if parcel_id else MatchStatus.NEEDS_REVIEW,
        identity_match_confidence=0.8 if parcel_id else 0.5,
    )


def parsed_identity(identity: PropertyIdentity) -> ParsedAddress:
    return parse_street(
        identity.street_address, identity.unit_or_suite, identity.city, identity.state, identity.postal_code
    )


def match_record(
    target: PropertyIdentity, record_address: str, record_zip: str | None, record_parcel: str | None
) -> MatchResult:
    t = parsed_identity(target)
    try:
        r = parse_street(record_address)
    except ValueError:
        return MatchResult(
            False, MatchStatus.UNMATCHED, 0.0, PermitScope.UNKNOWN, "unparseable record address"
        )
    why: list[str] = []
    if (t.house_number, t.street_name, t.street_type) != (r.house_number, r.street_name, r.street_type):
        return MatchResult(
            False,
            MatchStatus.UNMATCHED,
            0.0,
            PermitScope.UNKNOWN,
            f"street mismatch: {t.street_line!r} vs {r.street_line!r}",
        )
    conf = 0.6
    why.append("house number, street name and type agree")
    if t.direction and r.direction:
        if t.direction != r.direction:
            return MatchResult(
                False,
                MatchStatus.CONFLICTING,
                0.0,
                PermitScope.UNKNOWN,
                f"street direction conflict {t.direction} vs {r.direction}",
            )
        conf += 0.15
        why.append(f"direction {t.direction} agrees")
    else:
        why.append("direction missing on one side (not used as evidence)")
    if t.postal_code and record_zip:
        if t.postal_code != record_zip:
            return MatchResult(
                False,
                MatchStatus.CONFLICTING,
                0.0,
                PermitScope.UNKNOWN,
                f"ZIP conflict {t.postal_code} vs {record_zip}",
            )
        conf += 0.1
        why.append("ZIP agrees")
    if target.parcel_id and record_parcel:
        if target.parcel_id != record_parcel:
            return MatchResult(
                False,
                MatchStatus.CONFLICTING,
                0.0,
                PermitScope.UNKNOWN,
                f"parcel (APN) conflict {target.parcel_id} vs {record_parcel}",
            )
        conf += 0.15
        why.append("parcel APN agrees")

    if r.unit:
        if t.unit is None:
            return MatchResult(
                False,
                MatchStatus.UNMATCHED,
                0.0,
                PermitScope.UNIT,
                f"record is unit-specific ({r.unit}); target is the whole building",
            )
        if r.unit != t.unit:
            return MatchResult(
                False, MatchStatus.UNMATCHED, 0.0, PermitScope.UNIT, f"different unit ({r.unit} vs {t.unit})"
            )
        scope = PermitScope.UNIT
        why.append(f"unit {r.unit} agrees")
    elif r.floor:
        if t.unit and t.implied_floor == r.floor:
            scope = PermitScope.FLOOR
            conf -= 0.25
            why.append(
                f"floor-level record (floor {r.floor}); suite floor inferred from unit number — review"
            )
        else:
            return MatchResult(
                False,
                MatchStatus.UNMATCHED,
                0.0,
                PermitScope.FLOOR,
                f"floor-level record for floor {r.floor} not tied to target",
            )
    elif r.trailing:
        scope = PermitScope.UNKNOWN
        conf -= 0.3
        why.append(f"unclassified designator {r.trailing!r} — review")
    else:
        scope = PermitScope.BUILDING
        why.append("building-level record (no unit)")

    conf = round(max(0.0, min(1.0, conf)), 3)
    if scope in (PermitScope.FLOOR, PermitScope.UNKNOWN):
        status = MatchStatus.NEEDS_REVIEW
    elif conf >= 0.9:
        status = MatchStatus.EXACT
    elif conf >= 0.7:
        status = MatchStatus.PROBABLE
    else:
        status = MatchStatus.AMBIGUOUS
    return MatchResult(True, status, conf, scope, "; ".join(why))
