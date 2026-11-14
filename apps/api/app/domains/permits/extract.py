"""Extract technical facts from LADBS work descriptions. Pure functions, regex-based, deterministic.

Every fact keeps the exact source substring and standard limitations. Permit status is never
converted into verification status.
"""

from __future__ import annotations

import re

from app.contracts import ExtractedInfrastructureFact, PermitCategory

LIM_ELECTRICAL = [
    "Documents electrical service installed under this permit; does not establish current spare capacity.",
    "Service may be shared by the whole building; allocation to a specific unit is not stated.",
    "A finaled permit does not guarantee the condition is unchanged today.",
]
LIM_WATER = [
    "Documents water service diameter; does not establish current pressure, flow or remaining capacity.",
    "Concealed pipe routes and drain locations are not established by this record.",
    "A finaled permit does not guarantee the condition is unchanged today.",
]
LIM_FIXTURE = [
    "Documents a fixture installed under this permit; does not prove capacity for additional fixtures.",
    "A finaled permit does not guarantee the fixture still exists.",
]
LIM_GENERIC = ["Historical permit description; current condition not verified."]

_NUM_WORDS = {"ONE": 1, "TWO": 2, "THREE": 3, "FOUR": 4, "FIVE": 5, "SIX": 6}


def _fact(fact_type, value, unit, text, lim) -> ExtractedInfrastructureFact:
    return ExtractedInfrastructureFact(
        fact_type=fact_type, value=value, unit=unit, source_text=text.strip(), limitations=lim
    )


def _fraction(s: str) -> float:
    if "/" in s:
        a, b = s.split("/")
        return float(a) / float(b)
    return float(s)


def category_for(permit_group: str | None, permit_type: str | None) -> PermitCategory:
    t = (permit_type or "").upper()
    g = (permit_group or "").upper()
    if g == "ELECTRICAL" or t == "ELECTRICAL":
        return PermitCategory.ELECTRICAL
    return {
        "PLUMBING": PermitCategory.PLUMBING,
        "HVAC": PermitCategory.HVAC,
        "FIRE SPRINKLER": PermitCategory.FIRE_SPRINKLER,
    }.get(t, PermitCategory.OTHER_MECHANICAL if g == "MECHANICAL" else PermitCategory.OTHER)


def extract_facts(category: PermitCategory, work_desc: str) -> list[ExtractedInfrastructureFact]:
    d = work_desc.upper()
    facts: list[ExtractedInfrastructureFact] = []

    if category == PermitCategory.ELECTRICAL:
        m = re.search(
            r"(\d{2,4})\s*A(?:MPS?)?\b[^.]{0,40}?SERVICE|SERVICE[^.]{0,20}?(\d{2,4})\s*A(?:MPS?)?\b", d
        )
        if m:
            amps = int(m.group(1) or m.group(2))
            facts.append(_fact("electrical_service_amps", amps, "A", m.group(0), LIM_ELECTRICAL))
            v = re.search(r"(\d{3})\s*/\s*(\d{3})\s*V?", d)
            if v:
                facts.append(
                    _fact(
                        "electrical_service_voltage",
                        f"{v.group(1)}/{v.group(2)}",
                        "V",
                        v.group(0),
                        LIM_ELECTRICAL,
                    )
                )
            ph = re.search(r"\b([13])\s*(?:P|PH|PHASE)\b", d)
            if ph:
                facts.append(
                    _fact("electrical_service_phases", int(ph.group(1)), "phase", ph.group(0), LIM_ELECTRICAL)
                )
        c = re.search(r"\((\d+)\)\s*CIRCUITS|(\d+|TWO|THREE|FOUR)\s+CIRCUITS", d)
        if c:
            n = c.group(1) or c.group(2)
            facts.append(_fact("circuits_added", int(_NUM_WORDS.get(n, n)), "count", c.group(0), LIM_GENERIC))
        if "TRANSFORMER" in d:
            facts.append(_fact("transformer_installed", True, None, "TRANSFORMER", LIM_GENERIC))

    if category == PermitCategory.PLUMBING:
        m = re.search(r"(\d+(?:\.\d+)?|\d/\d|\d\s+\d/\d)\s*(?:INCH|IN\b|\")\s*WATER SERVICE", d)
        if m:
            facts.append(
                _fact(
                    "water_service_diameter",
                    _fraction(m.group(1).split()[-1])
                    if " " not in m.group(1)
                    else sum(_fraction(x) for x in m.group(1).split()),
                    "in",
                    m.group(0),
                    LIM_WATER,
                )
            )
        if "BACKFLOW" in d:
            facts.append(_fact("backflow_preventer", True, None, "BACKFLOW PREVENTER", LIM_GENERIC))
        cap = re.search(r"CAP\s+(\d+|TWO|THREE)\s+SINKS?", d)
        if cap:
            n = cap.group(1)
            facts.append(_fact("sinks_capped", int(_NUM_WORDS.get(n, n)), "count", cap.group(0), LIM_FIXTURE))
        else:
            s = re.search(
                r"\(?(\d+|ONE|TWO|THREE|FOUR)\)?\s+((?:BREAK ROOM|KITCHEN|HAND|MOP|LAV\w*)\s+)?SINKS?", d
            )
            if s:
                n = s.group(1)
                kind = (s.group(2) or "").strip().lower().replace(" ", "_") or "unspecified"
                facts.append(
                    _fact("sink_installed", int(_NUM_WORDS.get(n, n)), "count", s.group(0), LIM_FIXTURE)
                )
                facts.append(_fact("sink_type", kind, None, s.group(0), LIM_FIXTURE))
            elif "KITCHEN SINK" in d:
                facts.append(_fact("sink_installed", 1, "count", "KITCHEN SINK", LIM_FIXTURE))
        if "WATER HEATER" in d:
            facts.append(_fact("water_heater_work", True, None, "WATER HEATER", LIM_GENERIC))
        if "RESTROOM" in d:
            facts.append(_fact("restroom_work", True, None, "RESTROOM", LIM_GENERIC))
        if "POTABLE WATER" in d or "WASTE/VENT" in d:
            facts.append(_fact("water_and_waste_work", True, None, "POTABLE WATER AND WASTE/VENT", LIM_WATER))

    if category == PermitCategory.HVAC:
        t = re.search(r"(\d+(?:\.\d+)?)\s*-?\s*TON", d)
        if t:
            facts.append(_fact("hvac_unit_tons", float(t.group(1)), "ton", t.group(0), LIM_GENERIC))
        v = re.search(r"\((\d+)\)\s*VAV", d)
        if v:
            facts.append(_fact("vav_boxes", int(v.group(1)), "count", v.group(0), LIM_GENERIC))
        if "T.I" in d or "TENANT IMPROVEMENT" in d or "HVAC TI" in d:
            facts.append(_fact("hvac_tenant_improvement", True, None, work_desc[:60], LIM_GENERIC))

    if category == PermitCategory.FIRE_SPRINKLER:
        h = re.search(r"\(?(\d+)\)?\s*(?:FIRE\s*)?(?:SPRINKLER|SPRK)", d)
        if h:
            facts.append(_fact("sprinkler_heads_modified", int(h.group(1)), "count", h.group(0), LIM_GENERIC))
        facts.append(
            _fact(
                "fire_sprinkler_system_present",
                True,
                None,
                work_desc[:60],
                ["Sprinkler work on record implies a system existed at that time; coverage not verified."],
            )
        )

    a = re.search(r"([\d,]{3,7})\s*(?:SQ\.?\s*FT|SQFT|SFQT|SF\b|SQ\. FT)", d)
    if a:
        facts.append(
            _fact("affected_area_sqft", int(a.group(1).replace(",", "")), "sqft", a.group(0), LIM_GENERIC)
        )
    if "CHANGE OF USE" in d:
        facts.append(_fact("change_of_use", True, None, "CHANGE OF USE", LIM_GENERIC))
    return facts
