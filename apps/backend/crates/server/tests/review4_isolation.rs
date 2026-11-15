//! REVIEW (temporary, part 4 independent review): every call-flow route with a second project of
//! the same owner and with a second owner.

mod common;

use std::sync::Arc;

use serde_json::{json, Value};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use uuid::Uuid;

use common::TestApp;

async fn searched(t: &TestApp, token: &str) -> (String, String) {
    let p = t.create_project(token, &common::demo_project_body()).await;
    let pid = p["project"]["project_id"].as_str().unwrap().to_owned();
    let r = t
        .post(&format!("/api/v1/projects/{pid}/properties/search"), Some(token), &json!({"city": "Los Angeles", "state": "CA"}))
        .await;
    assert_eq!(r.status.as_u16(), 202);
    let props = t.get(&format!("/api/v1/projects/{pid}/properties"), Some(token)).await;
    (pid, props.body[0]["property_id"].as_str().unwrap().to_owned())
}

both_backends!(every_route_with_a_second_project_and_a_second_owner, |db| {
    let t = TestApp::new(db, &[]);
    t.app.extensions.insert(Arc::new(OwnerNotify::new(Arc::new(RecordingNotifier::new()))));
    let owner = Uuid::new_v4();
    let tok = t.token_for(owner);
    let (pa, propa) = searched(&t, &tok).await;
    let (pb, propb) = searched(&t, &tok).await; // same owner, project B
    let stranger = t.token_for(Uuid::new_v4());
    let (ps, props_) = searched(&t, &stranger).await;
    // A: a simulated call through an accepted handoff, leaving records of every kind.
    let out = t
        .post(&format!("/api/v1/projects/{pa}/outreach"), Some(&tok),
              &json!({"property_id": propa, "channel": "VOICE", "recipient": {"name": "D", "phone": "+15555550100"}, "purpose": "p"}))
        .await;
    assert_eq!(out.status.as_u16(), 202);
    let d = t
        .post(&format!("/api/v1/approvals/{}/decision", out.body["approval"]["approval_id"].as_str().unwrap()), Some(&tok),
              &json!({"decision": "APPROVED", "action_payload_hash": out.body["approval"]["action_payload_hash"]}))
        .await;
    assert_eq!(d.status.as_u16(), 200);
    let comm = out.body["communication"]["communication_id"].as_str().unwrap().to_owned();
    // Outreach for A's property under project B, and under the stranger's project.
    let mut results: Vec<(String, u16)> = Vec::new();
    for (label, project, token, prop) in [
        ("B: outreach with A's property", pb.clone(), tok.clone(), propa.clone()),
        ("stranger: outreach with A's property", ps.clone(), stranger.clone(), propa.clone()),
        ("stranger: outreach under A", pa.clone(), stranger.clone(), props_.clone()),
        ("A: outreach with B's property", pa.clone(), tok.clone(), propb.clone()),
    ] {
        let r = t
            .post(&format!("/api/v1/projects/{project}/outreach"), Some(&token),
                  &json!({"property_id": prop, "channel": "VOICE", "recipient": {"name": "D", "phone": "+15555550100"}, "purpose": "p"}))
            .await;
        results.push((label.into(), r.status.as_u16()));
        assert_eq!(r.status.as_u16(), 404, "{label}: {}", r.text);
    }
    for (label, project, token) in [("B", pb.clone(), tok.clone()), ("stranger own", ps.clone(), stranger.clone()), ("stranger under A", pa.clone(), stranger.clone())] {
        let r = t.post(&format!("/api/v1/projects/{project}/calls"), Some(&token), &json!({"communication_id": comm})).await;
        results.push((format!("{label}: start A's communication"), r.status.as_u16()));
        assert_eq!(r.status.as_u16(), 404, "{label}");
    }
    let call = t.post(&format!("/api/v1/projects/{pa}/calls"), Some(&tok), &json!({"communication_id": comm})).await;
    assert_eq!(call.status.as_u16(), 202);
    let cid = call.body["call_id"].as_str().unwrap().to_owned();
    for (label, project, token) in [("B", pb.clone(), tok.clone()), ("stranger own", ps.clone(), stranger.clone()), ("stranger under A", pa.clone(), stranger.clone())] {
        let r = t.post(&format!("/api/v1/projects/{project}/calls/{cid}/simulate"), Some(&token), &json!({"owner_action": "later"})).await;
        results.push((format!("{label}: simulate A's call"), r.status.as_u16()));
        assert_eq!(r.status.as_u16(), 404, "{label}");
    }
    // Leave A's handoff pending: run the simulator to the escalation with no response is not
    // possible through the route, so answer LATER and then read the handoff id.
    let sim = t.post(&format!("/api/v1/projects/{pa}/calls/{cid}/simulate"), Some(&tok), &json!({"owner_action": "later"})).await;
    assert_eq!(sim.status.as_u16(), 200, "{}", sim.text);
    let hs = t.get(&format!("/api/v1/projects/{pa}/handoffs"), Some(&tok)).await;
    let hid = hs.body[0]["handoff_request_id"].as_str().unwrap().to_owned();
    for (label, project, token) in [("B", pb.clone(), tok.clone()), ("stranger own", ps.clone(), stranger.clone()), ("stranger under A", pa.clone(), stranger.clone())] {
        let r = t.post(&format!("/api/v1/projects/{project}/handoffs/{hid}/response"), Some(&token), &json!({"response": "DECLINED"})).await;
        results.push((format!("{label}: answer A's handoff"), r.status.as_u16()));
        assert_eq!(r.status.as_u16(), 404, "{label}");
    }
    // Lists of B and of the stranger hold nothing of A; lists of A under a stranger are 404.
    for route in ["communications", "calls", "handoffs"] {
        for (label, project, token, want) in [("B", pb.clone(), tok.clone(), 200u16), ("stranger own", ps.clone(), stranger.clone(), 200), ("stranger under A", pa.clone(), stranger.clone(), 404)] {
            let r = t.get(&format!("/api/v1/projects/{project}/{route}"), Some(&token)).await;
            results.push((format!("{label}: list {route}"), r.status.as_u16()));
            assert_eq!(r.status.as_u16(), want, "{label} {route}");
            if want == 200 {
                assert!(!r.text.contains(&pa), "{label} {route} shows project A's records");
                assert_eq!(r.body.as_array().map(Vec::len), Some(0), "{label} {route}");
            }
        }
        let r = t.get(&format!("/api/v1/projects/{pa}/{route}"), None).await;
        assert_eq!(r.status.as_u16(), 401);
    }
    let a_calls: Value = t.get(&format!("/api/v1/projects/{pa}/calls"), Some(&tok)).await.body;
    assert_eq!(a_calls.as_array().unwrap().len(), 1);
    for (l, s) in &results {
        println!("REVIEW isolation {l}: {s}");
    }
    println!("REVIEW isolation checks: {}", results.len());
});
