//! Port of `apps/api/app/domains/property/address.py`: deterministic US street-address
//! normalization (LA-focused, works for Houston too).
//!
//! Preserves house number, street direction, street name, street type, unit/suite and ZIP. Also
//! classifies trailing designators found in LADBS `primary_address` ("STE 302", "#314",
//! "5th Floor", "ROOF", "Level P1") as UNIT / FLOOR / OTHER.

use std::sync::LazyLock;

use regex::Regex;

use crate::py;

/// `DIRECTIONS`.
pub fn direction(token: &str) -> Option<&'static str> {
    match token {
        "N" | "NORTH" => Some("N"),
        "S" | "SOUTH" => Some("S"),
        "E" | "EAST" => Some("E"),
        "W" | "WEST" => Some("W"),
        _ => None,
    }
}

/// `STREET_TYPES`.
pub fn street_type(token: &str) -> Option<&'static str> {
    Some(match token {
        "AVE" | "AVENUE" => "AVE",
        "BLVD" | "BOULEVARD" => "BLVD",
        "ST" | "STREET" => "ST",
        "DR" | "DRIVE" => "DR",
        "RD" | "ROAD" => "RD",
        "PL" | "PLACE" => "PL",
        "WAY" => "WAY",
        "LN" | "LANE" => "LN",
        "CT" | "COURT" => "CT",
        "PKWY" | "PARKWAY" => "PKWY",
        "HWY" | "HIGHWAY" => "HWY",
        "TER" | "TERRACE" => "TER",
        "CIR" => "CIR",
        "SQ" => "SQ",
        _ => return None,
    })
}

/// `UNIT_WORDS`.
pub const UNIT_WORDS: &[&str] = &[
    "STE", "SUITE", "UNIT", "APT", "#", "RM", "ROOM", "SPC", "SPACE",
];

static FLOOR_PAT: LazyLock<Regex> = LazyLock::new(|| {
    py::py_regex(
        r"^(?:(\d+)(?:ST|ND|RD|TH)?\s*(?:FL|FLR|FLOOR)\b|(?:FLOOR|FL|LEVEL)\s*([A-Z]?\d+)|(P\d+)\b)",
    )
});
static HOUSE: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"^\d+[A-Z]?$"));
static UNIT_TOKEN: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"^[A-Z]?\d+[A-Z]?$"));
static HASH_SPACE: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"#\s*"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"\s+"));
static UNIT_LETTER: LazyLock<Regex> =
    LazyLock::new(|| py::py_regex(r"^((?:STE|SUITE|UNIT|#)?\s*[A-Z]?\d+)\s+([A-Z])$"));
static UNIT_PREFIX: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"^(STE|SUITE|UNIT|#)\s*"));
static DIGITS: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"^\d+$"));

/// The address does not start with a house number (Python's `ValueError`, same message).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("address must start with a house number: {}", py::str_repr(.street))]
pub struct AddressError {
    pub street: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAddress {
    pub house_number: String,
    pub direction: Option<String>,
    pub street_name: String,
    pub street_type: Option<String>,
    pub unit: Option<String>,
    pub floor: Option<String>,
    /// Unclassified designator, e.g. "ROOF", "LOBBY AND".
    pub trailing: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
}

fn nonempty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|x| !x.is_empty())
}

impl ParsedAddress {
    pub fn street_line(&self) -> String {
        let parts = [
            Some(self.house_number.as_str()).filter(|x| !x.is_empty()),
            nonempty(&self.direction),
            Some(self.street_name.as_str()).filter(|x| !x.is_empty()),
            nonempty(&self.street_type),
        ];
        parts.into_iter().flatten().collect::<Vec<_>>().join(" ")
    }

    pub fn normalized(&self, include_unit: bool) -> String {
        let mut line = self.street_line();
        if include_unit {
            if let Some(u) = nonempty(&self.unit) {
                line.push_str(" STE ");
                line.push_str(u);
            }
        }
        let state_zip = [nonempty(&self.state), nonempty(&self.postal_code)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let tail = [
            nonempty(&self.city),
            Some(state_zip.as_str()).filter(|x| !x.is_empty()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        if tail.is_empty() {
            line
        } else {
            format!("{line}, {tail}")
        }
    }

    /// Suite '302' conventionally sits on floor 3. A heuristic only, used for NEEDS_REVIEW
    /// matches.
    pub fn implied_floor(&self) -> Option<String> {
        if let Some(f) = nonempty(&self.floor) {
            return Some(f.to_owned());
        }
        let u = nonempty(&self.unit)?;
        let n = u.chars().count();
        if DIGITS.is_match(u) && n >= 3 {
            return Some(u.chars().take(n - 2).collect());
        }
        None
    }
}

/// `_clean`: upper-case, dots to spaces, commas isolated, `#` followed by one space, whitespace
/// collapsed and stripped.
pub fn clean(s: &str) -> String {
    let s = py::upper(s).replace('.', " ").replace(',', " , ");
    let s = HASH_SPACE.replace_all(&s, "# ");
    let s = SPACES.replace_all(&s, " ");
    py::strip(&s).to_owned()
}

/// `_u`.
fn u(s: Option<&str>) -> Option<String> {
    s.filter(|x| !x.is_empty())
        .map(|x| py::strip(&py::upper(x)).to_owned())
}

/// `parse_street`.
pub fn parse_street(
    street: &str,
    unit: Option<&str>,
    city: Option<&str>,
    state: Option<&str>,
    postal_code: Option<&str>,
) -> Result<ParsedAddress, AddressError> {
    let cleaned = clean(street);
    let tokens: Vec<&str> = cleaned.split(' ').filter(|t| *t != ",").collect();
    let first = match tokens.first() {
        Some(t) if HOUSE.is_match(t) => *t,
        _ => {
            return Err(AddressError {
                street: street.to_owned(),
            })
        }
    };
    let house = first.to_owned();
    let mut i = 1;
    let mut dir = None;
    if i < tokens.len() && i + 1 < tokens.len() && street_type(tokens[i + 1]).is_none() {
        if let Some(d) = direction(tokens[i]) {
            dir = Some(d.to_owned());
            i += 1;
        }
    }
    let mut name_parts: Vec<&str> = Vec::new();
    let mut stype = None;
    while i < tokens.len() {
        let t = tokens[i];
        if let Some(st) = street_type(t).filter(|_| !name_parts.is_empty()) {
            stype = Some(st.to_owned());
            i += 1;
            break;
        }
        name_parts.push(t);
        i += 1;
    }
    let postal = postal_code.map(str::to_owned);
    if stype.is_none() {
        // No recognizable street type: everything after the number is the name.
        return Ok(ParsedAddress {
            house_number: house,
            direction: dir,
            street_name: name_parts.join(" "),
            street_type: None,
            unit: norm_unit(unit),
            floor: None,
            trailing: None,
            city: u(city),
            state: u(state),
            postal_code: postal,
        });
    }
    let rest = tokens[i..].join(" ");
    let rest = py::strip(&rest);
    let (parsed_unit, floor, trailing) = classify_designator(rest);
    Ok(ParsedAddress {
        house_number: house,
        direction: dir,
        street_name: name_parts.join(" "),
        street_type: stype,
        unit: norm_unit(unit).or(parsed_unit),
        floor,
        trailing,
        city: u(city),
        state: u(state),
        postal_code: postal,
    })
}

/// `_norm_unit`: "110 B" is suite 110B (not suite 110): a trailing single-letter designator is
/// kept.
pub fn norm_unit(unit: Option<&str>) -> Option<String> {
    let unit = unit.filter(|x| !x.is_empty())?;
    let cleaned = clean(unit);
    let joined = UNIT_LETTER.replace(&cleaned, "${1}${2}");
    if let (Some(u), _, _) = classify_designator(&joined) {
        return Some(u);
    }
    let stripped = UNIT_PREFIX.replace(&cleaned, "");
    let stripped = py::strip(&stripped);
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

/// `classify_designator`: `(unit, floor, trailing)` for the text after the street type.
pub fn classify_designator(rest: &str) -> (Option<String>, Option<String>, Option<String>) {
    let rest = py::strip(rest);
    if rest.is_empty() {
        return (None, None, None);
    }
    if let Some(m) = FLOOR_PAT.captures(rest) {
        let floor = (1..=3)
            .filter_map(|g| m.get(g))
            .map(|g| g.as_str())
            .find(|g| !g.is_empty())
            .map(str::to_owned);
        return (None, floor, None);
    }
    let toks: Vec<&str> = rest.split(' ').collect();
    if UNIT_WORDS.contains(&toks[0]) && toks.len() >= 2 && UNIT_TOKEN.is_match(toks[1]) {
        return (Some(toks[1].to_owned()), None, None);
    }
    if UNIT_TOKEN.is_match(toks[0]) {
        return (Some(toks[0].to_owned()), None, None);
    }
    (None, None, Some(rest.to_owned()))
}
