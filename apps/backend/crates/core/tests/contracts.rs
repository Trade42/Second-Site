//! ss-core: schema conformance, golden parity with the Python models, and the validator
//! failures that `apps/api/tests/test_contracts.py` checks for these types.

mod common;

use common::{assert_conforms, assert_rejected_by_schema, golden, read_json};
use serde_json::{json, Value};
use ss_core::*;
use uuid::Uuid;

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).expect("serialize")
}

fn sample_profile() -> BusinessExpansionProfile {
    let fixture = read_json("fixtures/projects/facial_studio.json");
    let mut bp = fixture["business_profile"].clone();
    bp["project_id"] = json!(Uuid::new_v4());
    BusinessExpansionProfile::from_json_value(bp).expect("fixture business profile is valid")
}

// ---- JSON Schema conformance -------------------------------------------------------------------

#[test]
fn project_context_conforms_to_schema() {
    let mut p = ProjectContext::new(Uuid::new_v4(), "x", Market::new("Los Angeles", "CA"));
    assert_conforms("ProjectContext", &to_value(&p));
    p.business_profile_id = Some(Uuid::new_v4());
    p.selected_property_id = Some(Uuid::new_v4());
    p.project_status = ProjectStatus::Comparing;
    assert_conforms("ProjectContext", &to_value(&p));
    assert_conforms("Market", &to_value(&p.market));
    // The check has teeth: a two-character rule violation is caught by the schema too.
    let mut bad = to_value(&p);
    bad["market"]["state"] = json!("CAL");
    assert_rejected_by_schema("ProjectContext", &bad);
}

#[test]
fn business_profile_and_parts_conform_to_schema() {
    let mut p = sample_profile();
    p.target_opening_date = chrono::NaiveDate::from_ymd_opt(2027, 3, 1);
    p.operational_requirements.staff_count = Some(4);
    p.infrastructure_requirements.min_electrical_service_amps = Some(200);
    let v = to_value(&p);
    assert_conforms("BusinessExpansionProfile", &v);
    assert_conforms("SpaceRequirements", &to_value(&p.space_requirements));
    assert_conforms(
        "InfrastructureNeeds",
        &to_value(&p.infrastructure_requirements),
    );
    assert_conforms(
        "OperationalRequirements",
        &to_value(&p.operational_requirements),
    );
    assert_conforms("DesignPreferences", &to_value(&p.design_preferences));
    assert_conforms("RankingPriorities", &to_value(&p.ranking_priorities));
    assert_conforms("FieldProvenance", &to_value(&p.source_references[0]));
    assert_conforms(
        "BusinessExpansionProfile",
        &to_value(&BusinessExpansionProfile {
            space_requirements: SpaceRequirements::default(),
            ..p
        }),
    );
}

#[test]
fn primitives_conform_to_schema() {
    assert_conforms("Money", &to_value(&Money::usd(-1250)));
    assert_conforms(
        "Money",
        &to_value(&Money {
            amount_minor: 1,
            currency: CurrencyCode::new("EUR").unwrap(),
        }),
    );
    assert_conforms(
        "Measurement",
        &to_value(&Measurement {
            value: 200.0,
            unit: "A".into(),
        }),
    );
    assert_conforms("Point2D", &to_value(&Point2D { x: 1.5, y: -2.0 }));
    assert_conforms(
        "SourceReference",
        &to_value(&SourceReference {
            evidence_id: Some(Uuid::new_v4()),
            description: "permit 20042-90000-08431".into(),
            url: Some("https://data.lacity.org/resource/67is-svtd.json".into()),
        }),
    );
    assert_conforms(
        "SourceReference",
        &to_value(&SourceReference {
            evidence_id: None,
            description: "d".into(),
            url: None,
        }),
    );
}

#[test]
fn enum_values_are_identical_to_python() {
    let g = golden("core/enums.json");
    let check = |name: &str, ours: Vec<&str>| {
        let theirs: Vec<&str> = g[name]
            .as_array()
            .unwrap_or_else(|| panic!("{name} missing from golden"))
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(ours, theirs, "{name}");
    };
    check(
        "EvidenceClassification",
        EvidenceClassification::ALL
            .iter()
            .map(|e| e.as_str())
            .collect(),
    );
    check(
        "VerificationStatus",
        VerificationStatus::ALL.iter().map(|e| e.as_str()).collect(),
    );
    check(
        "MatchStatus",
        MatchStatus::ALL.iter().map(|e| e.as_str()).collect(),
    );
    check(
        "DataOrigin",
        DataOrigin::ALL.iter().map(|e| e.as_str()).collect(),
    );
    check(
        "IntegrationStatus",
        IntegrationStatus::ALL.iter().map(|e| e.as_str()).collect(),
    );
    check(
        "ProjectStatus",
        ProjectStatus::ALL.iter().map(|e| e.as_str()).collect(),
    );
    check(
        "ConfirmationStatus",
        ConfirmationStatus::ALL.iter().map(|e| e.as_str()).collect(),
    );
    // Serde uses the same strings, and FromStr round-trips them.
    for m in MatchStatus::ALL {
        assert_eq!(to_value(m), json!(m.as_str()));
        assert_eq!(m.as_str().parse::<MatchStatus>().unwrap(), *m);
    }
    assert!("exact".parse::<MatchStatus>().is_err());
    // StrEnum ordering compares the strings.
    assert!(MatchStatus::Ambiguous < MatchStatus::Exact);
}

// ---- Golden parity -----------------------------------------------------------------------------

#[test]
fn facial_studio_business_profile_matches_python() {
    let g = golden("core/business_profile_facial_studio.json");
    let parsed = BusinessExpansionProfile::from_json_value(g["input"].clone()).expect("valid");
    assert_eq!(to_value(&parsed), g["expected"]);
    // Field order is Python's too.
    let ours: Vec<String> = to_value(&parsed)
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let expected_order = [
        "profile_id",
        "project_id",
        "business_type",
        "business_description",
        "current_location_count",
        "target_market",
        "preferred_neighborhoods",
        "currency",
        "opening_capital",
        "monthly_rent_limit",
        "working_capital_reserve",
        "space_requirements",
        "infrastructure_requirements",
        "operational_requirements",
        "target_opening_date",
        "design_preferences",
        "ranking_priorities",
        "confirmation_status",
        "source_references",
    ];
    let text = serde_json::to_string(&parsed).unwrap();
    let mut last = 0;
    for k in expected_order {
        let pos = text
            .find(&format!("\"{k}\":"))
            .unwrap_or_else(|| panic!("{k}"));
        assert!(pos >= last, "{k} out of order");
        last = pos;
    }
    assert_eq!(ours.len(), expected_order.len());
    // Round trip through text is stable.
    let again = BusinessExpansionProfile::from_json_str(&text).unwrap();
    assert_eq!(again, parsed);
}

#[test]
fn business_profile_without_profile_id_gets_a_fresh_one() {
    let g = golden("core/business_profile_facial_studio.json");
    let mut input = g["input"].clone();
    input.as_object_mut().unwrap().remove("profile_id");
    let a = BusinessExpansionProfile::from_json_value(input.clone()).unwrap();
    let b = BusinessExpansionProfile::from_json_value(input).unwrap();
    assert_ne!(a.profile_id, b.profile_id);
    let mut va = to_value(&a);
    va["profile_id"] = g["expected"]["profile_id"].clone();
    assert_eq!(va, g["expected"]);
}

#[test]
fn project_context_timestamps_serialize_like_python() {
    for case in golden("core/project_context_roundtrip.json")
        .as_array()
        .unwrap()
    {
        let p = ProjectContext::from_json_value(case["input"].clone()).expect("valid");
        assert_eq!(to_value(&p), case["expected"]);
    }
    let t = UtcDateTime::parse("2026-01-01T00:00:00.1234567+00:00").unwrap();
    assert_eq!(t.to_json_string(), "2026-01-01T00:00:00.123456Z");
}

/// Pydantic rejects second 60 (chrono parses it as a leap second, which would serialize as
/// `...:60.000000Z`, a value Python cannot read back).
#[test]
fn leap_seconds_are_rejected_like_pydantic() {
    for ts in [
        "2026-11-14T23:59:60Z",
        "2016-12-31T23:59:60+00:00",
        "2026-11-14T12:00:60Z",
    ] {
        assert!(UtcDateTime::parse(ts).is_err(), "{ts}");
        let err = ProjectContext::from_json_value(json!({
            "owner_id": Uuid::new_v4(), "project_name": "x",
            "market": {"city": "LA", "state": "CA"}, "created_at": ts
        }))
        .expect_err(ts);
        assert_eq!(
            serde_json::to_value(&err.errors[0].loc).unwrap(),
            json!(["created_at"])
        );
    }
    let t = UtcDateTime::parse("2026-11-14T12:00:59.999999Z").unwrap();
    assert_eq!(t.to_json_string(), "2026-11-14T12:00:59.999999Z");
}

/// Every Python validation failure is a Rust failure at the same location. Where the error is a
/// constraint or validator error (not a serde type error), the type and message are identical too.
/// serde stops at the first deserialization error, so Rust's errors are a subset of Python's;
/// for cases where every Python error is a post-parse constraint, the sets are equal.
#[test]
fn validator_failures_match_python() {
    const SAME_TEXT: &[&str] = &[
        "missing",
        "extra_forbidden",
        "value_error",
        "greater_than_equal",
        "greater_than",
        "less_than_equal",
        "string_too_short",
        "string_too_long",
        "enum",
    ];
    const POST_PARSE: &[&str] = &[
        "greater_than_equal",
        "greater_than",
        "less_than_equal",
        "string_too_short",
        "string_too_long",
    ];
    let cases = golden("core/validation_failures.json");
    let cases = cases.as_array().unwrap();
    assert!(cases.len() >= 40);
    for case in cases {
        let name = format!("{} / {}", case["model"], case["name"]);
        let input = case["input"].clone();
        let result = match case["model"].as_str().unwrap() {
            "ProjectContext" => ProjectContext::from_json_value(input).map(|_| ()),
            "BusinessExpansionProfile" => {
                BusinessExpansionProfile::from_json_value(input).map(|_| ())
            }
            "Money" => Money::from_json_value(input).map(|_| ()),
            "Measurement" => Measurement::from_json_value(input).map(|_| ()),
            "Point2D" => Point2D::from_json_value(input).map(|_| ()),
            "SourceReference" => SourceReference::from_json_value(input).map(|_| ()),
            other => panic!("unexpected model {other}"),
        };
        let err = result.expect_err(&name);
        let python: Vec<(String, Value, String)> = case["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["type"].as_str().unwrap().to_owned(),
                    e["loc"].clone(),
                    e["msg"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert!(!err.errors.is_empty(), "{name}");
        for e in &err.errors {
            let loc = serde_json::to_value(&e.loc).unwrap();
            let same_loc: Vec<_> = python.iter().filter(|p| p.1 == loc).collect();
            assert!(
                !same_loc.is_empty(),
                "{name}: Rust error at {loc} not reported by Python {python:?}"
            );
            if SAME_TEXT.contains(&same_loc[0].0.as_str()) {
                assert_eq!(e.kind, same_loc[0].0, "{name}");
                assert_eq!(e.msg, same_loc[0].2, "{name}");
            }
        }
        if python.iter().all(|p| POST_PARSE.contains(&p.0.as_str())) {
            assert_eq!(err.errors.len(), python.len(), "{name}: {err}");
        }
        assert_eq!(err.title, case["model"].as_str().unwrap());
    }
}

// ---- Ported from apps/api/tests/test_contracts.py ----------------------------------------------

#[test]
fn test_project_requires_fields_and_keeps_nullable_ids_null() {
    let err = ProjectContext::from_json_value(json!({
        "project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}
    }))
    .expect_err("owner_id missing");
    assert_eq!(err.errors[0].kind, "missing");
    let p = ProjectContext::new(Uuid::new_v4(), "x", Market::new("Los Angeles", "CA"));
    assert!(p.selected_property_id.is_none() && p.selected_design_version_id.is_none());
    assert_eq!(p.schema_version, SCHEMA_VERSION);
    let text = serde_json::to_string(&p).unwrap();
    assert_eq!(ProjectContext::from_json_str(&text).unwrap(), p);
}

#[test]
fn test_extra_fields_are_rejected() {
    let err = ProjectContext::from_json_value(json!({
        "owner_id": Uuid::new_v4(), "project_name": "x",
        "market": {"city": "LA", "state": "CA"}, "surprise": 1
    }))
    .expect_err("extra field");
    assert_eq!(err.errors[0].kind, "extra_forbidden");
}

#[test]
fn test_naive_timestamps_rejected() {
    let err = ProjectContext::from_json_value(json!({
        "owner_id": Uuid::new_v4(), "project_name": "x",
        "market": {"city": "LA", "state": "CA"}, "created_at": "2026-11-14T12:00:00"
    }))
    .expect_err("naive timestamp");
    assert!(err.mentions("timezone-aware"), "{err}");
    // An explicit offset is converted to UTC.
    let p = ProjectContext::from_json_value(json!({
        "owner_id": Uuid::new_v4(), "project_name": "x",
        "market": {"city": "LA", "state": "CA"}, "created_at": "2026-11-14T12:00:00-08:00"
    }))
    .unwrap();
    assert_eq!(p.created_at.to_json_string(), "2026-11-14T20:00:00Z");
}

#[test]
fn money_is_integer_minor_units_with_iso_currency() {
    assert!(Money::from_json_value(json!({"amount_minor": 1.5})).is_err());
    let m = Money::from_json_value(json!({"amount_minor": -250})).unwrap();
    assert_eq!(m.currency, "USD");
    assert_eq!(m, Money::usd(-250));
    for bad in ["usd", "US", "USDD", "U5D", "", "Usd"] {
        assert!(CurrencyCode::new(bad).is_err(), "{bad}");
    }
    for good in ["USD", "EUR", "JPY"] {
        assert_eq!(CurrencyCode::new(good).unwrap().as_str(), good);
    }
}

#[test]
fn constructed_values_are_validated_on_demand() {
    let mut p = ProjectContext::new(Uuid::new_v4(), "", Market::new("LA", "C"));
    p.record_version = 0;
    let err = p.validate().expect_err("three violations");
    let locs: Vec<String> = err
        .errors
        .iter()
        .map(|e| serde_json::to_string(&e.loc).unwrap())
        .collect();
    assert_eq!(
        locs,
        vec![
            r#"["project_name"]"#,
            r#"["market","state"]"#,
            r#"["record_version"]"#
        ]
    );
    let details = err.errors_json();
    assert_eq!(details[0]["type"], "string_too_short");
    assert!(err
        .to_string()
        .starts_with("3 validation errors for ProjectContext"));
}

// ---- state.py ----------------------------------------------------------------------------------

ss_core::string_enum! {
    enum Light {
        Red = "RED",
        Green = "GREEN",
        Off = "OFF",
    }
}

const LIGHTS: &TransitionTable<Light> = &[
    (Light::Red, &[Light::Green, Light::Off]),
    (Light::Green, &[Light::Red]),
];

#[test]
fn check_transition_allows_only_listed_targets() {
    assert_eq!(
        check_transition("light", LIGHTS, Light::Red, Light::Green),
        Ok(Light::Green)
    );
    let err = check_transition("light", LIGHTS, Light::Green, Light::Off).unwrap_err();
    assert_eq!(
        err.to_string(),
        "light: transition GREEN -> OFF is not allowed"
    );
    assert_eq!(
        (err.current.as_str(), err.target.as_str()),
        ("GREEN", "OFF")
    );
    // A state without an entry is terminal.
    assert!(check_transition("light", LIGHTS, Light::Off, Light::Red).is_err());
}
