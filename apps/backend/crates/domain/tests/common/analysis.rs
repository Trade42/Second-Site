//! Rust rebuild of `apps/api/tests/helpers.py` (`demo_profile`, `analyze_in_memory`) on top of
//! part 1's fixture providers: search -> permits -> feasibility -> cost scenario -> ranking.

use std::sync::Arc;

use serde_json::{json, Value};
use ss_core::indexmap::IndexMap;
use ss_core::{BusinessExpansionProfile, Contract};
use ss_domain::contracts::{
    CostScenario, InfrastructureAssessment, PropertyDigitalProfile, PropertyRankingResult,
    PropertySearchRequest, RenovationWorkItem, SourceEvidence,
};
use ss_domain::feasibility::FeasibilityEngine;
use ss_domain::finance::FinancialCalculationService;
use ss_domain::integrations::costar::FixtureCoStarProvider;
use ss_domain::integrations::la_permits::FixtureLaPermitProvider;
use ss_domain::permits::service::PermitIntelligenceService;
use ss_domain::property::service::{attach_permits, PropertyScoutService};
use ss_domain::ranking::RankingEngine;
use ss_domain::requirements::templates::requirements_for;
use uuid::Uuid;

use super::read_json;

/// `demo_profile(**overrides)`: the facial-studio fixture profile with top-level overrides and a
/// fresh `project_id` (or the one given in `overrides`).
pub fn demo_profile(overrides: Value) -> BusinessExpansionProfile {
    let mut fx = read_json("fixtures/projects/facial_studio.json")["business_profile"].clone();
    fx["project_id"] = json!(Uuid::new_v4());
    if let (Some(doc), Some(changes)) = (fx.as_object_mut(), overrides.as_object()) {
        for (k, v) in changes {
            doc.insert(k.clone(), v.clone());
        }
    }
    BusinessExpansionProfile::from_json_value(fx).expect("valid demo profile")
}

pub struct Analysis {
    /// Keyed by `property_identity.street_address`, in search order.
    pub props: IndexMap<String, PropertyDigitalProfile>,
    pub scenarios: IndexMap<Uuid, CostScenario>,
    pub assessments: IndexMap<Uuid, Vec<InfrastructureAssessment>>,
    pub work_items: IndexMap<Uuid, Vec<RenovationWorkItem>>,
    pub evidence: IndexMap<Uuid, Vec<SourceEvidence>>,
    pub ranking: PropertyRankingResult,
}

/// `analyze_in_memory(profile, horizon)`.
pub fn analyze_in_memory(profile: &BusinessExpansionProfile, horizon: Option<i64>) -> Analysis {
    analyze_with_ids(profile, horizon, &[])
}

/// `analyze_in_memory`, optionally replacing the freshly generated property ids (in search
/// order) with fixed ones before permits are matched, so golden vectors are reproducible.
pub fn analyze_with_ids(
    profile: &BusinessExpansionProfile,
    horizon: Option<i64>,
    fixed_ids: &[Uuid],
) -> Analysis {
    let mut props = PropertyScoutService::new(Arc::new(FixtureCoStarProvider::new()))
        .search(profile.project_id, &PropertySearchRequest::default())
        .expect("fixture search");
    for (p, id) in props.iter_mut().zip(fixed_ids) {
        p.property_id = *id;
        p.property_identity.property_id = *id;
    }
    let permits = PermitIntelligenceService::new(Arc::new(
        FixtureLaPermitProvider::new().expect("permit snapshot"),
    ));
    let reqs = requirements_for(profile);
    let mut scenarios = IndexMap::new();
    let mut assessments = IndexMap::new();
    let mut work_items = IndexMap::new();
    let mut evidence = IndexMap::new();
    let mut out_props = Vec::new();
    for p in props {
        let res = permits
            .fetch_and_match(&p.property_identity, Some(profile.project_id))
            .expect("permits");
        let p = attach_permits(&p, res.permits, &res.datasets, "FIXTURE");
        let (a, w) = FeasibilityEngine
            .evaluate(profile, &reqs, &p, &res.evidence)
            .expect("feasibility");
        let s = FinancialCalculationService
            .scenario(profile, &p, &w, horizon, None)
            .expect("scenario");
        assessments.insert(p.property_id, a);
        work_items.insert(p.property_id, w);
        evidence.insert(p.property_id, res.evidence);
        scenarios.insert(p.property_id, s);
        out_props.push(p);
    }
    let ranking = RankingEngine
        .rank(profile, &out_props, &scenarios, &assessments, 1)
        .expect("ranking");
    Analysis {
        props: out_props
            .into_iter()
            .map(|p| (p.property_identity.street_address.clone(), p))
            .collect(),
        scenarios,
        assessments,
        work_items,
        evidence,
        ranking,
    }
}
