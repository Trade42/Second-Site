//! Port of the database-free tests in `apps/api/tests/test_identity_permits.py`, assertion for
//! assertion. Not ported (HTTP transport / live): `test_live_provider_query_shape_and_pagination`,
//! `test_live_provider_classifies_errors` (both drive `LiveLAPermitProvider` through an httpx mock
//! transport) and `test_live_la_open_data_permit` (network). Their pure parts are covered below by
//! `soql_and_prefix_shape` and `provider_error_classification`.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use ss_core::MatchStatus;
use ss_domain::contracts::{PermitCategory, PermitScope, PropertyIdentity, ScalarValue};
use ss_domain::integrations::base::{kind_for_http_status, ProviderError, ProviderErrorKind};
use ss_domain::integrations::la_permits::{
    address_prefix, soql_escape, FixtureLaPermitProvider, LaPermitProvider,
};
use ss_domain::permits::extract::extract_facts;
use ss_domain::permits::service::PermitIntelligenceService;
use ss_domain::property::address::parse_street;
use ss_domain::property::identity::{build_identity, match_record};

fn hollywood() -> PropertyIdentity {
    build_identity(
        "6922 W Hollywood Blvd",
        Some("302"),
        "Los Angeles",
        "CA",
        Some("90028"),
        Some("5548006011"),
        None,
        None,
        None,
    )
    .unwrap()
}

fn alhambra() -> PropertyIdentity {
    build_identity(
        "5380 E Alhambra Ave",
        None,
        "Los Angeles",
        "CA",
        Some("90032"),
        Some("5220031007"),
        None,
        None,
        None,
    )
    .unwrap()
}

fn service() -> PermitIntelligenceService {
    PermitIntelligenceService::new(Arc::new(FixtureLaPermitProvider::new().unwrap()))
}

fn facts_map(cat: PermitCategory, text: &str) -> Vec<(String, ScalarValue)> {
    extract_facts(cat, text)
        .unwrap()
        .into_iter()
        .map(|f| (f.fact_type, f.value))
        .collect()
}

fn get<'a>(facts: &'a [(String, ScalarValue)], key: &str) -> &'a ScalarValue {
    &facts
        .iter()
        .find(|(k, _)| k == key)
        .unwrap_or_else(|| panic!("{key}"))
        .1
}

#[test]
fn test_normalization_preserves_direction_unit_zip() {
    assert_eq!(
        hollywood().normalized_address,
        "6922 W HOLLYWOOD BLVD STE 302, LOS ANGELES, CA 90028"
    );
    assert_eq!(
        alhambra().normalized_address,
        "5380 E ALHAMBRA AVE, LOS ANGELES, CA 90032"
    );
    assert_eq!(
        parse_street("5380 East Alhambra Avenue", None, None, None, None)
            .unwrap()
            .street_line(),
        "5380 E ALHAMBRA AVE"
    );
    for raw in [
        "6922 W HOLLYWOOD BLVD 302",
        "6922 W HOLLYWOOD BLVD STE 302",
        "6922 W Hollywood Blvd Suite 302",
        "6922 w hollywood blvd #302",
    ] {
        assert_eq!(
            parse_street(raw, None, None, None, None)
                .unwrap()
                .unit
                .as_deref(),
            Some("302"),
            "{raw}"
        );
    }
}

#[test]
fn test_suite_specific_matching() {
    let h = hollywood();
    let m = match_record(
        &h,
        "6922 W HOLLYWOOD BLVD STE 302",
        Some("90028"),
        Some("5548006011"),
    )
    .unwrap();
    assert!(m.matched && m.scope == PermitScope::Unit && m.status == MatchStatus::Exact);
    assert!(
        !match_record(
            &h,
            "6922 W HOLLYWOOD BLVD 304",
            Some("90028"),
            Some("5548006011")
        )
        .unwrap()
        .matched
    );
    let b = match_record(
        &h,
        "6922 W HOLLYWOOD BLVD",
        Some("90028"),
        Some("5548006011"),
    )
    .unwrap();
    assert!(b.matched && b.scope == PermitScope::Building);
}

#[test]
fn test_never_match_on_street_name_alone_or_conflicts() {
    let a = alhambra();
    assert!(
        !match_record(&a, "5382 E ALHAMBRA AVE", Some("90032"), None)
            .unwrap()
            .matched
    ); // neighbor building
    assert_eq!(
        match_record(&a, "5380 W ALHAMBRA AVE", Some("90032"), None)
            .unwrap()
            .status,
        MatchStatus::Conflicting
    );
    assert_eq!(
        match_record(&a, "5380 E ALHAMBRA AVE", Some("90031"), None)
            .unwrap()
            .status,
        MatchStatus::Conflicting
    );
    assert_eq!(
        match_record(&a, "5380 E ALHAMBRA AVE", Some("90032"), Some("9999999999"))
            .unwrap()
            .status,
        MatchStatus::Conflicting
    );
    assert!(
        !match_record(&a, "5380 E ALHAMBRA ST", Some("90032"), None)
            .unwrap()
            .matched
    );
}

#[test]
fn test_ambiguous_floor_level_records_need_review() {
    let h = hollywood();
    let m = match_record(
        &h,
        "6922 W HOLLYWOOD BLVD 3rd Floor",
        Some("90028"),
        Some("5548006011"),
    )
    .unwrap();
    assert!(m.matched && m.status == MatchStatus::NeedsReview && m.scope == PermitScope::Floor);
    assert!(
        !match_record(
            &h,
            "6922 W HOLLYWOOD BLVD 5th Floor",
            Some("90028"),
            Some("5548006011")
        )
        .unwrap()
        .matched
    );
}

#[test]
fn test_missing_direction_is_not_used_as_evidence() {
    let ident = build_identity(
        "6922 Hollywood Blvd",
        Some("Suite 302"),
        "Los Angeles",
        "CA",
        Some("90028"),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let m = match_record(
        &ident,
        "6922 W HOLLYWOOD BLVD 302",
        Some("90028"),
        Some("5548006011"),
    )
    .unwrap();
    assert!(
        m.matched
            && m.status == MatchStatus::Probable
            && m.explanation.contains("direction missing")
    );
}

#[test]
fn test_extraction_from_real_permit_text() {
    let elec_facts = extract_facts(
        PermitCategory::Electrical,
        "SCOPE OF WORK INCLUDES A NEW 800A (120/240 3P 4W) SERVICE FEEDING VARIOUS SUBPANELS, AND ONE TRANSFORMER. TOTAL AFFECTED AREA IS 9840 SFQT.",
    )
    .unwrap();
    let got: Vec<(String, ScalarValue)> = elec_facts
        .iter()
        .map(|f| (f.fact_type.clone(), f.value.clone()))
        .collect();
    assert_eq!(get(&got, "electrical_service_amps").as_f64(), Some(800.0));
    assert_eq!(
        get(&got, "electrical_service_voltage").as_str(),
        Some("120/240")
    );
    assert_eq!(get(&got, "electrical_service_phases").as_f64(), Some(3.0));
    assert_eq!(get(&got, "affected_area_sqft").as_f64(), Some(9840.0));
    assert!(elec_facts.iter().all(|f| !f.limitations.is_empty()));
    let water = facts_map(
        PermitCategory::Plumbing,
        "POTABLE WATER AND WASTE/VENT. NEW 2 INCH WATER SERVICE WITH BACKFLOW PREVENTER.",
    );
    assert_eq!(get(&water, "water_service_diameter").as_f64(), Some(2.0));
    assert_eq!(get(&water, "backflow_preventer").as_bool(), Some(true));
    let sink = facts_map(
        PermitCategory::Plumbing,
        "Rough and finish for 1 break room sink",
    );
    assert_eq!(
        sink,
        vec![
            ("sink_installed".to_owned(), ScalarValue::Number(1.0)),
            (
                "sink_type".to_owned(),
                ScalarValue::Text("break_room".into())
            ),
        ]
    );
    assert_eq!(
        facts_map(PermitCategory::Plumbing, "cap 2 sinks"),
        vec![("sinks_capped".to_owned(), ScalarValue::Number(2.0))]
    );
}

#[test]
fn test_permit_number_lookup_and_historical_preservation() {
    let raws = FixtureLaPermitProvider::new()
        .unwrap()
        .get_permit("19041-10000-39945")
        .unwrap();
    assert!(raws.len() == 1 && raws[0].record["status_desc"] == "Permit Finaled");
    let res = service().fetch_and_match(&alhambra(), None).unwrap();
    let rec = res
        .permits
        .iter()
        .find(|p| p.permit_number == "19041-10000-39945")
        .unwrap();
    assert!(
        rec.permit_status == "Permit Finaled"
            && rec.finaled_date.map(|d| d.to_string()).as_deref() == Some("2022-07-20")
    );
    let ev: Vec<_> = res
        .evidence
        .iter()
        .filter(|e| {
            e.source_record_id.as_deref() == Some(rec.permit_number.as_str())
                && e.observation_type == "electrical_service_amps"
        })
        .collect();
    assert!(!ev.is_empty() && ev[0].evidence_classification.as_str() == "DOCUMENTED_HISTORICAL");
    assert!(ev[0]
        .limitations
        .iter()
        .any(|l| l.contains("spare capacity")));
    // Record exists; not a capacity verification.
    assert_eq!(ev[0].verification_status.as_str(), "SOURCE_CONFIRMED");
}

#[test]
fn test_suite_302_gets_its_sink_but_not_suite_304s() {
    let res = service().fetch_and_match(&hollywood(), None).unwrap();
    let nums: HashSet<&str> = res
        .permits
        .iter()
        .map(|p| p.permit_number.as_str())
        .collect();
    assert!(nums.contains("20042-90000-08431")); // suite 302 break-room sink
    assert!(!nums.contains("20042-90000-08432")); // suite 304's identical sink permit
    assert!(res.rejected.iter().any(|(n, _)| n == "20042-90000-08432"));
}

#[test]
fn test_missing_data_returns_no_matches() {
    let ident = build_identity(
        "1 Nowhere St",
        None,
        "Los Angeles",
        "CA",
        Some("90001"),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let res = service().fetch_and_match(&ident, None).unwrap();
    assert!(res.permits.is_empty() && res.evidence.is_empty());
}

// ---- pure parts of the two HTTP-transport tests -------------------------------------------------

#[test]
fn soql_and_prefix_shape() {
    assert_eq!(
        address_prefix("5380", Some("E"), "ALHAMBRA"),
        "5380 E ALHAMBRA"
    );
    assert_eq!(address_prefix("6922", None, "hollywood"), "6922 HOLLYWOOD");
    assert_eq!(address_prefix("1", Some(""), "x"), "1 X");
    assert_eq!(soql_escape("O'NEIL"), "O''NEIL");
}

#[test]
fn provider_error_classification() {
    assert_eq!(kind_for_http_status(503), ProviderErrorKind::Unavailable);
    assert_eq!(kind_for_http_status(401), ProviderErrorKind::Auth);
    assert_eq!(kind_for_http_status(403), ProviderErrorKind::Auth);
    assert_eq!(kind_for_http_status(404), ProviderErrorKind::NotFound);
    assert_eq!(kind_for_http_status(429), ProviderErrorKind::RateLimited);
    assert_eq!(kind_for_http_status(400), ProviderErrorKind::BadResponse);
    let e = ProviderError::new("la_open_data", kind_for_http_status(503), "HTTP 503");
    assert!(e.retryable());
    assert_eq!(e.to_string(), "[la_open_data] UNAVAILABLE: HTTP 503");
    assert!(!ProviderError::new("x", ProviderErrorKind::Auth, "m").retryable());
}

#[test]
fn fixture_permit_provider_status_and_labels() {
    let p = FixtureLaPermitProvider::new().unwrap();
    let st = p.status();
    assert_eq!(st.status.as_str(), "MOCK");
    assert!(st.note.contains("FIXTURE_REAL"));
    let raws = p
        .search_by_address_prefix("6922", Some("W"), "HOLLYWOOD", Some("90028"))
        .unwrap();
    assert!(!raws.is_empty());
    assert!(raws
        .iter()
        .all(|r| r.data_origin.as_str() == "FIXTURE_REAL"));
    assert!(p
        .search_by_address_prefix("6922", Some("W"), "HOLLYWOOD", Some("00000"))
        .unwrap()
        .is_empty());
    assert!(FixtureLaPermitProvider::from_path("/nonexistent/snapshot.json").is_err());
}
