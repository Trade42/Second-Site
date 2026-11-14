//! Port of `apps/api/app/domains/permits/extract.py`: technical facts from LADBS work
//! descriptions. Pure, regex-based, deterministic.
//!
//! Every fact keeps the exact source substring and the standard limitations. Permit status is
//! never converted into verification status. Integer facts become floats, as Pydantic stores an
//! `int` in a `float | str | bool` field.

use std::sync::LazyLock;

use regex::Regex;
use ss_core::EvidenceClassification;

use crate::contracts::{ExtractedInfrastructureFact, PermitCategory, ScalarValue};
use crate::py;

pub const LIM_ELECTRICAL: &[&str] = &[
    "Documents electrical service installed under this permit; does not establish current spare capacity.",
    "Service may be shared by the whole building; allocation to a specific unit is not stated.",
    "A finaled permit does not guarantee the condition is unchanged today.",
];
pub const LIM_WATER: &[&str] = &[
    "Documents water service diameter; does not establish current pressure, flow or remaining capacity.",
    "Concealed pipe routes and drain locations are not established by this record.",
    "A finaled permit does not guarantee the condition is unchanged today.",
];
pub const LIM_FIXTURE: &[&str] = &[
    "Documents a fixture installed under this permit; does not prove capacity for additional fixtures.",
    "A finaled permit does not guarantee the fixture still exists.",
];
pub const LIM_GENERIC: &[&str] =
    &["Historical permit description; current condition not verified."];
pub const LIM_SPRINKLER_SYSTEM: &[&str] =
    &["Sprinkler work on record implies a system existed at that time; coverage not verified."];

/// Python would raise while extracting (`int('')`, `1/0`): the text cannot be processed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ExtractError(pub String);

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| py::py_regex($pat));
    };
}

re!(
    AMPS,
    r"(\d{2,4})\s*A(?:MPS?)?\b[^.]{0,40}?SERVICE|SERVICE[^.]{0,20}?(\d{2,4})\s*A(?:MPS?)?\b"
);
re!(VOLTS, r"(\d{3})\s*/\s*(\d{3})\s*V?");
re!(PHASES, r"\b([13])\s*(?:P|PH|PHASE)\b");
re!(
    CIRCUITS,
    r"\((\d+)\)\s*CIRCUITS|(\d+|TWO|THREE|FOUR)\s+CIRCUITS"
);
re!(
    WATER,
    r#"(\d+(?:\.\d+)?|\d/\d|\d\s+\d/\d)\s*(?:INCH|IN\b|")\s*WATER SERVICE"#
);
re!(CAP, r"CAP\s+(\d+|TWO|THREE)\s+SINKS?");
re!(
    SINKS,
    r"\(?(\d+|ONE|TWO|THREE|FOUR)\)?\s+((?:BREAK ROOM|KITCHEN|HAND|MOP|LAV\w*)\s+)?SINKS?"
);
re!(TONS, r"(\d+(?:\.\d+)?)\s*-?\s*TON");
re!(VAV, r"\((\d+)\)\s*VAV");
re!(SPRINKLERS, r"\(?(\d+)\)?\s*(?:FIRE\s*)?(?:SPRINKLER|SPRK)");
re!(
    AREA,
    r"([\d,]{3,7})\s*(?:SQ\.?\s*FT|SQFT|SFQT|SF\b|SQ\. FT)"
);

fn fact(
    fact_type: &str,
    value: ScalarValue,
    unit: Option<&str>,
    text: &str,
    lim: &[&str],
) -> ExtractedInfrastructureFact {
    ExtractedInfrastructureFact {
        fact_type: fact_type.to_owned(),
        value,
        unit: unit.map(str::to_owned),
        source_text: py::strip(text).to_owned(),
        classification: EvidenceClassification::DocumentedHistorical,
        limitations: lim.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// `int(s)` of a digit string, as a float (only ASCII digits are supported here; CPython also
/// accepts other Unicode decimal digits).
fn int_value(s: &str) -> Result<f64, ExtractError> {
    let t = py::strip(s);
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ExtractError(format!(
            "invalid literal for int() with base 10: {}",
            py::str_repr(s)
        )));
    }
    t.parse::<f64>()
        .map_err(|_| ExtractError(format!("invalid literal for int(): {}", py::str_repr(s))))
}

/// `int(_NUM_WORDS.get(n, n))`.
fn count_value(n: &str) -> Result<f64, ExtractError> {
    match n {
        "ONE" => Ok(1.0),
        "TWO" => Ok(2.0),
        "THREE" => Ok(3.0),
        "FOUR" => Ok(4.0),
        "FIVE" => Ok(5.0),
        "SIX" => Ok(6.0),
        other => int_value(other),
    }
}

fn float_value(s: &str) -> Result<f64, ExtractError> {
    py::strip(s).parse::<f64>().map_err(|_| {
        ExtractError(format!(
            "could not convert string to float: {}",
            py::str_repr(s)
        ))
    })
}

/// `_fraction`.
fn fraction(s: &str) -> Result<f64, ExtractError> {
    match s.split_once('/') {
        Some((a, b)) if !b.contains('/') => {
            let (a, b) = (float_value(a)?, float_value(b)?);
            if b == 0.0 {
                return Err(ExtractError("float division by zero".into()));
            }
            Ok(a / b)
        }
        Some(_) => Err(ExtractError(
            "too many values to unpack (expected 2)".into(),
        )),
        None => float_value(s),
    }
}

/// `category_for`.
pub fn category_for(permit_group: Option<&str>, permit_type: Option<&str>) -> PermitCategory {
    let t = py::upper(permit_type.unwrap_or(""));
    let g = py::upper(permit_group.unwrap_or(""));
    if g == "ELECTRICAL" || t == "ELECTRICAL" {
        return PermitCategory::Electrical;
    }
    match t.as_str() {
        "PLUMBING" => PermitCategory::Plumbing,
        "HVAC" => PermitCategory::Hvac,
        "FIRE SPRINKLER" => PermitCategory::FireSprinkler,
        _ if g == "MECHANICAL" => PermitCategory::OtherMechanical,
        _ => PermitCategory::Other,
    }
}

fn group<'h>(c: &regex::Captures<'h>, i: usize) -> Option<&'h str> {
    c.get(i).map(|m| m.as_str()).filter(|s| !s.is_empty())
}

/// `extract_facts`.
pub fn extract_facts(
    category: PermitCategory,
    work_desc: &str,
) -> Result<Vec<ExtractedInfrastructureFact>, ExtractError> {
    let d = py::upper(work_desc);
    let mut facts = Vec::new();
    let first60 = py::prefix_chars(work_desc, 60);

    if category == PermitCategory::Electrical {
        if let Some(m) = AMPS.captures(&d) {
            let n = group(&m, 1).or_else(|| group(&m, 2)).unwrap_or("");
            facts.push(fact(
                "electrical_service_amps",
                int_value(n)?.into(),
                Some("A"),
                &m[0],
                LIM_ELECTRICAL,
            ));
            if let Some(v) = VOLTS.captures(&d) {
                facts.push(fact(
                    "electrical_service_voltage",
                    format!("{}/{}", &v[1], &v[2]).into(),
                    Some("V"),
                    &v[0],
                    LIM_ELECTRICAL,
                ));
            }
            if let Some(ph) = PHASES.captures(&d) {
                facts.push(fact(
                    "electrical_service_phases",
                    int_value(&ph[1])?.into(),
                    Some("phase"),
                    &ph[0],
                    LIM_ELECTRICAL,
                ));
            }
        }
        if let Some(c) = CIRCUITS.captures(&d) {
            let n = group(&c, 1).or_else(|| group(&c, 2)).unwrap_or("");
            facts.push(fact(
                "circuits_added",
                count_value(n)?.into(),
                Some("count"),
                &c[0],
                LIM_GENERIC,
            ));
        }
        if d.contains("TRANSFORMER") {
            facts.push(fact(
                "transformer_installed",
                true.into(),
                None,
                "TRANSFORMER",
                LIM_GENERIC,
            ));
        }
    }

    if category == PermitCategory::Plumbing {
        if let Some(m) = WATER.captures(&d) {
            let g1 = &m[1];
            let value = if !g1.contains(' ') {
                let parts = py::split_whitespace(g1);
                fraction(parts.last().copied().unwrap_or(""))?
            } else {
                let mut total = 0.0;
                for x in py::split_whitespace(g1) {
                    total += fraction(x)?;
                }
                total
            };
            facts.push(fact(
                "water_service_diameter",
                value.into(),
                Some("in"),
                &m[0],
                LIM_WATER,
            ));
        }
        if d.contains("BACKFLOW") {
            facts.push(fact(
                "backflow_preventer",
                true.into(),
                None,
                "BACKFLOW PREVENTER",
                LIM_GENERIC,
            ));
        }
        if let Some(cap) = CAP.captures(&d) {
            facts.push(fact(
                "sinks_capped",
                count_value(&cap[1])?.into(),
                Some("count"),
                &cap[0],
                LIM_FIXTURE,
            ));
        } else if let Some(s) = SINKS.captures(&d) {
            let kind = py::strip(s.get(2).map(|g| g.as_str()).unwrap_or(""))
                .to_lowercase()
                .replace(' ', "_");
            let kind = if kind.is_empty() {
                "unspecified".to_owned()
            } else {
                kind
            };
            facts.push(fact(
                "sink_installed",
                count_value(&s[1])?.into(),
                Some("count"),
                &s[0],
                LIM_FIXTURE,
            ));
            facts.push(fact("sink_type", kind.into(), None, &s[0], LIM_FIXTURE));
        } else if d.contains("KITCHEN SINK") {
            facts.push(fact(
                "sink_installed",
                1.0.into(),
                Some("count"),
                "KITCHEN SINK",
                LIM_FIXTURE,
            ));
        }
        if d.contains("WATER HEATER") {
            facts.push(fact(
                "water_heater_work",
                true.into(),
                None,
                "WATER HEATER",
                LIM_GENERIC,
            ));
        }
        if d.contains("RESTROOM") {
            facts.push(fact(
                "restroom_work",
                true.into(),
                None,
                "RESTROOM",
                LIM_GENERIC,
            ));
        }
        if d.contains("POTABLE WATER") || d.contains("WASTE/VENT") {
            facts.push(fact(
                "water_and_waste_work",
                true.into(),
                None,
                "POTABLE WATER AND WASTE/VENT",
                LIM_WATER,
            ));
        }
    }

    if category == PermitCategory::Hvac {
        if let Some(t) = TONS.captures(&d) {
            facts.push(fact(
                "hvac_unit_tons",
                float_value(&t[1])?.into(),
                Some("ton"),
                &t[0],
                LIM_GENERIC,
            ));
        }
        if let Some(v) = VAV.captures(&d) {
            facts.push(fact(
                "vav_boxes",
                int_value(&v[1])?.into(),
                Some("count"),
                &v[0],
                LIM_GENERIC,
            ));
        }
        if d.contains("T.I") || d.contains("TENANT IMPROVEMENT") || d.contains("HVAC TI") {
            facts.push(fact(
                "hvac_tenant_improvement",
                true.into(),
                None,
                first60,
                LIM_GENERIC,
            ));
        }
    }

    if category == PermitCategory::FireSprinkler {
        if let Some(h) = SPRINKLERS.captures(&d) {
            facts.push(fact(
                "sprinkler_heads_modified",
                int_value(&h[1])?.into(),
                Some("count"),
                &h[0],
                LIM_GENERIC,
            ));
        }
        facts.push(fact(
            "fire_sprinkler_system_present",
            true.into(),
            None,
            first60,
            LIM_SPRINKLER_SYSTEM,
        ));
    }

    if let Some(a) = AREA.captures(&d) {
        facts.push(fact(
            "affected_area_sqft",
            int_value(&a[1].replace(',', ""))?.into(),
            Some("sqft"),
            &a[0],
            LIM_GENERIC,
        ));
    }
    if d.contains("CHANGE OF USE") {
        facts.push(fact(
            "change_of_use",
            true.into(),
            None,
            "CHANGE OF USE",
            LIM_GENERIC,
        ));
    }
    Ok(facts)
}
