"""Deterministic US street-address normalization (LA-focused, works for Houston too).

Preserves house number, street direction, street name, street type, unit/suite and ZIP.
Also classifies trailing designators found in LADBS `primary_address` ("STE 302", "#314",
"5th Floor", "ROOF", "Level P1") as UNIT / FLOOR / OTHER.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

DIRECTIONS = {"N": "N", "NORTH": "N", "S": "S", "SOUTH": "S", "E": "E", "EAST": "E", "W": "W", "WEST": "W"}
STREET_TYPES = {
    "AVE": "AVE",
    "AVENUE": "AVE",
    "BLVD": "BLVD",
    "BOULEVARD": "BLVD",
    "ST": "ST",
    "STREET": "ST",
    "DR": "DR",
    "DRIVE": "DR",
    "RD": "RD",
    "ROAD": "RD",
    "PL": "PL",
    "PLACE": "PL",
    "WAY": "WAY",
    "LN": "LN",
    "LANE": "LN",
    "CT": "CT",
    "COURT": "CT",
    "PKWY": "PKWY",
    "PARKWAY": "PKWY",
    "HWY": "HWY",
    "HIGHWAY": "HWY",
    "TER": "TER",
    "TERRACE": "TER",
    "CIR": "CIR",
    "SQ": "SQ",
}
UNIT_WORDS = {"STE", "SUITE", "UNIT", "APT", "#", "RM", "ROOM", "SPC", "SPACE"}
FLOOR_PAT = re.compile(
    r"^(?:(\d+)(?:ST|ND|RD|TH)?\s*(?:FL|FLR|FLOOR)\b|(?:FLOOR|FL|LEVEL)\s*([A-Z]?\d+)|(P\d+)\b)"
)


@dataclass(frozen=True)
class ParsedAddress:
    house_number: str
    direction: str | None
    street_name: str
    street_type: str | None
    unit: str | None
    floor: str | None
    trailing: str | None  # unclassified designator, e.g. "ROOF", "LOBBY AND"
    city: str | None = None
    state: str | None = None
    postal_code: str | None = None

    @property
    def street_line(self) -> str:
        parts = [self.house_number, self.direction, self.street_name, self.street_type]
        return " ".join(p for p in parts if p)

    def normalized(self, include_unit: bool = True) -> str:
        line = self.street_line
        if include_unit and self.unit:
            line += f" STE {self.unit}"
        tail = ", ".join(
            p for p in [self.city, " ".join(x for x in [self.state, self.postal_code] if x)] if p
        )
        return f"{line}, {tail}" if tail else line

    @property
    def implied_floor(self) -> str | None:
        """Suite '302' conventionally sits on floor 3. A heuristic only — used for NEEDS_REVIEW matches."""
        if self.floor:
            return self.floor
        if self.unit and self.unit.isdigit() and len(self.unit) >= 3:
            return self.unit[: len(self.unit) - 2]
        return None


def _clean(s: str) -> str:
    s = s.upper().replace(".", " ").replace(",", " , ")
    s = re.sub(r"#\s*", "# ", s)
    return re.sub(r"\s+", " ", s).strip()


def parse_street(
    street: str,
    unit: str | None = None,
    city: str | None = None,
    state: str | None = None,
    postal_code: str | None = None,
) -> ParsedAddress:
    tokens = [t for t in _clean(street).split(" ") if t != ","]
    if not tokens or not re.match(r"^\d+[A-Z]?$", tokens[0]):
        raise ValueError(f"address must start with a house number: {street!r}")
    house = tokens[0]
    i = 1
    direction = None
    if (
        i < len(tokens)
        and tokens[i] in DIRECTIONS
        and i + 1 < len(tokens)
        and tokens[i + 1] not in STREET_TYPES
    ):
        direction = DIRECTIONS[tokens[i]]
        i += 1
    name_parts: list[str] = []
    street_type = None
    while i < len(tokens):
        t = tokens[i]
        if t in STREET_TYPES and name_parts:
            street_type = STREET_TYPES[t]
            i += 1
            break
        name_parts.append(t)
        i += 1
    if street_type is None:
        # No recognizable street type: everything after the number is the name.
        rest = " ".join(name_parts)
        return ParsedAddress(
            house, direction, rest, None, _norm_unit(unit), None, None, _u(city), _u(state), postal_code
        )
    rest = " ".join(tokens[i:]).strip()
    parsed_unit, floor, trailing = classify_designator(rest)
    return ParsedAddress(
        house,
        direction,
        " ".join(name_parts),
        street_type,
        _norm_unit(unit) or parsed_unit,
        floor,
        trailing,
        _u(city),
        _u(state),
        postal_code,
    )


def _u(s: str | None) -> str | None:
    return s.upper().strip() if s else None


def _norm_unit(unit: str | None) -> str | None:
    if not unit:
        return None
    # "110 B" is suite 110B (not suite 110): keep a trailing single-letter designator.
    unit = re.sub(r"^((?:STE|SUITE|UNIT|#)?\s*[A-Z]?\d+)\s+([A-Z])$", r"\1\2", _clean(unit))
    u, _, _ = classify_designator(unit)
    if u:
        return u
    return re.sub(r"^(STE|SUITE|UNIT|#)\s*", "", _clean(unit)).strip() or None


def classify_designator(rest: str) -> tuple[str | None, str | None, str | None]:
    """Return (unit, floor, trailing) for the text after the street type."""
    rest = rest.strip()
    if not rest:
        return None, None, None
    m = FLOOR_PAT.match(rest)
    if m:
        return None, next(g for g in m.groups() if g), None
    toks = rest.split(" ")
    if toks[0] in UNIT_WORDS and len(toks) >= 2 and re.match(r"^[A-Z]?\d+[A-Z]?$", toks[1]):
        return toks[1], None, None
    if re.match(r"^[A-Z]?\d+[A-Z]?$", toks[0]):
        return toks[0], None, None
    return None, None, rest
