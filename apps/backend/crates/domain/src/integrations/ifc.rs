//! Port of `apps/api/app/integrations/ifc/__init__.py`, the STEP-text part only: the IFC model
//! reader for the IFC-Bench engineering benchmark (isolated from real property data).
//!
//! **No IfcOpenShell.** Python uses IfcOpenShell when it is installed and otherwise a minimal
//! STEP-text reader for the small committed fixture. There is no IfcOpenShell for Rust, so this
//! port always uses the STEP-text reader ([`read_step_text`]): it recognises
//! `IFCPROPERTYSINGLEVALUE`, `IFCPROPERTYSET` and `IFCRELDEFINESBYPROPERTIES` on one-line entities
//! of the MEP element types in [`MEP_TYPES`]. It is not an IFC parser: no schema, no geometry,
//! no multi-line entities, no type objects, no quantities. [`status`] says so.
//! Observations built from these elements carry `provider='ifc_bench'` and no property id.

use std::path::Path;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde::Serialize;
use ss_core::IntegrationStatus;

use crate::contracts::values::ScalarValue;
use crate::contracts::ProviderStatusEntry;
use crate::py;
use crate::DomainError;

use super::base::status_entry;

/// `MEP_TYPES`.
pub const MEP_TYPES: [&str; 8] = [
    "IFCPIPESEGMENT",
    "IFCDUCTSEGMENT",
    "IFCSANITARYTERMINAL",
    "IFCFLOWTERMINAL",
    "IFCELECTRICDISTRIBUTIONBOARD",
    "IFCAIRTERMINAL",
    "IFCPUMP",
    "IFCVALVE",
];

/// `IfcElement`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IfcElement {
    pub global_id: String,
    pub ifc_type: String,
    pub name: Option<String>,
    /// `"<property set>.<property>"` -> value (a float, a bool, or the raw text).
    pub properties: IndexMap<String, ScalarValue>,
    pub source_file: String,
}

/// `ifcopenshell_available()`: always false in the Rust backend.
pub fn ifcopenshell_available() -> bool {
    false
}

/// The reader's status entry: STEP-text fallback only.
pub fn status() -> ProviderStatusEntry {
    status_entry(
        "ifc_bench",
        "step_text",
        IntegrationStatus::Partial,
        false,
        "Minimal STEP-text reader only (no IfcOpenShell in the Rust backend): MEP elements and \
         their single-value property sets from one-line entities. Not a full IFC parser.",
    )
}

static ENTITY: LazyLock<Regex> =
    LazyLock::new(|| py::py_regex(r"(?m)^#(\d+)\s*=\s*([A-Z0-9_]+)\((.*)\);\s*$"));
static STR: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"'((?:[^']|'')*)'"));
static VALUE: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"IFC[A-Z]+\(([^)]*)\)"));
static REF: LazyLock<Regex> = LazyLock::new(|| py::py_regex(r"#(\d+)"));

/// Python `int()` of a `\d+` match (Unicode decimal digits included).
fn entity_id(s: &str) -> Result<u128, DomainError> {
    py::decimal_and_space_to_ascii(s)
        .parse::<u128>()
        .map_err(|_| DomainError::BadRecord(format!("entity id #{s} out of range")))
}

fn strings(args: &str) -> Vec<String> {
    STR.captures_iter(args)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_owned()))
        .collect()
}

fn refs(args: &str) -> Result<Vec<u128>, DomainError> {
    REF.captures_iter(args)
        .filter_map(|c| c.get(1))
        .map(|m| entity_id(m.as_str()))
        .collect()
}

/// `read_mep_elements(path)`: always [`read_step_text`] (see the module documentation).
pub fn read_mep_elements(path: &Path) -> Result<Vec<IfcElement>, DomainError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| DomainError::BadRecord(format!("{}: {e}", path.display())))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    read_step_text(&text, &name)
}

/// `_read_step_text`: elements, `IFCPROPERTYSINGLEVALUE`, `IFCPROPERTYSET`,
/// `IFCRELDEFINESBYPROPERTIES`, as Python's regular expressions read them. `source_file` is the
/// file name recorded on every element. Where Python raises `IndexError` (a property value with
/// no quoted name, a relation with no references) this returns `BadRecord`.
pub fn read_step_text(text: &str, source_file: &str) -> Result<Vec<IfcElement>, DomainError> {
    // `{int(id): (type, args)}`: a repeated id keeps its first position and its last value.
    let mut ents: IndexMap<u128, (String, String)> = IndexMap::new();
    for c in ENTITY.captures_iter(text) {
        let (Some(id), Some(t), Some(args)) = (c.get(1), c.get(2), c.get(3)) else {
            continue;
        };
        ents.insert(
            entity_id(id.as_str())?,
            (t.as_str().to_owned(), args.as_str().to_owned()),
        );
    }
    let mut props: IndexMap<u128, (String, ScalarValue)> = IndexMap::new();
    for (eid, (t, args)) in &ents {
        if t == "IFCPROPERTYSINGLEVALUE" {
            let name = strings(args).into_iter().next().ok_or_else(|| {
                DomainError::BadRecord(format!("#{eid} IFCPROPERTYSINGLEVALUE has no name"))
            })?;
            let raw = VALUE
                .captures(args)
                .and_then(|c| c.get(1))
                .map_or(String::new(), |m| m.as_str().trim_matches('\'').to_owned());
            let val = match py::float_of_str(&raw) {
                Some(f) => ScalarValue::Number(f),
                None => match raw.as_str() {
                    ".T." => ScalarValue::Bool(true),
                    ".F." => ScalarValue::Bool(false),
                    _ => ScalarValue::Text(raw),
                },
            };
            props.insert(*eid, (name, val));
        }
    }
    let mut psets: IndexMap<u128, (String, Vec<u128>)> = IndexMap::new();
    for (eid, (t, args)) in &ents {
        if t == "IFCPROPERTYSET" {
            let strs = strings(args);
            let name = strs.get(1).cloned().unwrap_or_else(|| "Pset".into());
            let tail = args.split_once('(').map_or(args.as_str(), |(_, rest)| rest);
            psets.insert(*eid, (name, refs(tail)?));
        }
    }
    let mut elem_props: IndexMap<u128, IndexMap<String, ScalarValue>> = IndexMap::new();
    for (eid, (t, args)) in &ents {
        if t == "IFCRELDEFINESBYPROPERTIES" {
            let r = refs(args)?;
            let (pset_id, targets) = r.split_last().ok_or_else(|| {
                DomainError::BadRecord(format!(
                    "#{eid} IFCRELDEFINESBYPROPERTIES has no references"
                ))
            })?;
            if let Some((pname, prop_ids)) = psets.get(pset_id) {
                for target in targets {
                    for pid in prop_ids {
                        if let Some((k, v)) = props.get(pid) {
                            elem_props
                                .entry(*target)
                                .or_default()
                                .insert(format!("{pname}.{k}"), v.clone());
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    for (eid, (t, args)) in &ents {
        if MEP_TYPES.contains(&t.as_str()) {
            let strs = strings(args);
            out.push(IfcElement {
                global_id: strs.first().cloned().unwrap_or_else(|| eid.to_string()),
                ifc_type: t.clone(),
                name: strs.get(1).cloned(),
                properties: elem_props.get(eid).cloned().unwrap_or_default(),
                source_file: source_file.to_owned(),
            });
        }
    }
    Ok(out)
}
