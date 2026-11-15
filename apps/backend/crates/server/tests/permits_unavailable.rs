//! A permit provider that fails must not fail the whole property search (permits-2).
//!
//! Deliberate difference from Python (`apps/api/app/workflows/property_analysis.py`, which lets
//! the `ProviderError` fail the task): with a stub permit provider that fails, the search task
//! COMPLETES, listings, costs and the ranking exist, the `la_open_data` manifest entry and
//! `missing_information` say the permit records were unavailable, no permit record is attached,
//! the assessments that depended on permit evidence are UNKNOWN, and after a timeout the
//! remaining properties of that search do not call the provider again. A failure that is not a
//! provider error still fails the task. On both stores.

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};
use ss_core::DataOrigin;
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{ProviderError, ProviderErrorKind};
use ss_domain::integrations::la_permits::{LaPermitProvider, RawPermit};
use ss_server::integrations::Integrations;

use common::*;

/// What the stub does on every `search_by_address_prefix`.
#[derive(Clone)]
enum Script {
    Fail(ProviderErrorKind),
    /// A successful answer with no records.
    Empty,
    /// A record whose `primary_address` is not a string: a non-provider (data) failure.
    Malformed,
}

struct StubPermits {
    script: Script,
    calls: Arc<AtomicUsize>,
}

impl LaPermitProvider for StubPermits {
    fn mode(&self) -> &str {
        "live"
    }
    fn search_by_address_prefix(
        &self,
        _house_number: &str,
        _direction: Option<&str>,
        _street_name: &str,
        _zip_code: Option<&str>,
    ) -> Result<Vec<RawPermit>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.script {
            Script::Fail(kind) => Err(ProviderError::new(
                "la_open_data",
                *kind,
                "scripted failure",
            )),
            Script::Empty => Ok(vec![]),
            Script::Malformed => {
                let mut record = Map::new();
                record.insert("permit_nbr".into(), json!("X-1"));
                record.insert("primary_address".into(), json!(5380));
                Ok(vec![RawPermit {
                    dataset: "ysqd-apz7".into(),
                    record,
                    data_origin: DataOrigin::Live,
                    source_url: "stub://x".into(),
                }])
            }
        }
    }
    fn get_permit(&self, _permit_number: &str) -> Result<Vec<RawPermit>, ProviderError> {
        unreachable!("not used by the search")
    }
    fn status(&self) -> ProviderStatusEntry {
        unreachable!("not used")
    }
}

/// Install the stub as the permit provider (the other providers stay the fixture ones).
fn install(t: &TestApp, script: Script) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let base = Integrations::from_settings(&t.app.settings);
    t.app.extensions.insert(Arc::new(Integrations {
        permits: Arc::new(StubPermits {
            script,
            calls: calls.clone(),
        }),
        ..base
    }));
    calls
}

async fn new_project(t: &TestApp, tok: &str) -> String {
    t.create_project(tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn search(t: &TestApp, tok: &str, pid: &str) -> Resp {
    t.post(
        &format!("/api/v1/projects/{pid}/properties/search"),
        Some(tok),
        &json!({"city": "Los Angeles", "state": "CA"}),
    )
    .await
}

fn street(p: &Value) -> String {
    p["property_identity"]["street_address"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Per street address: requirement id -> (status, cites permit evidence).
async fn assessments(t: &TestApp, tok: &str, pid: &str) -> Statuses {
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(tok))
        .await
        .body;
    let mut out = HashMap::new();
    for p in props.as_array().unwrap() {
        let prop_id = p["property_id"].as_str().unwrap();
        let ev = t
            .get(
                &format!("/api/v1/projects/{pid}/properties/{prop_id}/evidence"),
                Some(tok),
            )
            .await
            .body;
        let permit_ev: HashSet<String> = ev
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["provider"] == "la_open_data")
            .map(|e| e["evidence_id"].as_str().unwrap().to_owned())
            .collect();
        let feas = t
            .get(
                &format!("/api/v1/projects/{pid}/properties/{prop_id}/feasibility"),
                Some(tok),
            )
            .await
            .body;
        let mut m = HashMap::new();
        for a in feas.as_array().unwrap() {
            let cites = a["evidence_ids"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| permit_ev.contains(e.as_str().unwrap()));
            m.insert(
                a["requirement_id"].as_str().unwrap().to_owned(),
                (a["status"].as_str().unwrap().to_owned(), cites),
            );
        }
        out.insert(street(p), m);
    }
    out
}

/// Everything the owner should see after a search whose permit step failed with `kind`.
async fn assert_unavailable_outcome(t: &TestApp, tok: &str, pid: &str, r: &Resp, kind: &str) {
    assert_eq!(r.status.as_u16(), 202, "{}", r.text);
    let task = &r.body["task"];
    assert_eq!(task["status"], "COMPLETED", "{task}");
    // The task result says the permit step was skipped, per property, and why.
    let skipped = task["output_reference"]["permit_step_skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("no permit_step_skipped in {task}"));
    assert_eq!(skipped.len(), 2, "{task}");
    assert!(task["output_reference"]["ranking_id"].is_string());

    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(tok))
        .await
        .body;
    let props = props.as_array().unwrap();
    assert_eq!(props.len(), 2);
    for p in props {
        assert_conforms("PropertyDigitalProfile", p);
        assert!(p["commercial_listing"].is_object(), "listing kept");
        assert_eq!(p["permit_records"], json!([]), "nothing attached");
        let entries: Vec<&Value> = p["source_manifest"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["provider"] == "la_open_data")
            .collect();
        assert_eq!(entries.len(), 1, "{p}");
        let m = entries[0];
        assert_eq!(m["status"], "BLOCKED", "{m}");
        assert_eq!(m["record_count"], 0);
        assert_eq!(m["retrieved_at"], Value::Null, "nothing was retrieved");
        let note = m["note"].as_str().unwrap();
        assert!(note.contains("could not be reached"), "{note}");
        assert!(note.contains(kind), "{note}");
        assert!(note.contains("unknown, not zero"), "{note}");
        let missing: Vec<&str> = p["missing_information"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            missing
                .iter()
                .any(|s| s.starts_with("Permit records unavailable")),
            "{missing:?}"
        );
        assert!(
            !missing
                .iter()
                .any(|s| s.starts_with("No permit records matched")),
            "an unreachable provider is not 'no permits': {missing:?}"
        );
        let pid_ = p["property_id"].as_str().unwrap();
        let permits = t
            .get(
                &format!("/api/v1/projects/{pid}/properties/{pid_}/permits"),
                Some(tok),
            )
            .await
            .body;
        assert_eq!(permits, json!([]));
    }

    // Costs and the ranking exist; the ranking counts the UNKNOWNs.
    let scenarios = t
        .get(&format!("/api/v1/projects/{pid}/cost-scenarios"), Some(tok))
        .await
        .body;
    assert_eq!(scenarios.as_array().unwrap().len(), 2);
    let rankings = t
        .get(&format!("/api/v1/projects/{pid}/rankings"), Some(tok))
        .await
        .body;
    assert_eq!(rankings.as_array().unwrap().len(), 1);
    assert_conforms("PropertyRankingResult", &rankings[0]);
    let ranked = rankings[0]["ranked_properties"].as_array().unwrap();
    assert_eq!(ranked.len(), 2);
    let now = assessments(t, tok, pid).await;
    let by_id: HashMap<String, String> = props
        .iter()
        .map(|p| (p["property_id"].as_str().unwrap().to_owned(), street(p)))
        .collect();
    for r in ranked {
        let s = &by_id[r["property_id"].as_str().unwrap()];
        let unknown = now[s].values().filter(|(st, _)| st == "UNKNOWN").count();
        assert_eq!(r["unknown_requirement_count"], json!(unknown), "{r}");
    }

    // The feed: no "permits ingested" claim, the task completed.
    let events = t
        .get(&format!("/api/v1/projects/{pid}/events"), Some(tok))
        .await
        .body["events"]
        .clone();
    let types: Vec<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert!(!types.contains(&"property.permits.ingested"), "{types:?}");
    for want in [
        "property.search.completed",
        "property.analysis.completed",
        "property.ranking.updated",
        "task.completed",
    ] {
        assert!(types.contains(&want), "{want} missing from {types:?}");
    }
}

type Statuses = HashMap<String, HashMap<String, (String, bool)>>;

/// The permit-failure run equals a run whose provider answered "no records" (`empty`):
/// "produced as they would be with no permit records". Requirements that permit evidence backed
/// in the fixture run (`baseline`) cite no permit evidence now and are never MEETS; at least one
/// of them is UNKNOWN now (the others fall back to non-permit evidence, e.g. a broker statement).
fn assert_permit_dependent_unknown(baseline: &Statuses, empty: &Statuses, now: &Statuses) {
    let mut to_unknown = 0;
    for (s, reqs) in baseline {
        for (rid, (status, cites)) in reqs {
            let (now_status, now_cites) = &now[s][rid];
            assert!(!now_cites, "{s} {rid} cites permit evidence");
            assert_eq!(
                now_status, &empty[s][rid].0,
                "{s} {rid} differs from no permits"
            );
            if *cites && status != "UNKNOWN" {
                assert_ne!(now_status, "MEETS", "{s} {rid} was {status} on permits");
                if now_status == "UNKNOWN" {
                    to_unknown += 1;
                }
            }
        }
        assert_eq!(reqs.len(), now[s].len());
    }
    assert!(
        to_unknown > 0,
        "the fixture run must have permit-backed assessments that become UNKNOWN"
    );
}

/// The assessments of a search whose permit provider answered with no records.
async fn empty_run(t: &TestApp, tok: &str) -> Statuses {
    install(t, Script::Empty);
    let pid = new_project(t, tok).await;
    let r = search(t, tok, &pid).await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    assessments(t, tok, &pid).await
}

both_backends!(unavailable_provider_completes_the_search, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    // Baseline in fixture mode, before the stub is installed.
    let base_pid = new_project(&t, &tok).await;
    let r = search(&t, &tok, &base_pid).await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    assert!(
        r.body["task"]["output_reference"]
            .get("permit_step_skipped")
            .is_none(),
        "fixture mode output unchanged"
    );
    let baseline = assessments(&t, &tok, &base_pid).await;

    let empty = empty_run(&t, &tok).await;
    let calls = install(&t, Script::Fail(ProviderErrorKind::Unavailable));
    let pid = new_project(&t, &tok).await;
    let r = search(&t, &tok, &pid).await;
    assert_unavailable_outcome(&t, &tok, &pid, &r, "UNAVAILABLE").await;
    // An immediate failure does not stop the next property from trying.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let skipped = r.body["task"]["output_reference"]["permit_step_skipped"]
        .as_array()
        .unwrap()
        .clone();
    for s in &skipped {
        assert_eq!(s["error_kind"], "UNAVAILABLE");
        assert_eq!(s["provider_called"], true);
    }
    assert_permit_dependent_unknown(&baseline, &empty, &assessments(&t, &tok, &pid).await);
});

both_backends!(timeout_skips_the_remaining_properties, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let base_pid = new_project(&t, &tok).await;
    search(&t, &tok, &base_pid).await;
    let baseline = assessments(&t, &tok, &base_pid).await;

    let empty = empty_run(&t, &tok).await;
    let calls = install(&t, Script::Fail(ProviderErrorKind::Timeout));
    let pid = new_project(&t, &tok).await;
    let r = search(&t, &tok, &pid).await;
    assert_unavailable_outcome(&t, &tok, &pid, &r, "TIMEOUT").await;
    // One property waited for the provider; the other did not call it.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let skipped = r.body["task"]["output_reference"]["permit_step_skipped"]
        .as_array()
        .unwrap()
        .clone();
    let called: Vec<bool> = skipped
        .iter()
        .map(|s| s["provider_called"].as_bool().unwrap())
        .collect();
    assert_eq!(called, vec![true, false]);
    for s in &skipped {
        assert_eq!(s["error_kind"], "TIMEOUT");
    }
    assert_permit_dependent_unknown(&baseline, &empty, &assessments(&t, &tok, &pid).await);
});

both_backends!(rate_limited_and_bad_answer_complete_too, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    for (kind, name) in [
        (ProviderErrorKind::RateLimited, "RATE_LIMITED"),
        (ProviderErrorKind::BadResponse, "BAD_RESPONSE"),
    ] {
        let calls = install(&t, Script::Fail(kind));
        let pid = new_project(&t, &tok).await;
        let r = search(&t, &tok, &pid).await;
        assert_unavailable_outcome(&t, &tok, &pid, &r, name).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
});

both_backends!(non_provider_failure_still_fails_the_task, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    install(&t, Script::Malformed);
    let pid = new_project(&t, &tok).await;
    let r = search(&t, &tok, &pid).await;
    assert_eq!(r.status.as_u16(), 202, "{}", r.text);
    assert_eq!(r.body["task"]["status"], "FAILED", "{}", r.text);
    assert_eq!(r.body["task"]["error_code"], "TypeError");
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    assert_eq!(props, json!([]), "nothing of the failed slice is kept");
});

both_backends!(analyze_route_also_continues, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let pid = new_project(&t, &tok).await;
    search(&t, &tok, &pid).await;
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    let prop_id = props[0]["property_id"].as_str().unwrap().to_owned();
    let had = props[0]["permit_records"].as_array().unwrap().len();
    assert!(had > 0, "fixture permits stored by the first search");
    install(&t, Script::Fail(ProviderErrorKind::Unavailable));
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/{prop_id}/analyze"),
            Some(&tok),
            &json!({}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let p = t
        .get(
            &format!("/api/v1/projects/{pid}/properties/{prop_id}"),
            Some(&tok),
        )
        .await
        .body;
    let m = p["source_manifest"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["provider"] == "la_open_data")
        .unwrap()
        .clone();
    assert_eq!(m["status"], "BLOCKED", "{m}");
    // Records stored by an earlier, successful retrieval stay (they are what the feasibility
    // evidence cites); the note says they come from that earlier retrieval.
    assert_eq!(m["record_count"], json!(had));
    assert!(
        m["note"].as_str().unwrap().contains("earlier retrieval"),
        "{m}"
    );
});
