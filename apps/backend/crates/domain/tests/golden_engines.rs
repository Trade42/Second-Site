//! Golden-vector parity for part 2 (feasibility, finance, ranking, evidence validator). Vectors
//! are produced by `tests-golden/domain/gen_domain2.py` running the Python implementation over the
//! repository fixtures; see that script for the cases and the normalization.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::labels::{expand, normalize_labels, uuids_in};
use common::{assert_json_eq, golden, read_json, to_value};
use serde_json::{json, Map, Value};
use ss_core::indexmap::IndexMap;
use ss_core::{BusinessExpansionProfile, Contract};
use ss_domain::contracts::{
    CostScenario, InfrastructureAssessment, InfrastructureRequirement, PropertyDigitalProfile,
    PropertySearchRequest, PyDecimal, RenovationWorkItem, SourceEvidence,
};
use ss_domain::evidence::validate_property;
use ss_domain::feasibility::FeasibilityEngine;
use ss_domain::finance::{apply_bp, work_item_totals, FinancialCalculationService};
use ss_domain::integrations::costar::FixtureCoStarProvider;
use ss_domain::integrations::la_permits::FixtureLaPermitProvider;
use ss_domain::permits::service::PermitIntelligenceService;
use ss_domain::property::service::{attach_permits, PropertyScoutService};
use ss_domain::py;
use ss_domain::ranking::{usd, RankingEngine};
use ss_domain::requirements::templates::requirements_for;
use ss_domain::DomainError;
use uuid::Uuid;

/// `demo_profile(**overrides)` of the generator: fixed project and profile ids.
fn profile(meta: &Value, overrides: &Value) -> BusinessExpansionProfile {
    let mut fx = read_json("fixtures/projects/facial_studio.json")["business_profile"].clone();
    let doc = fx.as_object_mut().unwrap();
    for (k, v) in overrides.as_object().unwrap() {
        doc.insert(k.clone(), v.clone());
    }
    doc.insert("project_id".into(), meta["project_id"].clone());
    doc.insert("profile_id".into(), meta["profile_id"].clone());
    BusinessExpansionProfile::from_json_value(fx).unwrap()
}

fn parse<T: Contract>(v: &Value) -> T {
    T::from_json_value(v.clone()).unwrap_or_else(|e| panic!("{}: {e:?}\n{v:#}", T::NAME))
}

fn parse_list<T: Contract>(v: &Value) -> Vec<T> {
    v.as_array().unwrap().iter().map(parse).collect()
}

fn values<T: serde::Serialize>(items: &[T]) -> Value {
    Value::Array(items.iter().map(to_value).collect())
}

fn base_fixed(meta: &Value) -> HashSet<String> {
    ["project_id", "profile_id"]
        .iter()
        .map(|k| meta[*k].as_str().unwrap().to_owned())
        .collect()
}

/// The vector's Python error, checked against the Rust error kind:
/// `ValidationError` -> `Validation` (same title, error types and locations);
/// `ValueError`/`TypeError` from `float()`/`int()` -> `BadRecord` ending with Python's message;
/// any other `ValueError` -> `Value` with Python's exact message.
fn assert_same_error(name: &str, py_err: &Value, err: &DomainError) {
    let kind = py_err["error"].as_str().unwrap();
    let message = py_err["message"].as_str().unwrap();
    match (kind, err) {
        ("ValidationError", DomainError::Validation(e)) => {
            assert_eq!(e.title, py_err["title"], "{name}");
            let ours: Vec<Value> = e
                .errors_json()
                .as_array()
                .unwrap()
                .iter()
                .map(|x| json!({"type": x["type"], "loc": x["loc"]}))
                .collect();
            assert_eq!(Value::Array(ours), py_err["errors"], "{name}");
        }
        ("ValueError" | "TypeError", DomainError::BadRecord(m)) => {
            assert!(m.ends_with(message), "{name}: {m:?} vs Python {message:?}");
        }
        ("ValueError", DomainError::Value(m)) => assert_eq!(m, message, "{name}"),
        _ => panic!("{name}: Python raised {kind} ({message}), Rust returned {err:?}"),
    }
}

fn horizon_key(h: Option<i64>) -> String {
    h.map_or_else(|| "profile".to_owned(), |h| h.to_string())
}

#[test]
fn pipeline_matches_python_on_repository_fixtures() {
    let g = golden("domain/part2_pipeline.json");
    let meta = &g["_meta"];
    let horizons = [Some(36), Some(120), None];
    let cases = g["cases"].as_array().unwrap();
    assert!(cases.len() >= 6);
    let scout = PropertyScoutService::new(Arc::new(FixtureCoStarProvider::new()));
    let permits = PermitIntelligenceService::new(Arc::new(FixtureLaPermitProvider::new().unwrap()));
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let prof = profile(meta, &case["profile_overrides"]);
        let ids: Vec<Uuid> = case["fixed_property_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| Uuid::parse_str(v.as_str().unwrap()).unwrap())
            .collect();
        let mut props = scout
            .search(prof.project_id, &PropertySearchRequest::default())
            .unwrap();
        assert_eq!(props.len(), ids.len(), "{name}");
        let reqs = requirements_for(&prof);
        let mut out_props = Map::new();
        let mut done = Vec::new();
        let mut assessments = IndexMap::new();
        let mut scenarios: IndexMap<String, IndexMap<Uuid, CostScenario>> = IndexMap::new();
        for (p, id) in props.iter_mut().zip(&ids) {
            p.property_id = *id;
            p.property_identity.property_id = *id;
            let res = permits
                .fetch_and_match(&p.property_identity, Some(prof.project_id))
                .unwrap();
            let p = attach_permits(p, res.permits, &res.datasets, "FIXTURE");
            let (a, w) = FeasibilityEngine
                .evaluate(&prof, &reqs, &p, &res.evidence)
                .unwrap();
            let mut sc_json = Map::new();
            for h in horizons {
                let s = FinancialCalculationService
                    .scenario(&prof, &p, &w, h, None)
                    .unwrap();
                sc_json.insert(horizon_key(h), to_value(&s));
                scenarios
                    .entry(horizon_key(h))
                    .or_default()
                    .insert(p.property_id, s);
            }
            out_props.insert(
                p.property_identity.street_address.clone(),
                json!({
                    "property_id": p.property_id,
                    "evidence": values(&res.evidence),
                    "assessments": values(&a),
                    "work_items": values(&w),
                    "scenarios": sc_json,
                    "problems": validate_property(&p, &res.evidence, &a),
                }),
            );
            assessments.insert(p.property_id, a);
            done.push(p);
        }
        let mut rankings = Map::new();
        for (h, scs) in &scenarios {
            let r = RankingEngine
                .rank(&prof, &done, scs, &assessments, 1)
                .unwrap();
            rankings.insert(h.clone(), to_value(&r));
        }
        let mut fixed = base_fixed(meta);
        fixed.extend(ids.iter().map(Uuid::to_string));
        let ours = normalize_labels(
            &json!({"properties": out_props, "rankings": rankings}),
            &fixed,
        );
        assert_json_eq(&ours, &case["output"], name);
    }
}

fn run_feasibility(meta: &Value, input: &Value) -> Result<Value, DomainError> {
    let prof = profile(meta, &input["profile_overrides"]);
    let reqs: Vec<InfrastructureRequirement> = parse_list(&input["requirements"]);
    let prop: PropertyDigitalProfile = parse(&input["property"]);
    let ev: Vec<SourceEvidence> = parse_list(&input["evidence"]);
    let (a, w) = FeasibilityEngine.evaluate(&prof, &reqs, &prop, &ev)?;
    let mut scenarios = Map::new();
    for h in [36, 120] {
        let s = FinancialCalculationService.scenario(&prof, &prop, &w, Some(h), None)?;
        scenarios.insert(h.to_string(), to_value(&s));
    }
    Ok(json!({
        "assessments": values(&a),
        "work_items": values(&w),
        "scenarios": scenarios,
        "problems": validate_property(&prop, &ev, &a),
    }))
}

fn run_finance(meta: &Value, input: &Value) -> Result<Value, DomainError> {
    let prof = profile(meta, &input["profile_overrides"]);
    let prop: PropertyDigitalProfile = parse(&input["property"]);
    let work: Vec<RenovationWorkItem> = parse_list(&input["work_items"]);
    let design = input["design_version_id"]
        .as_str()
        .map(|s| Uuid::parse_str(s).unwrap());
    let s = FinancialCalculationService.scenario(
        &prof,
        &prop,
        &work,
        input["horizon"].as_i64(),
        design,
    )?;
    Ok(json!({"scenario": to_value(&s)}))
}

fn pairs<T>(v: &Value, f: impl Fn(&Value) -> T) -> IndexMap<Uuid, T> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|kv| (Uuid::parse_str(kv[0].as_str().unwrap()).unwrap(), f(&kv[1])))
        .collect()
}

fn run_ranking(meta: &Value, input: &Value) -> Result<Value, DomainError> {
    let prof = profile(meta, &input["profile_overrides"]);
    let props: Vec<PropertyDigitalProfile> = parse_list(&input["properties"]);
    let scenarios = pairs(&input["scenarios"], parse::<CostScenario>);
    let assessments = pairs(&input["assessments"], |v| {
        parse_list::<InfrastructureAssessment>(v)
    });
    let version = input["ranking_version"].as_i64().unwrap();
    let r = RankingEngine.rank(&prof, &props, &scenarios, &assessments, version)?;
    Ok(json!({"ranking": to_value(&r)}))
}

#[test]
fn engine_cases_match_python() {
    let g = golden("domain/part2_cases.json");
    let meta = &g["_meta"];
    let bases = &g["bases"];
    let cases = g["cases"].as_array().unwrap();
    let mut seen_kinds = HashSet::new();
    let mut errors = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let kind = case["kind"].as_str().unwrap();
        seen_kinds.insert(kind.to_owned());
        let input = expand(&case["input"], bases);
        if kind == "validator" {
            let prop: PropertyDigitalProfile = parse(&input["property"]);
            let ev: Vec<SourceEvidence> = parse_list(&input["evidence"]);
            let a: Vec<InfrastructureAssessment> = parse_list(&input["assessments"]);
            assert_eq!(
                json!({"problems": validate_property(&prop, &ev, &a)}),
                case["output"],
                "{name}"
            );
            continue;
        }
        let result = match kind {
            "feasibility" => run_feasibility(meta, &input),
            "finance" => run_finance(meta, &input),
            "ranking" => run_ranking(meta, &input),
            other => panic!("unknown case kind {other}"),
        };
        let expected = &case["output"];
        // The one deliberate difference from Python in these cases: a room program of zero
        // counts fails Python's engine (partitions quantity 0) but completes here; the Rust
        // result is pinned in tests/empty_program.rs. Python's recorded error is still checked.
        if name == "error_zero_rooms" {
            assert_eq!(expected["error"]["title"], "RenovationWorkItem", "{name}");
            assert_eq!(
                expected["error"]["errors"][0]["loc"],
                json!(["quantity"]),
                "{name}"
            );
            assert!(result.is_ok(), "{name}: {result:?}");
            continue;
        }
        match (result, expected.get("error")) {
            (Err(e), Some(py_err)) => {
                errors += 1;
                assert_same_error(name, py_err, &e);
            }
            (Err(e), None) => panic!("{name}: Rust failed with {e:?}, Python succeeded"),
            (Ok(v), Some(py_err)) => panic!("{name}: Python raised {py_err}, Rust returned {v:#}"),
            (Ok(v), None) => {
                let mut fixed = base_fixed(meta);
                fixed.extend(uuids_in(&input));
                assert_json_eq(&normalize_labels(&v, &fixed), expected, name);
            }
        }
    }
    assert_eq!(seen_kinds.len(), 4);
    assert!(
        cases.len() >= 40 && errors >= 8,
        "{} cases, {errors} errors",
        cases.len()
    );
}

#[test]
fn cpython_numeric_semantics_used_by_the_engines() {
    let g = golden("domain/part2_units.json");
    for c in g["sum"].as_array().unwrap() {
        let xs: Vec<f64> = c["xs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap())
            .collect();
        let want = c["sum"].as_f64().unwrap();
        assert_eq!(
            py::sum_floats(xs.iter().copied()).to_bits(),
            want.to_bits(),
            "sum({xs:?})"
        );
    }
    for c in g["true_div"].as_array().unwrap() {
        let (a, b) = (c["a"].as_i64().unwrap(), c["b"].as_i64().unwrap());
        let want = c["q"].as_f64().unwrap();
        assert_eq!(
            py::int_true_div(a, b).unwrap().to_bits(),
            want.to_bits(),
            "{a} / {b}"
        );
    }
    for c in g["round"].as_array().unwrap() {
        let x = c["x"].as_f64().unwrap();
        assert_eq!(
            py::round(x, 6).to_bits(),
            c["round6"].as_f64().unwrap().to_bits(),
            "round({x}, 6)"
        );
        assert_eq!(
            py::round(x, 0) as i64,
            c["round0"].as_i64().unwrap(),
            "round({x})"
        );
    }
    for c in g["work_item_totals"].as_array().unwrap() {
        let costs = &c["costs"];
        let w = RenovationWorkItem::from_json_value(json!({
            "property_id": Uuid::nil(), "category": "OTHER", "description": "x",
            "quantity": c["quantity"], "unit": "u", "unit_cost_low": costs[0],
            "unit_cost_expected": costs[1], "unit_cost_high": costs[2],
        }))
        .unwrap();
        let (lo, ex, hi) = work_item_totals(&w).unwrap();
        assert_eq!(json!([lo, ex, hi]), c["totals"], "{c}");
    }
    for c in g["apply_bp"].as_array().unwrap() {
        let got = apply_bp(c["amount"].as_i64().unwrap(), c["bp"].as_i64().unwrap()).unwrap();
        assert_eq!(json!(got), c["result"], "{c}");
    }
    for c in g["usd"].as_array().unwrap() {
        assert_eq!(usd(c["cents"].as_i64().unwrap()), c["usd"], "{c}");
    }
    for c in g["area_quantity"].as_array().unwrap() {
        let r = py::round(c["area"].as_f64().unwrap(), 1);
        assert_eq!(r.to_bits(), c["round1"].as_f64().unwrap().to_bits(), "{c}");
        let d = PyDecimal(py::decimal_of_f64(r).unwrap());
        assert_eq!(d.to_string(), c["decimal"], "{c}");
    }
}

#[test]
fn golden_inputs_never_present_synthetic_data_as_real() {
    let g = golden("domain/part2_cases.json");
    let bases = &g["bases"];
    for case in g["cases"].as_array().unwrap() {
        let input = expand(&case["input"], bases);
        for e in input["evidence"].as_array().into_iter().flatten() {
            if e["provider"] == "broker_call" {
                assert_eq!(e["data_origin"], "FIXTURE_SYNTHETIC");
                assert!(e["limitations"][0]
                    .as_str()
                    .unwrap()
                    .starts_with("SYNTHETIC"));
            }
        }
        for p in input["properties"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(input.get("property"))
        {
            if let Some(l) = p["commercial_listing"].as_object() {
                if l["provider"] == "fixture" {
                    assert_eq!(l["data_origin"], "FIXTURE_SYNTHETIC");
                }
            }
        }
    }
}
