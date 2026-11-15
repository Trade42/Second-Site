//! A property search for a profile with no room program must COMPLETE (empty-program-1).
//!
//! Deliberate difference from Python (`apps/api/app/domains/feasibility/engine.py`, whose
//! partitions work item of quantity 0 fails the task with a `ValidationError`): the search
//! completes, every property is assessed and costed and the ranking exists; the enclosed-rooms
//! and area assessments are UNKNOWN because no rooms were stated; no partitions work item is
//! priced and the cost scenario's excluded costs say the room build-out is not included. On
//! both stores.

mod common;

use serde_json::{json, Value};

use common::*;

async fn project_with_rooms(t: &TestApp, tok: &str, rooms: Value) -> String {
    let mut body = demo_project_body();
    body["business_profile"]["space_requirements"] = json!({"required_rooms": rooms});
    t.create_project(tok, &body).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn assert_completes(t: &TestApp, tok: &str, rooms: Value) {
    let pid = project_with_rooms(t, tok, rooms.clone()).await;
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(tok),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 202, "{}", r.text);
    let task = &r.body["task"];
    assert_eq!(task["status"], "COMPLETED", "{rooms}: {task}");
    assert_eq!(task["error_code"], Value::Null, "{task}");
    assert!(task["output_reference"]["ranking_id"].is_string(), "{task}");

    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(tok))
        .await
        .body;
    let props = props.as_array().unwrap();
    assert_eq!(props.len(), 2);
    for p in props {
        let prop_id = p["property_id"].as_str().unwrap();
        let feas = t
            .get(
                &format!("/api/v1/projects/{pid}/properties/{prop_id}/feasibility"),
                Some(tok),
            )
            .await
            .body;
        let feas = feas.as_array().unwrap();
        let by = |suffix: &str| {
            feas.iter()
                .find(|a| a["requirement_id"].as_str().unwrap().ends_with(suffix))
                .unwrap_or_else(|| panic!("no {suffix} in {feas:?}"))
        };
        for suffix in ["space.enclosed_rooms", "space.min_area"] {
            let a = by(suffix);
            assert_conforms("InfrastructureAssessment", a);
            assert_eq!(a["status"], "UNKNOWN", "{a}");
            assert!(
                a["uncertainty"]
                    .as_str()
                    .unwrap()
                    .starts_with("No room program"),
                "{a}"
            );
        }
        // Electrical is assessed as before.
        assert!(feas.iter().any(|a| a["requirement_id"]
            .as_str()
            .unwrap()
            .ends_with("electrical.tenant_service")));
    }

    let scenarios = t
        .get(&format!("/api/v1/projects/{pid}/cost-scenarios"), Some(tok))
        .await
        .body;
    let scenarios = scenarios.as_array().unwrap();
    assert_eq!(scenarios.len(), 2);
    for s in scenarios {
        assert_conforms("CostScenario", s);
        let lines = s["line_items"].as_array().unwrap();
        assert!(lines.iter().all(|li| li["category"] != "PARTITIONS"), "{s}");
        assert!(
            lines.iter().any(|li| li["category"] == "PERMITS_FEES"),
            "{s}"
        );
        let excluded = s["excluded_costs"].as_array().unwrap();
        assert!(
            excluded
                .iter()
                .any(|x| x.as_str().unwrap().contains("no room program stated yet")),
            "{s}"
        );
    }

    let rankings = t
        .get(&format!("/api/v1/projects/{pid}/rankings"), Some(tok))
        .await
        .body;
    let latest = rankings.as_array().unwrap().last().unwrap();
    assert_eq!(latest["ranked_properties"].as_array().unwrap().len(), 2);
}

both_backends!(search_without_rooms_completes, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    assert_completes(&t, &tok, json!({})).await;
    assert_completes(&t, &tok, json!({"treatment": 0})).await;
});

both_backends!(a_real_program_still_prices_partitions, |db| {
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let pid = project_with_rooms(&t, &tok, json!({"treatment": 3})).await;
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let scenarios = t
        .get(
            &format!("/api/v1/projects/{pid}/cost-scenarios"),
            Some(&tok),
        )
        .await
        .body;
    for s in scenarios.as_array().unwrap() {
        assert!(s["line_items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|li| li["category"] == "PARTITIONS"));
        assert!(!s["excluded_costs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x.as_str().unwrap().contains("no room program")));
    }
});
