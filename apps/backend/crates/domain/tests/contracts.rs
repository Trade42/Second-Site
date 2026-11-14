//! Domain contracts: schema conformance for every public contract type, enum strings, the
//! validators `apps/api/tests/test_contracts.py` checks for these types, and the CoStar kit tests
//! from `apps/api/tests/test_sponsor_providers.py` (kit part only).

mod common;

use std::sync::Arc;

use common::*;
use serde_json::{json, Value};
use ss_core::{Contract, DataOrigin, EvidenceClassification, IntegrationStatus};
use ss_domain::contracts::*;
use ss_domain::integrations::costar::FixtureCoStarProvider;
use ss_domain::integrations::costar::{CoStarProvider, KitCoStarProvider};
use ss_domain::integrations::la_permits::FixtureLaPermitProvider;
use ss_domain::permits::service::PermitIntelligenceService;
use ss_domain::property::address::norm_unit;
use ss_domain::property::service::{attach_permits, profile_from_listing, PropertyScoutService};
use uuid::Uuid;

fn u() -> Uuid {
    Uuid::new_v4()
}

fn schema_enum(schema: &str, name: &str) -> Vec<String> {
    let s = read_json(&format!("packages/contracts/schemas/{schema}.json"));
    s["$defs"][name]["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("{schema} has no $defs.{name}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn strs<T: AsRef<str>>(all: &[T]) -> Vec<String> {
    all.iter().map(|e| e.as_ref().to_owned()).collect()
}

#[test]
fn enum_strings_match_the_wire_contract() {
    assert_eq!(
        strs(PermitCategory::ALL),
        schema_enum("PermitRecord", "PermitCategory")
    );
    assert_eq!(
        strs(PermitScope::ALL),
        schema_enum("PermitRecord", "PermitScope")
    );
    assert_eq!(
        strs(ListingType::ALL),
        schema_enum("PropertyDigitalProfile", "ListingType")
    );
    assert_eq!(
        strs(AssessmentStatus::ALL),
        schema_enum("PropertyDigitalProfile", "AssessmentStatus")
    );
    assert_eq!(
        strs(ReviewStatus::ALL),
        schema_enum("PropertyDigitalProfile", "ReviewStatus")
    );
    assert_eq!(
        strs(WorkCategory::ALL),
        schema_enum("RenovationWorkItem", "WorkCategory")
    );
    assert_eq!(strs(CostKind::ALL), schema_enum("CostScenario", "CostKind"));
    assert_eq!(
        strs(RequirementCategory::ALL),
        schema_enum("InfrastructureRequirement", "RequirementCategory")
    );
    assert_eq!(
        strs(Severity::ALL),
        schema_enum("InfrastructureRequirement", "Severity")
    );
    assert_eq!(
        strs(DataOrigin::ALL),
        schema_enum("PermitRecord", "DataOrigin")
    );
    assert_eq!(
        strs(EvidenceClassification::ALL),
        schema_enum("PermitRecord", "EvidenceClassification")
    );
}

/// A full profile from the fixture pipeline: listing, assets, permits with facts and evidence.
fn pipeline() -> (PropertyDigitalProfile, Vec<SourceEvidence>) {
    let scout = PropertyScoutService::new(Arc::new(FixtureCoStarProvider::new()));
    let permits = PermitIntelligenceService::new(Arc::new(FixtureLaPermitProvider::new().unwrap()));
    let profiles = scout
        .search(u(), &PropertySearchRequest::default())
        .unwrap();
    let prof = profiles
        .into_iter()
        .find(|p| p.property_identity.unit_or_suite.is_some())
        .unwrap();
    let res = permits
        .fetch_and_match(&prof.property_identity, Some(prof.project_id))
        .unwrap();
    (
        attach_permits(&prof, res.permits, &res.datasets, "FIXTURE"),
        res.evidence,
    )
}

#[test]
fn every_public_contract_conforms_to_its_schema() {
    let (mut prof, evidence) = pipeline();
    assert!(!prof.permit_records.is_empty() && !evidence.is_empty());
    prof.interior_assets.push(InteriorAsset {
        provider: "matterport".into(),
        asset_id: "m1".into(),
        asset_type: "matterport_model".into(),
        data_origin: DataOrigin::FixtureSynthetic,
        url: Some("https://my.matterport.com/show/?m=m1".into()),
        label: Some("Suite 302".into()),
        area_sqm: Some(104.8),
        matches_listing_unit: Some(true),
        has_measured_geometry: false,
    });
    prof.building_observations.push(InfrastructureObservation {
        observation_id: u(),
        property_id: Some(prof.property_id),
        observation_type: "sink_present".into(),
        value: Some(true.into()),
        unit: None,
        classification: EvidenceClassification::DocumentedHistorical,
        evidence_ids: vec![evidence[0].evidence_id],
        limitations: vec!["historical".into()],
        description: "d".into(),
    });
    prof.infrastructure_assessments
        .push(InfrastructureAssessment {
            assessment_id: u(),
            project_id: prof.project_id,
            property_id: prof.property_id,
            requirement_id: "facial_studio.plumbing.treatment_sinks".into(),
            status: AssessmentStatus::Unknown,
            observed_value: None,
            required_value: Some(3.0.into()),
            evidence_ids: vec![],
            limitations: vec![],
            proposed_work: vec![],
            verification_questions: vec!["Where are the supply and drain lines?".into()],
            uncertainty: "Sink capacity not established".into(),
            review_status: ReviewStatus::NeedsReview,
        });
    prof.last_analyzed_at = Some(ss_core::utcnow());
    prof.validate().unwrap();
    let v = to_value(&prof);
    assert_conforms("PropertyDigitalProfile", &v);
    assert_conforms("PropertyIdentity", &to_value(&prof.property_identity));
    assert_conforms(
        "CommercialListing",
        &to_value(prof.commercial_listing.as_ref().unwrap()),
    );
    for a in &prof.interior_assets {
        assert_conforms("InteriorAsset", &to_value(a));
    }
    for a in &prof.exterior_assets {
        assert_conforms("ExteriorAsset", &to_value(a));
    }
    for m in &prof.source_manifest {
        assert_conforms("SourceManifestEntry", &to_value(m));
    }
    for p in &prof.permit_records {
        assert_conforms("PermitRecord", &to_value(p));
        for f in &p.extracted_infrastructure_facts {
            assert_conforms("ExtractedInfrastructureFact", &to_value(f));
        }
    }
    for e in &evidence {
        assert_conforms("SourceEvidence", &to_value(e));
    }
    assert_conforms(
        "InfrastructureObservation",
        &to_value(&prof.building_observations[0]),
    );
    assert_conforms(
        "InfrastructureAssessment",
        &to_value(&prof.infrastructure_assessments[0]),
    );
    assert_conforms(
        "PropertySearchRequest",
        &to_value(&PropertySearchRequest::default()),
    );
    assert_conforms(
        "ProviderStatusEntry",
        &to_value(
            &FixtureLaPermitProvider::new()
                .unwrap_or_else(|e| panic!("{e}"))
                .status_entry_for_test(),
        ),
    );

    let item = RenovationWorkItem::from_json_value(json!({
        "property_id": u(), "category": "PLUMBING", "description": "x", "quantity": 2.5,
        "unit": "ea", "unit_cost_low": 1, "unit_cost_expected": 2, "unit_cost_high": 3,
    }))
    .unwrap();
    assert_conforms("RenovationWorkItem", &to_value(&item));
    let line = CostLineItem {
        line_item_id: "rent".into(),
        category: "RENT".into(),
        description: "x".into(),
        kind: CostKind::Monthly,
        low: -5,
        expected: 1,
        high: 2,
        source_work_item_id: Some(item.work_item_id),
        evidence_ids: vec![u()],
    };
    assert_conforms("CostLineItem", &to_value(&line));
    let scenario = scenario(vec![line.clone()]);
    scenario.validate().unwrap();
    assert_conforms("CostScenario", &to_value(&scenario));
    let hc = HardConstraintResult {
        property_id: u(),
        constraint: "budget".into(),
        passed: None,
        detail: "unknown rent".into(),
    };
    assert_conforms("HardConstraintResult", &to_value(&hc));
    let ranked = RankedProperty {
        rank: 1,
        property_id: u(),
        total_score: 0.7,
        disqualified: false,
        long_term_cost: Some(100),
        unknown_requirement_count: 2,
    };
    assert_conforms("RankedProperty", &to_value(&ranked));
    let ranking = PropertyRankingResult::from_json_value(json!({
        "project_id": u(), "ranking_version": 1, "criteria_weights": {"total_cost": 0.5},
        "hard_constraint_results": [to_value(&hc)], "ranked_properties": [to_value(&ranked)],
        "normalized_scores": {"p": {"total_cost": 1.0}}, "uncertainty_penalties": {"p": 0.1},
        "cost_scenario_ids": [scenario.scenario_id], "explanations": {"p": "cheapest"},
    }))
    .unwrap();
    assert_conforms("PropertyRankingResult", &to_value(&ranking));
    for r in ss_domain::requirements::templates::requirements_for(
        &ss_core::BusinessExpansionProfile::from_json_value({
            let mut bp =
                read_json("fixtures/projects/facial_studio.json")["business_profile"].clone();
            bp["project_id"] = json!(u());
            bp
        })
        .unwrap(),
    ) {
        assert_conforms("InfrastructureRequirement", &to_value(&r));
    }
    // The checks have teeth.
    let mut bad = to_value(&prof.permit_records[0]);
    bad["scope"] = json!("SUITE");
    assert_rejected_by_schema("PermitRecord", &bad);
}

trait StatusForTest {
    fn status_entry_for_test(&self) -> ProviderStatusEntry;
}

impl StatusForTest for FixtureLaPermitProvider {
    fn status_entry_for_test(&self) -> ProviderStatusEntry {
        use ss_domain::integrations::la_permits::LaPermitProvider;
        self.status()
    }
}

fn scenario(lines: Vec<CostLineItem>) -> CostScenario {
    CostScenario {
        scenario_id: u(),
        project_id: u(),
        property_id: u(),
        design_version_id: None,
        currency: Default::default(),
        opening_cost_low: 1,
        opening_cost_expected: 1,
        opening_cost_high: 1,
        upfront_cash_expected: 1,
        monthly_operating_cost: 0,
        comparison_horizon_months: 36,
        long_term_cost: 1,
        contingency: 0,
        line_items: lines,
        assumptions: vec![],
        excluded_costs: vec![],
        source_evidence_ids: vec![],
        calculated_at: ss_core::utcnow(),
    }
}

// ---- ported from apps/api/tests/test_contracts.py ----------------------------------------------

#[test]
fn test_money_is_integer_minor_units_and_ordered() {
    let base = json!({
        "property_id": u(), "category": "PLUMBING", "description": "x", "quantity": 1,
        "unit": "ea", "unit_cost_low": 500, "unit_cost_expected": 400, "unit_cost_high": 900,
    });
    let err = RenovationWorkItem::from_json_value(base.clone()).unwrap_err();
    assert!(err.mentions("low <= expected <= high"), "{err}");
    let mut fractional = base.clone();
    fractional["unit_cost_low"] = json!(1.5);
    fractional["unit_cost_expected"] = json!(2);
    fractional["unit_cost_high"] = json!(3);
    assert!(RenovationWorkItem::from_json_value(fractional).is_err());
    let mut zero_qty = base;
    zero_qty["unit_cost_low"] = json!(1);
    zero_qty["unit_cost_expected"] = json!(1);
    zero_qty["unit_cost_high"] = json!(1);
    zero_qty["quantity"] = json!(0);
    let err = RenovationWorkItem::from_json_value(zero_qty).unwrap_err();
    assert_eq!(err.errors[0].kind, "greater_than");
}

#[test]
fn test_cost_scenario_rejects_duplicate_lines() {
    let li = CostLineItem {
        line_item_id: "a".into(),
        category: "X".into(),
        description: "x".into(),
        kind: CostKind::OneTime,
        low: 1,
        expected: 1,
        high: 1,
        source_work_item_id: None,
        evidence_ids: vec![],
    };
    let err = scenario(vec![li.clone(), li]).validate().unwrap_err();
    assert!(err.mentions("double counting"), "{err}");
    let mut s = scenario(vec![]);
    s.opening_cost_low = 5;
    assert!(s.validate().unwrap_err().mentions("opening costs"));
}

#[test]
fn test_historical_evidence_must_state_limitations() {
    let err = SourceEvidence::from_json_value(json!({
        "provider": "la_open_data", "data_origin": "FIXTURE_REAL", "observation_type": "x",
        "evidence_classification": "DOCUMENTED_HISTORICAL",
    }))
    .unwrap_err();
    assert!(err.mentions("limitations"), "{err}");
    let ok = SourceEvidence::from_json_value(json!({
        "provider": "la_open_data", "data_origin": "FIXTURE_REAL", "observation_type": "x",
        "evidence_classification": "DOCUMENTED_HISTORICAL", "limitations": ["historical"],
        "extracted_value": {"nested": [1, 2]},
    }))
    .unwrap();
    assert_eq!(ok.verification_status.as_str(), "UNVERIFIED");
    assert_eq!(ok.extracted_value, json!({"nested": [1, 2]}));
    // Field errors are reported first; the model rule does not run on an invalid model.
    let err = SourceEvidence::from_json_value(json!({
        "provider": "x", "data_origin": "LIVE", "observation_type": "x",
        "evidence_classification": "DOCUMENTED_HISTORICAL", "match_confidence": 1.5,
    }))
    .unwrap_err();
    assert_eq!(err.errors.len(), 1);
    assert_eq!(err.errors[0].kind, "less_than_equal");
}

#[test]
fn test_unknown_is_never_meets_and_needs_questions() {
    let base = |status: &str| {
        json!({"project_id": u(), "property_id": u(), "requirement_id": "r", "status": status,
               "uncertainty": "none"})
    };
    let err = InfrastructureAssessment::from_json_value(base("MEETS")).unwrap_err(); // MEETS without evidence
    assert!(err.mentions("MEETS requires"));
    let err = InfrastructureAssessment::from_json_value(base("UNKNOWN")).unwrap_err(); // no questions
    assert!(err.mentions("verification_questions"));
    let mut meets = base("MEETS");
    meets["evidence_ids"] = json!([u()]);
    assert!(InfrastructureAssessment::from_json_value(meets).is_ok());
    assert!(InfrastructureAssessment::from_json_value(base("MAY_REQUIRE_WORK")).is_ok());
}

#[test]
fn domain_contract_validators() {
    assert!(PropertySearchRequest::from_json_value(json!({"limit": 51})).is_err());
    assert!(PropertySearchRequest::from_json_value(json!({"limit": 0})).is_err());
    assert!(PropertySearchRequest::from_json_value(json!({"min_area_sqm": 0})).is_err());
    assert!(PropertySearchRequest::from_json_value(json!({"surprise": 1})).is_err());
    let r = PropertySearchRequest::from_json_value(json!({})).unwrap();
    assert_eq!(
        (r.city.as_str(), r.state.as_str(), r.limit),
        ("Los Angeles", "CA", 10)
    );
    assert!(CommercialListing::from_json_value(json!({
        "provider": "fixture", "data_origin": "FIXTURE_SYNTHETIC", "asking_rent_monthly": -1
    }))
    .is_err());
    assert!(CommercialListing::from_json_value(json!({
        "provider": "fixture", "data_origin": "FIXTURE_SYNTHETIC", "lease_term_months": 0
    }))
    .is_err());
    assert!(CommercialListing::from_json_value(json!({
        "provider": "fixture", "data_origin": "FIXTURE_SYNTHETIC", "currency": "usd"
    }))
    .is_err());
    assert!(InteriorAsset::from_json_value(json!({
        "provider": "p", "asset_id": "a", "asset_type": "t", "data_origin": "LIVE", "area_sqm": 0
    }))
    .is_err());
    assert!(PropertyIdentity::from_json_value(json!({
        "normalized_address": "x", "street_address": "x", "city": "LA", "state": "CA", "latitude": 91
    }))
    .is_err());
    let hc = HardConstraintResult::from_json_value(json!({
        "property_id": u(), "constraint": "c", "detail": "d"
    }));
    assert!(hc.is_err(), "passed is required (may be null)");
    assert!(HardConstraintResult::from_json_value(json!({
        "property_id": u(), "constraint": "c", "detail": "d", "passed": null
    }))
    .is_ok());
}

#[test]
fn values_serialize_like_pydantic() {
    // `float | str | bool`: integers become floats, booleans stay booleans.
    let f = ExtractedInfrastructureFact::from_json_value(json!({
        "fact_type": "a", "value": 200, "source_text": "x"
    }))
    .unwrap();
    assert_eq!(to_value(&f)["value"], json!(200.0));
    assert_eq!(
        to_value(&f)["classification"],
        json!("DOCUMENTED_HISTORICAL")
    );
    let b = ExtractedInfrastructureFact::from_json_value(json!({
        "fact_type": "a", "value": true, "source_text": "x"
    }))
    .unwrap();
    assert_eq!(to_value(&b)["value"], json!(true));
    // Decimal quantities are strings in str(Decimal) form.
    for (input, out) in [
        (json!(1), "1"),
        (json!("1.50"), "1.50"),
        (json!(2.5), "2.5"),
        (json!(3), "3"),
        (json!("0.0000001"), "1E-7"),
    ] {
        let d: PyDecimal = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(to_value(&d), json!(out), "{input}");
    }
}

// ---- CoStar data kit (from apps/api/tests/test_sponsor_providers.py) ----------------------------

fn kit() -> KitCoStarProvider {
    KitCoStarProvider::new(golden_dir("costar_kit_synthetic"))
}

fn req(v: Value) -> PropertySearchRequest {
    PropertySearchRequest::from_json_value(v).unwrap()
}

#[test]
fn test_kit_search_maps_real_fields() {
    let p = kit();
    let res = p
        .search(&req(
            json!({"city": "Los Angeles", "state": "CA", "limit": 10}),
        ))
        .unwrap();
    let ids: Vec<&str> = res.iter().map(|r| r.provider_listing_id.as_str()).collect();
    // No rate / off market / industrial.
    assert!(!ids.contains(&"1:13") && !ids.contains(&"1:14") && !ids.contains(&"1:15"));
    assert_eq!(p.skipped_no_rate(), 1);
    assert!(res
        .iter()
        .all(|r| r.provider == "costar" && r.data_origin == DataOrigin::FixtureReal));
    let by = |id: &str| res.iter().find(|r| r.provider_listing_id == id).unwrap();
    assert_eq!(ids.iter().filter(|i| i.starts_with("1:")).count(), 2); // at most two spaces per building
    let cheapest = &res[0];
    assert!(
        cheapest.provider_listing_id == "2:21" && cheapest.listing["asking_rent_monthly"] == 300000
    );
    let s16 = &by("1:16").listing;
    assert!(s16["asking_rent_monthly"] == 345000 && s16["nnn_charges_monthly"].is_null());
    assert!(
        s16["lease_service_type"] == "TripleNet"
            && s16["existing_buildout"] == "second_generation_salon"
    );
    assert!(s16["lease_term_months"] == 60 && s16["available_from"] == "2026-12-01");
    assert_eq!(
        s16["photo_urls"],
        json!(["https://cdn.example/i/abc/113/image.jpg"])
    );
    let a = &by("1:16").address;
    assert!(a["street_address"] == "100 W Main St" && a["postal_code"] == "90012");
    let mp: Vec<_> = by("1:16")
        .interior_assets
        .iter()
        .filter(|a| a["provider"] == "matterport")
        .collect();
    assert_eq!(
        mp.iter()
            .map(|a| a["asset_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["MODEL1", "MODEL2"]
    );
    assert!(!mp.iter().any(|a| a["has_measured_geometry"] == true));
    let houston = p
        .search(&req(json!({"city": "Houston", "state": "TX"})))
        .unwrap();
    assert_eq!(
        houston
            .iter()
            .map(|r| r.provider_listing_id.as_str())
            .collect::<Vec<_>>(),
        vec!["3:31"]
    );
}

#[test]
fn test_kit_filters_and_profile() {
    let p = kit();
    let res = p
        .search(&req(json!({"city": "Los Angeles", "state": "CA", "max_monthly_rent": 400000, "min_area_sqm": 90})))
        .unwrap();
    let mut ids: Vec<&str> = res.iter().map(|r| r.provider_listing_id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["1:12", "1:16", "2:21"]); // 1:11 is over budget
    let raw = res
        .iter()
        .find(|r| r.provider_listing_id == "1:12")
        .unwrap();
    let prof = profile_from_listing(u(), raw).unwrap();
    // Full service: expenses included in rent.
    assert_eq!(
        prof.commercial_listing
            .as_ref()
            .unwrap()
            .nnn_charges_monthly,
        Some(0)
    );
    assert_eq!(prof.source_manifest[0].status, "SNAPSHOT");
    assert_eq!(
        prof.property_identity.provider_property_ids["costar_property"],
        "100"
    );
    assert_eq!(
        prof.interior_assets[0].url.as_deref(),
        Some("https://my.matterport.com/show/?m=MODEL1")
    );
}

#[test]
fn test_kit_missing_is_explicit() {
    let p = KitCoStarProvider::new(golden_dir("costar_kit_does_not_exist"));
    assert_eq!(p.status().status, IntegrationStatus::Blocked);
    let err = p.search(&PropertySearchRequest::default()).unwrap_err();
    assert!(err.to_string().contains("NOT_CONFIGURED"), "{err}");
}

#[test]
fn test_unit_letter_is_kept() {
    assert_eq!(norm_unit(Some("110 B")).as_deref(), Some("110B"));
    assert_eq!(norm_unit(Some("Ste 110 B")).as_deref(), Some("110B"));
    assert_eq!(norm_unit(Some("302")).as_deref(), Some("302"));
}

#[test]
fn test_missing_net_lease_opex_is_estimated_and_flagged() {
    let p = KitCoStarProvider::new(golden_dir("costar_kit_synthetic_opex"));
    let res = p.search(&req(json!({"limit": 50}))).unwrap();
    let by = |id: &str| {
        &res.iter()
            .find(|r| r.provider_listing_id == id)
            .unwrap()
            .listing
    };
    let s16 = by("1:16"); // TripleNet, no CAM quoted, 1,150 SF
    assert!(
        s16["nnn_is_estimate"] == true
            && s16["disclaimer"].as_str().unwrap().contains("median CAM")
    );
    assert_eq!(s16["nnn_charges_monthly"], 105417); // median $11/SF/yr x 1,150 SF / 12
    assert_eq!(by("900:900")["nnn_is_estimate"], false); // quoted CAM is used as-is
    assert_eq!(by("1:12")["nnn_charges_monthly"], 0); // full service stays included
}

#[test]
fn fixture_listings_are_never_presented_as_costar() {
    let scout = PropertyScoutService::new(Arc::new(FixtureCoStarProvider::new()));
    let profiles = scout
        .search(u(), &PropertySearchRequest::default())
        .unwrap();
    assert_eq!(profiles.len(), 2);
    for p in &profiles {
        let l = p.commercial_listing.as_ref().unwrap();
        assert_eq!(l.provider, "fixture");
        assert_eq!(l.data_origin, DataOrigin::FixtureSynthetic);
        assert!(l
            .disclaimer
            .as_deref()
            .unwrap()
            .contains("Not a CoStar record"));
        assert_eq!(p.source_manifest[0].status, "MOCK");
        assert!(p
            .property_identity
            .provider_property_ids
            .contains_key("fixture"));
        assert!(!p
            .property_identity
            .provider_property_ids
            .contains_key("costar"));
    }
    assert_eq!(
        FixtureCoStarProvider::new().status().status,
        IntegrationStatus::Mock
    );
}
