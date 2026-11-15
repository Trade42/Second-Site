//! Port of `apps/api/app/domains/feasibility/engine.py`: the Infrastructure Feasibility Engine
//! (deterministic rules over sourced evidence).
//!
//! Inputs: `BusinessExpansionProfile`, `InfrastructureRequirement[]`, `PropertyDigitalProfile`,
//! `SourceEvidence[]`. Outputs: `InfrastructureAssessment[]` + `RenovationWorkItem[]`.
//!
//! Truthfulness rules (Python's, unchanged):
//! - historical permit facts never yield MEETS for current-capacity requirements; at most UNKNOWN
//!   or MAY_REQUIRE_WORK with explicit limitations and verification questions;
//! - UNKNOWN is never MEETS; unknowns carry a cost allowance range so ranking reflects the risk.
//!
//! Every string (observed values, limitations, verification questions, uncertainty, proposed
//! work, work-item descriptions and assumptions) is Python's, byte for byte.

use std::collections::{HashMap, HashSet};

use ss_core::{
    new_id, BusinessExpansionProfile, Contract, CurrencyCode, DataOrigin, EvidenceClassification,
};
use uuid::Uuid;

use crate::contracts::{
    AssessmentStatus, InfrastructureAssessment, InfrastructureRequirement, PermitScope,
    PropertyDigitalProfile, PyDecimal, RenovationWorkItem, ReviewStatus, ScalarValue,
    SourceEvidence, WorkCategory,
};
use crate::error::DomainError;
use crate::finance::cost_catalog::CostKey;
use crate::py;

/// Evidence classifications the engine reasons over (Python's `USABLE`). `DERIVED_ESTIMATE`,
/// `UNKNOWN` and `CONFLICTING` evidence is ignored.
pub const USABLE: &[EvidenceClassification] = &[
    EvidenceClassification::DocumentedHistorical,
    EvidenceClassification::GeometryMeasured,
    EvidenceClassification::OwnerReported,
    EvidenceClassification::ProfessionallyVerified,
    EvidenceClassification::VisuallyObserved,
];

/// The limitation line an assessment carries when evidence it would have read was left out by
/// [`counts_for`] (not in Python).
pub const SIMULATED_NOT_COUNTED: &str = "A simulated answer on this point exists (scripted demo data, not a statement about this property); it was not counted, because this listing is not demo data.";

/// True when the property's listing is itself invented demo data (`FIXTURE_SYNTHETIC`). A
/// property without a listing is not demo data.
pub fn synthetic_listing(prop: &PropertyDigitalProfile) -> bool {
    prop.commercial_listing
        .as_ref()
        .is_some_and(|l| l.data_origin == DataOrigin::FixtureSynthetic)
}

/// The one rule on what evidence may move a property's feasibility, costs, ranking, bill and
/// payment plan (all of them start from [`FeasibilityEngine::evaluate`], which applies it):
/// `FIXTURE_SYNTHETIC` evidence (a simulated call's scripted answer, any invented record) counts
/// only for a property whose listing is itself `FIXTURE_SYNTHETIC`. For every other listing
/// (`LIVE`, `FIXTURE_REAL`, `USER_PROVIDED`, none) it is left out: fixture data never passes for
/// real data. Evidence of every other origin counts as before.
///
/// Deliberate difference from Python, which counts evidence whatever its origin.
pub fn counts_for(prop: &PropertyDigitalProfile, e: &SourceEvidence) -> bool {
    e.data_origin != DataOrigin::FixtureSynthetic || synthetic_listing(prop)
}

/// The observation types each rule of [`FeasibilityEngine::evaluate`] reads, by requirement id
/// suffix. Keep in step with the `of_type` calls there.
fn types_read(requirement_id: &str) -> &'static [&'static str] {
    if requirement_id.ends_with("electrical.tenant_service") {
        &["electrical_service_amps", "tenant_subpanel_amps"]
    } else if requirement_id.ends_with("plumbing.treatment_sinks") {
        &[
            "sink_installed",
            "in_unit_plumbing",
            "water_service_diameter",
        ]
    } else if requirement_id.ends_with("plumbing.water_service") {
        &[
            "water_service_diameter",
            "water_and_waste_work",
            "sink_installed",
        ]
    } else if requirement_id.ends_with("hvac.ventilation") {
        &["hvac_tenant_improvement", "vav_boxes", "hvac_zonable"]
    } else {
        &[]
    }
}

/// A work-item quantity as Python passes it to `_work`: an `int` or a `float`. It becomes
/// `Decimal(str(qty))`, so `Int(5)` is `"5"` and `Float(139.4)` is `"139.4"`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Quantity {
    Int(i64),
    Float(f64),
}

/// `_work`: one `RenovationWorkItem` priced from the cost catalog, validated like Pydantic would
/// (quantity `> 0`, unit costs ordered).
#[allow(clippy::too_many_arguments)]
pub fn work_item(
    property_id: Uuid,
    key: CostKey,
    category: WorkCategory,
    description: String,
    qty: Quantity,
    assumptions: Vec<String>,
    evidence_ids: Vec<Uuid>,
    assessment_id: Option<Uuid>,
) -> Result<RenovationWorkItem, DomainError> {
    let cost = key.unit_cost();
    let quantity = match qty {
        Quantity::Int(n) => PyDecimal::from(n),
        Quantity::Float(f) => PyDecimal(py::decimal_of_f64(f).ok_or_else(|| {
            DomainError::BadRecord(format!(
                "work item quantity {} is not a finite decimal",
                py::float_repr(f)
            ))
        })?),
    };
    let mut assumptions = assumptions;
    assumptions.push(format!("cost_key={}", key.as_str()));
    let w = RenovationWorkItem {
        work_item_id: new_id(),
        property_id,
        category,
        description,
        quantity,
        unit: cost.unit.to_owned(),
        currency: CurrencyCode::default(),
        unit_cost_low: cost.low,
        unit_cost_expected: cost.expected,
        unit_cost_high: cost.high,
        requires_professional_quote: true,
        assumptions,
        evidence_ids,
        source_assessment_id: assessment_id,
    };
    w.validate()?;
    Ok(w)
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn ids(items: &[&SourceEvidence]) -> Vec<Uuid> {
    items.iter().map(|e| e.evidence_id).collect()
}

/// `float(e.extracted_value)`.
fn value_float(e: &SourceEvidence) -> Result<f64, DomainError> {
    py::float_of_value(&e.extracted_value)
        .map_err(|m| DomainError::BadRecord(format!("evidence {}: {m}", e.evidence_id)))
}

/// `float(req.required_value)`.
fn required_float(req: &InfrastructureRequirement) -> Result<f64, DomainError> {
    let bad =
        |m: String| DomainError::BadRecord(format!("requirement {}: {m}", req.requirement_id));
    match &req.required_value {
        Some(ScalarValue::Number(f)) => Ok(*f),
        Some(ScalarValue::Bool(b)) => Ok(if *b { 1.0 } else { 0.0 }),
        Some(ScalarValue::Text(s)) => py::float_of_str(s).ok_or_else(|| {
            bad(format!(
                "could not convert string to float: {}",
                py::str_repr(s)
            ))
        }),
        None => Err(bad(
            "float() argument must be a string or a real number, not 'NoneType'".into(),
        )),
    }
}

/// `int(req.required_value)`.
fn required_int(req: &InfrastructureRequirement) -> Result<i64, DomainError> {
    let bad =
        |m: String| DomainError::BadRecord(format!("requirement {}: {m}", req.requirement_id));
    match &req.required_value {
        Some(ScalarValue::Number(f)) => py::int_of_float(*f).map_err(bad),
        Some(ScalarValue::Bool(b)) => Ok(i64::from(*b)),
        Some(ScalarValue::Text(s)) => py::int_of_str(s).map_err(bad),
        None => Err(bad(
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
                .into(),
        )),
    }
}

/// `str(int(x))` for a float taken from evidence.
fn int_text(e: &SourceEvidence, x: f64) -> Result<String, DomainError> {
    py::int_str_of_float(x)
        .map_err(|m| DomainError::BadRecord(format!("evidence {}: {m}", e.evidence_id)))
}

/// `max(items, key=lambda e: float(e.extracted_value))`: the first item with the largest value.
/// `items` is never empty here.
fn max_by_value<'a>(
    items: &[&'a SourceEvidence],
) -> Result<(&'a SourceEvidence, f64), DomainError> {
    let mut best: Option<(&SourceEvidence, f64)> = None;
    for e in items {
        let k = value_float(e)?;
        // Python replaces the running maximum only when the new key is strictly greater.
        let replace = match best {
            None => true,
            Some((_, bk)) => k.partial_cmp(&bk) == Some(std::cmp::Ordering::Greater),
        };
        if replace {
            best = Some((e, k));
        }
    }
    best.ok_or_else(|| DomainError::Value("max() arg is an empty sequence".into()))
}

/// The listing's `existing_buildout` (`_buildout`).
fn buildout(prop: &PropertyDigitalProfile) -> Option<&str> {
    prop.commercial_listing
        .as_ref()
        .and_then(|l| l.existing_buildout.as_deref())
}

/// `x or default` for an optional string.
fn or_str<'a>(v: Option<&'a str>, default: &'a str) -> &'a str {
    match v {
        Some(s) if !s.is_empty() => s,
        _ => default,
    }
}

/// Uncertainty of the enclosed-rooms assessment when no room program is stated (not in Python).
pub const NO_PROGRAM_ROOMS: &str =
    "No room program stated yet; room partitions are not assessed or priced.";
/// Uncertainty of the area assessment when its 0 m2 comes only from an empty room program.
pub const NO_PROGRAM_AREA: &str =
    "No room program or minimum area stated yet; the area needed is not assessed.";
/// Verification question on both of those assessments.
pub const NO_PROGRAM_QUESTION: &str = "How many rooms of each type does the business need?";

/// True when the profile states no room counts: `required_rooms` is empty or every count is 0.
/// A negative count is not "no program"; it stays an error downstream.
pub fn no_room_program(profile: &BusinessExpansionProfile) -> bool {
    profile
        .space_requirements
        .required_rooms
        .values()
        .all(|n| *n == 0)
}

/// `FeasibilityEngine`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FeasibilityEngine;

impl FeasibilityEngine {
    pub fn new() -> Self {
        Self
    }

    /// `FeasibilityEngine.evaluate`. Only evidence whose `property_id` is this property's,
    /// whose classification is in [`USABLE`] and which [`counts_for`] this property is
    /// considered, in the order given.
    ///
    /// Errors where Python raises: [`DomainError::Validation`] for an assessment or work item
    /// Pydantic would reject (for example a negative room count gives a negative quantity);
    /// [`DomainError::BadRecord`] when an evidence value or a requirement value cannot be
    /// converted with `float()`/`int()`.
    ///
    /// Deliberate difference from Python: a profile whose room program is empty (or lists only
    /// zero counts, see [`no_room_program`]) does not fail. Python builds a partitions work item
    /// of quantity 0 and Pydantic rejects it, which fails the whole property search while the
    /// person has simply not said how many rooms they need yet. Here the enclosed-rooms
    /// requirement then gets no work item (there is nothing to build) and an UNKNOWN assessment
    /// saying the room program is not stated, and an area requirement of 0 m2 that comes only
    /// from that empty program is UNKNOWN instead of being compared with the listing area. Any
    /// profile with a non-zero room count takes Python's path unchanged, and a zero quantity that
    /// comes from negative counts is still a validation error.
    pub fn evaluate(
        &self,
        profile: &BusinessExpansionProfile,
        requirements: &[InfrastructureRequirement],
        prop: &PropertyDigitalProfile,
        evidence: &[SourceEvidence],
    ) -> Result<(Vec<InfrastructureAssessment>, Vec<RenovationWorkItem>), DomainError> {
        let usable: Vec<&SourceEvidence> = evidence
            .iter()
            .filter(|e| {
                e.property_id == Some(prop.property_id)
                    && USABLE.contains(&e.evidence_classification)
            })
            .collect();
        // Not in Python (see `counts_for`): synthetic evidence does not count for a listing that
        // is not itself synthetic. The observation types left out are remembered so that the
        // assessments reading them can say so.
        let (ev, left_out): (Vec<&SourceEvidence>, Vec<&SourceEvidence>) =
            usable.into_iter().partition(|e| counts_for(prop, e));
        let left_out: HashSet<&str> = left_out
            .iter()
            .map(|e| e.observation_type.as_str())
            .collect();
        let unit_permit_evidence: HashSet<Uuid> = prop
            .permit_records
            .iter()
            .filter(|p| p.scope == PermitScope::Unit)
            .flat_map(|p| p.source_evidence_ids.iter().copied())
            .collect();
        let mut by_type: HashMap<&str, Vec<&SourceEvidence>> = HashMap::new();
        for e in &ev {
            by_type
                .entry(e.observation_type.as_str())
                .or_default()
                .push(e);
        }
        let of_type =
            |t: &str| -> Vec<&SourceEvidence> { by_type.get(t).cloned().unwrap_or_default() };
        let listing = prop.commercial_listing.as_ref();
        let mut assessments: Vec<InfrastructureAssessment> = Vec::new();
        let mut work: Vec<RenovationWorkItem> = Vec::new();
        let (pid, prj) = (prop.property_id, prop.project_id);
        let open_shell = buildout(prop) == Some("open_shell");
        let no_program = no_room_program(profile);
        let base =
            |rid: &str, status: AssessmentStatus, uncertainty: &str| InfrastructureAssessment {
                assessment_id: new_id(),
                project_id: prj,
                property_id: pid,
                requirement_id: rid.to_owned(),
                status,
                observed_value: None,
                required_value: None,
                evidence_ids: Vec::new(),
                limitations: Vec::new(),
                proposed_work: Vec::new(),
                verification_questions: Vec::new(),
                uncertainty: uncertainty.to_owned(),
                review_status: ReviewStatus::Automated,
            };
        let checked =
            |a: InfrastructureAssessment| -> Result<InfrastructureAssessment, DomainError> {
                a.validate()?;
                Ok(a)
            };

        for req in requirements {
            let rid = req.requirement_id.as_str();
            if rid.ends_with("space.min_area") {
                let area = listing.and_then(|l| l.available_area_sqm);
                // Not in Python (see `evaluate`): 0 m2 asked because no rooms were stated is
                // not a requirement the listing area can meet.
                if no_program && required_float(req)? == 0.0 {
                    assessments.push(checked(InfrastructureAssessment {
                        observed_value: area.map(ScalarValue::Number),
                        required_value: req.required_value.clone(),
                        verification_questions: strings(&[NO_PROGRAM_QUESTION]),
                        ..base(rid, AssessmentStatus::Unknown, NO_PROGRAM_AREA)
                    })?);
                    continue;
                }
                let a = match area {
                    None => InfrastructureAssessment {
                        required_value: req.required_value.clone(),
                        verification_questions: strings(&["What is the rentable and usable area?"]),
                        ..base(
                            rid,
                            AssessmentStatus::Unknown,
                            "Available area not provided by any source.",
                        )
                    },
                    Some(area) => {
                        let ok = area >= required_float(req)?;
                        InfrastructureAssessment {
                            observed_value: Some(ScalarValue::Number(area)),
                            required_value: req.required_value.clone(),
                            limitations: strings(&[
                                "Area comes from the listing (not measured); usable area may be lower.",
                            ]),
                            verification_questions: strings(&[
                                "Confirm usable (not rentable) square footage.",
                            ]),
                            review_status: ReviewStatus::NeedsReview,
                            ..base(
                                rid,
                                if ok {
                                    AssessmentStatus::MayRequireWork
                                } else {
                                    AssessmentStatus::Fails
                                },
                                "Listing area, not a measured floor plan.",
                            )
                        }
                    }
                };
                assessments.push(checked(a)?);
            } else if rid.ends_with("space.enclosed_rooms") {
                let n = required_int(req)?;
                // Not in Python (see `evaluate`): no rooms stated means no partitions to price.
                if n == 0 && no_program {
                    assessments.push(checked(InfrastructureAssessment {
                        required_value: Some(ScalarValue::from(n)),
                        observed_value: Some(ScalarValue::from(or_str(buildout(prop), "unknown"))),
                        verification_questions: strings(&[NO_PROGRAM_QUESTION]),
                        ..base(rid, AssessmentStatus::Unknown, NO_PROGRAM_ROOMS)
                    })?);
                    continue;
                }
                let key = if open_shell {
                    CostKey::PartitionRoomOpenShell
                } else {
                    CostKey::PartitionRoomOfficeBuildout
                };
                let a = checked(InfrastructureAssessment {
                    required_value: Some(ScalarValue::from(n)),
                    observed_value: Some(ScalarValue::from(or_str(buildout(prop), "unknown"))),
                    proposed_work: vec![format!("Build or adapt {n} enclosed rooms")],
                    verification_questions: strings(&[
                        "Can existing partitions be reused for treatment rooms?",
                    ]),
                    ..base(
                        rid,
                        AssessmentStatus::MayRequireWork,
                        "Existing partition layout not measured.",
                    )
                })?;
                let assessment_id = a.assessment_id;
                assessments.push(a);
                work.push(work_item(
                    pid,
                    key,
                    WorkCategory::Partitions,
                    format!(
                        "Partitions for {n} rooms ({})",
                        or_str(buildout(prop), "unknown build-out")
                    ),
                    Quantity::Int(n),
                    strings(&["Room count from business profile"]),
                    Vec::new(),
                    Some(assessment_id),
                )?);
            } else if rid.ends_with("electrical.tenant_service") {
                let amps = of_type("electrical_service_amps");
                let reported = of_type("tenant_subpanel_amps");
                let questions = strings(&[
                    "What amperage is allocated to the tenant space's subpanel, and how much is in use?",
                ]);
                if !reported.is_empty() {
                    let (best, best_value) = max_by_value(&reported)?;
                    let enough = best_value >= required_float(req)?;
                    let mut limitations = best.limitations.clone();
                    limitations.push(
                        "Reported capacity is not verified load data; confirm with an electrician."
                            .to_owned(),
                    );
                    let mut evidence_ids = ids(&reported);
                    evidence_ids.extend(ids(&amps));
                    let a = checked(InfrastructureAssessment {
                        observed_value: Some(ScalarValue::Text(format!(
                            "{}A tenant panel (broker-reported)",
                            int_text(best, best_value)?
                        ))),
                        required_value: req.required_value.clone(),
                        evidence_ids,
                        limitations,
                        verification_questions: strings(&[
                            "Can an electrician confirm the panel rating and current load?",
                        ]),
                        review_status: ReviewStatus::NeedsReview,
                        ..base(
                            rid,
                            if enough {
                                AssessmentStatus::Meets
                            } else {
                                AssessmentStatus::MayRequireWork
                            },
                            "Broker statement, not a load calculation or inspection.",
                        )
                    })?;
                    let (assessment_id, evidence_ids) = (a.assessment_id, a.evidence_ids.clone());
                    assessments.push(a);
                    work.push(work_item(
                        pid,
                        if enough {
                            CostKey::ElectricalCircuitsReportedCapacity
                        } else {
                            CostKey::ElectricalSubpanelAllowance
                        },
                        WorkCategory::Electrical,
                        if enough {
                            "Dedicated circuits for treatment equipment"
                        } else {
                            "Allowance: subpanel/circuit upgrade (reported capacity below requirement)"
                        }
                        .to_owned(),
                        Quantity::Int(1),
                        strings(&["Based on broker-reported panel capacity"]),
                        evidence_ids,
                        Some(assessment_id),
                    )?);
                    continue;
                }
                let a = if !amps.is_empty() {
                    let (best, best_value) = max_by_value(&amps)?;
                    InfrastructureAssessment {
                        observed_value: Some(ScalarValue::Text(format!(
                            "{}A building service (historical permit)",
                            int_text(best, best_value)?
                        ))),
                        required_value: req.required_value.clone(),
                        evidence_ids: ids(&amps),
                        limitations: best.limitations.clone(),
                        verification_questions: questions,
                        review_status: ReviewStatus::NeedsReview,
                        ..base(
                            rid,
                            AssessmentStatus::Unknown,
                            "A documented building service does not establish capacity available to this unit.",
                        )
                    }
                } else {
                    InfrastructureAssessment {
                        required_value: req.required_value.clone(),
                        verification_questions: questions,
                        ..base(
                            rid,
                            AssessmentStatus::Unknown,
                            "No electrical service rating found in permits or listing.",
                        )
                    }
                };
                let a = checked(a)?;
                let (assessment_id, evidence_ids) = (a.assessment_id, a.evidence_ids.clone());
                assessments.push(a);
                work.push(work_item(
                    pid,
                    CostKey::ElectricalSubpanelAllowance,
                    WorkCategory::Electrical,
                    "Allowance: possible tenant subpanel/circuit upgrade (capacity unknown)"
                        .to_owned(),
                    Quantity::Int(1),
                    strings(&["Unknown capacity carried as an allowance range"]),
                    evidence_ids,
                    Some(assessment_id),
                )?);
            } else if rid.ends_with("plumbing.treatment_sinks") {
                let n = required_int(req)?;
                let sinks = of_type("sink_installed");
                let in_unit: Vec<&SourceEvidence> = sinks
                    .iter()
                    .copied()
                    .filter(|e| unit_permit_evidence.contains(&e.evidence_id))
                    .collect();
                let reported_plumbing = of_type("in_unit_plumbing");
                let (mut a, key) = match reported_plumbing.last() {
                    Some(rp) if in_unit.is_empty() => {
                        let has = py::truthy(&rp.extracted_value);
                        let a = InfrastructureAssessment {
                            observed_value: Some(ScalarValue::from(if has {
                                "Broker reports existing in-unit plumbing"
                            } else {
                                "Broker reports no plumbing in the unit"
                            })),
                            required_value: Some(ScalarValue::from(n)),
                            evidence_ids: vec![rp.evidence_id],
                            limitations: rp.limitations.clone(),
                            proposed_work: vec![if has {
                                format!(
                                    "Extend supply/drain from existing in-unit plumbing to {n} treatment sinks"
                                )
                            } else {
                                format!("New supply and drain runs for {n} treatment sinks")
                            }],
                            verification_questions: strings(&[
                                "Where exactly are the supply and drain lines?",
                            ]),
                            review_status: ReviewStatus::NeedsReview,
                            ..base(
                                rid,
                                AssessmentStatus::MayRequireWork,
                                "Broker statement; line locations and capacity not inspected.",
                            )
                        };
                        let key = if has {
                            CostKey::SinkExtendFromExisting
                        } else {
                            CostKey::SinkNewRunSlab
                        };
                        (a, key)
                    }
                    _ if !in_unit.is_empty() => {
                        let values = in_unit.iter().map(|e| value_float(e)).collect::<Result<
                            Vec<f64>,
                            DomainError,
                        >>(
                        )?;
                        let total = py::sum_floats(values);
                        let a = InfrastructureAssessment {
                            observed_value: Some(ScalarValue::Text(format!(
                                "{} sink(s) documented in this unit",
                                int_text(in_unit[0], total)?
                            ))),
                            required_value: Some(ScalarValue::from(n)),
                            evidence_ids: ids(&in_unit),
                            limitations: in_unit[0].limitations.clone(),
                            proposed_work: vec![format!(
                                "Extend supply/drain from existing in-unit plumbing to {n} treatment sinks"
                            )],
                            verification_questions: strings(&[
                                "Where are the existing supply and drain lines and can they serve additional sinks?",
                            ]),
                            review_status: ReviewStatus::NeedsReview,
                            ..base(
                                rid,
                                AssessmentStatus::MayRequireWork,
                                "An existing sink suggests nearby plumbing but does not prove capacity for more fixtures.",
                            )
                        };
                        (a, CostKey::SinkExtendFromExisting)
                    }
                    _ => {
                        let ws = of_type("water_service_diameter");
                        let observed = match ws.first() {
                            Some(w) => format!(
                                "No in-unit plumbing documented; building has {}in water service (historical)",
                                py::value_str(&w.extracted_value)
                            ),
                            None => "No in-unit plumbing documented".to_owned(),
                        };
                        let a = InfrastructureAssessment {
                            required_value: Some(ScalarValue::from(n)),
                            evidence_ids: ids(&ws),
                            observed_value: Some(ScalarValue::Text(observed)),
                            limitations: match ws.first() {
                                Some(w) => w.limitations.clone(),
                                None => strings(&["No plumbing records found for this unit."]),
                            },
                            proposed_work: vec![format!(
                                "New supply and drain runs for {n} treatment sinks"
                            )],
                            verification_questions: strings(&[
                                "Where are the nearest drain and supply lines? Is slab cutting required?",
                            ]),
                            review_status: ReviewStatus::NeedsReview,
                            ..base(
                                rid,
                                AssessmentStatus::MayRequireWork,
                                "Drain locations unknown; new runs may require slab work.",
                            )
                        };
                        (a, CostKey::SinkNewRunSlab)
                    }
                };
                a.validate()?;
                // Python appends after construction (no re-validation), so this follows it.
                if profile
                    .infrastructure_requirements
                    .minimize_plumbing_changes
                    && key == CostKey::SinkNewRunSlab
                {
                    a.limitations.push(
                        "Owner prefers minimal plumbing changes; this property needs new runs."
                            .to_owned(),
                    );
                }
                let (assessment_id, evidence_ids) = (a.assessment_id, a.evidence_ids.clone());
                assessments.push(a);
                work.push(work_item(
                    pid,
                    key,
                    WorkCategory::Plumbing,
                    format!("Treatment sinks x{n}"),
                    Quantity::Int(n),
                    strings(&["One sink per treatment room"]),
                    evidence_ids,
                    Some(assessment_id),
                )?);
            } else if rid.ends_with("plumbing.water_service") {
                let mut support = of_type("water_service_diameter");
                support.extend(of_type("water_and_waste_work"));
                support.extend(of_type("sink_installed"));
                let a = match support.first() {
                    Some(first) => InfrastructureAssessment {
                        observed_value: Some(ScalarValue::from(
                            "Domestic water documented (historical permits)",
                        )),
                        required_value: Some(ScalarValue::Bool(true)),
                        evidence_ids: ids(&support),
                        limitations: first.limitations.clone(),
                        verification_questions: strings(&[
                            "Confirm service is active and adequate for added fixtures.",
                        ]),
                        review_status: ReviewStatus::NeedsReview,
                        ..base(
                            rid,
                            AssessmentStatus::Meets,
                            "Presence documented historically; adequacy for added fixtures not verified.",
                        )
                    },
                    None => InfrastructureAssessment {
                        required_value: Some(ScalarValue::Bool(true)),
                        verification_questions: strings(&[
                            "Is domestic water available to the space?",
                        ]),
                        ..base(rid, AssessmentStatus::Unknown, "No water service evidence found.")
                    },
                };
                assessments.push(checked(a)?);
            } else if rid.ends_with("hvac.ventilation") {
                let mut hv = of_type("hvac_tenant_improvement");
                hv.extend(of_type("vav_boxes"));
                let unit_hv: Vec<&SourceEvidence> = hv
                    .iter()
                    .copied()
                    .filter(|e| unit_permit_evidence.contains(&e.evidence_id))
                    .collect();
                let n = profile
                    .space_requirements
                    .required_rooms
                    .get("treatment")
                    .copied()
                    .unwrap_or(0);
                let zon = of_type("hvac_zonable");
                let (key, obs, status, ev_ids) = match zon.last() {
                    Some(z) if unit_hv.is_empty() => {
                        let zonable = py::truthy(&z.extracted_value);
                        (
                            if zonable {
                                CostKey::HvacRezoneExisting
                            } else {
                                CostKey::HvacNewDistribution
                            },
                            if zonable {
                                "Broker reports existing HVAC can be zoned per room"
                            } else {
                                "Broker reports HVAC cannot be zoned per room"
                            },
                            AssessmentStatus::MayRequireWork,
                            vec![z.evidence_id],
                        )
                    }
                    _ if !unit_hv.is_empty() => (
                        CostKey::HvacRezoneExisting,
                        "HVAC tenant improvement documented in this unit",
                        AssessmentStatus::MayRequireWork,
                        ids(&unit_hv),
                    ),
                    _ => (
                        CostKey::HvacNewDistribution,
                        "No unit-level HVAC work documented",
                        AssessmentStatus::Unknown,
                        ids(&hv),
                    ),
                };
                let a = checked(InfrastructureAssessment {
                    observed_value: Some(ScalarValue::from(obs)),
                    required_value: Some(ScalarValue::Bool(true)),
                    evidence_ids: ev_ids.clone(),
                    limitations: strings(&[
                        "Historical HVAC permits do not establish current capacity or zoning.",
                    ]),
                    proposed_work: vec![format!("Supply/return to {n} treatment rooms")],
                    verification_questions: strings(&[
                        "Can existing HVAC distribution be zoned to each treatment room?",
                    ]),
                    review_status: ReviewStatus::NeedsReview,
                    ..base(rid, status, "Zoning and capacity not verified.")
                })?;
                let assessment_id = a.assessment_id;
                assessments.push(a);
                work.push(work_item(
                    pid,
                    key,
                    WorkCategory::Hvac,
                    format!("HVAC distribution for {n} treatment rooms"),
                    Quantity::Int(if n != 0 { n } else { 1 }),
                    vec![obs.to_owned()],
                    ev_ids,
                    Some(assessment_id),
                )?);
            } else {
                assessments.push(checked(InfrastructureAssessment {
                    required_value: req.required_value.clone(),
                    verification_questions: vec![req.verification_method.clone()],
                    ..base(
                        rid,
                        AssessmentStatus::Unknown,
                        "No rule implemented for this requirement yet.",
                    )
                })?);
            }
        }

        // Not in Python (see `counts_for`): one limitation line on each assessment whose rule
        // reads an observation type that was left out. Appended after validation, like Python's
        // own late append above.
        if !left_out.is_empty() {
            for a in &mut assessments {
                if types_read(&a.requirement_id)
                    .iter()
                    .any(|t| left_out.contains(t))
                {
                    a.limitations.push(SIMULATED_NOT_COUNTED.to_owned());
                }
            }
        }

        // `area = listing.available_area_sqm if listing and listing.available_area_sqm else 0`:
        // a zero area adds no finishes line.
        if let Some(area) = listing
            .and_then(|l| l.available_area_sqm)
            .filter(|a| *a != 0.0)
        {
            work.push(work_item(
                pid,
                CostKey::Finishes,
                WorkCategory::Finishes,
                "Finishes (floor/wall/ceiling refresh)".to_owned(),
                Quantity::Float(py::round(area, 1)),
                strings(&["Listing area"]),
                Vec::new(),
                None,
            )?);
        }
        work.push(work_item(
            pid,
            CostKey::PermitsFees,
            WorkCategory::PermitsFees,
            "Permits and plan check fees".to_owned(),
            Quantity::Int(1),
            Vec::new(),
            Vec::new(),
            None,
        )?);
        work.push(work_item(
            pid,
            CostKey::DesignProfessional,
            WorkCategory::DesignProfessional,
            "Architect/engineer for TI drawings".to_owned(),
            Quantity::Int(1),
            Vec::new(),
            Vec::new(),
            None,
        )?);
        Ok((assessments, work))
    }
}
