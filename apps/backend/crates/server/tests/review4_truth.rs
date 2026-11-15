//! REVIEW (temporary, part 4 independent review): what the broker says stays reported evidence;
//! scripted lines during handoffs; the join token; handoff isolation.

mod common;

use std::sync::Arc;

use serde_json::{json, Value};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::store::comms::{CALLS, OWNER_CHANNELS};
use ss_server::store::repo::EVIDENCE;
use ss_server::store::Query;
use uuid::Uuid;

use common::TestApp;

struct Ctx {
    t: TestApp,
    token: String,
    owner: Uuid,
    fake: Arc<RecordingNotifier>,
}

impl Ctx {
    fn new(db: &common::TestDb) -> Ctx {
        let t = TestApp::new(db, &[]);
        let fake = Arc::new(RecordingNotifier::new());
        t.app.extensions.insert(Arc::new(OwnerNotify::new(fake.clone())));
        let owner = Uuid::new_v4();
        let token = t.token_for(owner);
        Ctx { t, token, owner, fake }
    }

    async fn project(&self) -> (Uuid, Value) {
        let p = self.t.create_project(&self.token, &common::demo_project_body()).await;
        let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
        let r = self
            .t
            .post(&format!("/api/v1/projects/{pid}/properties/search"), Some(&self.token), &json!({"city": "Los Angeles", "state": "CA"}))
            .await;
        assert_eq!(r.status.as_u16(), 202);
        let props = self.t.get(&format!("/api/v1/projects/{pid}/properties"), Some(&self.token)).await;
        let hollywood = props
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["property_identity"]["normalized_address"].as_str().unwrap_or("").contains("HOLLYWOOD"))
            .cloned()
            .unwrap();
        (pid, hollywood)
    }

    async fn link(&self) {
        let mut tx = self.t.app.store.begin().await.unwrap();
        OWNER_CHANNELS
            .add(&mut *tx, &ss_domain::contracts::OwnerChannel {
                owner_channel_id: Uuid::new_v4(),
                owner_id: self.owner,
                platform: "imessage".into(),
                handle: "+15555550142".into(),
                status: ss_domain::contracts::OwnerChannelStatus::Linked,
                space_id: Some("spc".into()),
                agent_user_id: Some("usr".into()),
                last_inbound_message_id: None,
                linked_at: None,
                updated_at: ss_core::UtcDateTime::now(),
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn call(&self, pid: Uuid, prop: &str) -> Value {
        let out = self
            .t
            .post(
                &format!("/api/v1/projects/{pid}/outreach"),
                Some(&self.token),
                &json!({"property_id": prop, "channel": "VOICE",
                        "recipient": {"role": "broker", "name": "Dana", "phone": "+15555550100"},
                        "purpose": "Verify"}),
            )
            .await;
        let d = self
            .t
            .post(
                &format!("/api/v1/approvals/{}/decision", out.body["approval"]["approval_id"].as_str().unwrap()),
                Some(&self.token),
                &json!({"decision": "APPROVED", "action_payload_hash": out.body["approval"]["action_payload_hash"]}),
            )
            .await;
        assert_eq!(d.status.as_u16(), 200);
        let c = self
            .t
            .post(&format!("/api/v1/projects/{pid}/calls"), Some(&self.token), &json!({"communication_id": out.body["communication"]["communication_id"]}))
            .await;
        assert_eq!(c.status.as_u16(), 202, "{}", c.text);
        c.body
    }
}

both_backends!(negations_and_long_numbers_stay_labelled, |db| {
    let c = Ctx::new(db);
    c.link().await;
    let (pid, prop) = c.project().await;
    let prop_id = prop["property_id"].as_str().unwrap().to_owned();
    let before: Vec<(String, String)> = prop["infrastructure_assessments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["requirement_id"].as_str().unwrap().to_owned(), a["status"].as_str().unwrap().to_owned()))
        .collect();
    let call = c.call(pid, &prop_id).await;
    let cid = call["call_id"].as_str().unwrap();
    let answers = json!({
        "tenant_subpanel_amps": "It's a 12345 amp panel, honestly I don't know.",
        "in_unit_plumbing": "Absolutely not.",
        "hvac_zonable": "It does not.",
        "usable_area_sqft": "about 9,500,000 sf",
    });
    let r = c
        .t
        .post(&format!("/api/v1/projects/{pid}/calls/{cid}/simulate"), Some(&c.token), &json!({"owner_action": "later", "answers": answers}))
        .await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.text);
    for f in r.body["findings"].as_array().unwrap() {
        println!("REVIEW finding {} = {} {} | {}", f["claim_type"], f["value"], f["unit"], f.get("source_quote").or(f.get("quote")).unwrap_or(&Value::Null));
    }
    println!("REVIEW outcome summary: {}", r.body["outcome_summary"]);
    let mut tx = c.t.app.store.begin().await.unwrap();
    let ev = EVIDENCE
        .list(&mut *tx, Query::new().eq("property_id", prop_id.parse::<Uuid>().unwrap()))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for e in ev.iter().filter(|e| e.provider == "broker_call") {
        println!(
            "REVIEW evidence {} = {} class={} origin={} verification={} limits={:?}",
            e.observation_type, e.extracted_value, e.evidence_classification.as_str(), e.data_origin.as_str(), e.verification_status.as_str(), e.limitations
        );
        assert_eq!(e.evidence_classification.as_str(), "OWNER_REPORTED");
        assert!(e.limitations.iter().any(|l| l.contains("not independently verified")));
        assert!(e.limitations.iter().any(|l| l.contains("SIMULATED CALL")));
    }
    let after = c.t.get(&format!("/api/v1/projects/{pid}/properties/{prop_id}"), Some(&c.token)).await;
    for a in after.body["infrastructure_assessments"].as_array().unwrap() {
        let rid = a["requirement_id"].as_str().unwrap();
        let was = before.iter().find(|(r, _)| r == rid).map(|(_, s)| s.as_str()).unwrap_or("-");
        let broker = a["evidence_ids"].as_array().unwrap().iter().any(|id| ev.iter().any(|e| e.provider == "broker_call" && e.evidence_id.to_string() == id.as_str().unwrap()));
        println!(
            "REVIEW assessment {rid}: {was} -> {} | from broker evidence: {broker} | observed {} | review {} | uncertainty {} | limits {}",
            a["status"], a["observed_value"], a["review_status"], a["uncertainty"], a["limitations"]
        );
        if broker && a["status"] == "MEETS" {
            let text = a.to_string();
            assert!(text.contains("broker-reported") || text.contains("Broker statement") || text.contains("not independently verified"), "{rid}: MEETS from broker evidence without the label");
            assert_eq!(a["review_status"], "NEEDS_REVIEW");
        }
    }
    // Owner texts carry no token and no reference.
    c.t.app.owner_notify().idle().await;
    for r in c.fake.requests() {
        assert!(!r.text.contains("ref_"), "{}", r.text);
        println!("REVIEW owner text [{}]: {}", r.purpose, r.text);
    }
});

both_backends!(scripted_lines_during_handoffs, |db| {
    for action in ["accept", "later", "none"] {
        let c = Ctx::new(db);
        c.link().await;
        let (pid, prop) = c.project().await;
        let call = c.call(pid, prop["property_id"].as_str().unwrap()).await;
        let cid = call["call_id"].as_str().unwrap();
        let r = c
            .t
            .post(&format!("/api/v1/projects/{pid}/calls/{cid}/simulate"), Some(&c.token), &json!({"owner_action": action}))
            .await;
        assert_eq!(r.status.as_u16(), 200, "{}", r.text);
        let mut pending = false;
        for turn in r.body["transcript"].as_array().unwrap() {
            let (sp, tx) = (turn["speaker"].as_str().unwrap(), turn["text"].as_str().unwrap());
            println!("REVIEW [{action}] {sp}: {tx}");
            let lower = tx.to_lowercase();
            if sp == "assistant" {
                for bad in ["i agree", "we agree", "yes, we can", "we will pay", "deposit is fine", "i accept", "we accept", "deal", "sounds good, we"] {
                    assert!(!lower.contains(bad), "assistant committed: {tx}");
                }
                assert!(!pending || lower.contains("joining") || lower.contains("follow up"), "assistant spoke during a pending handoff: {tx}");
            }
            if sp == "assistant" && lower.contains("let me check if they can join") {
                pending = true;
            }
            if sp == "assistant" && (lower.contains("follow up with you directly") || lower.contains("joining")) {
                pending = false;
            }
        }
        println!("REVIEW [{action}] final state {} summary {}", r.body["state"], r.body["outcome_summary"]);
    }
});

both_backends!(join_token_and_handoff_isolation, |db| {
    let c = Ctx::new(db);
    c.link().await;
    let (pid, prop) = c.project().await;
    let call = c.call(pid, prop["property_id"].as_str().unwrap()).await;
    let cid: Uuid = call["call_id"].as_str().unwrap().parse().unwrap();
    // A token of the join purpose is refused on ordinary routes.
    let join = ss_server::security::issue_token_with("test-secret", c.owner, 180, &format!("join:{cid}"));
    let r = c.t.get(&format!("/api/v1/projects/{pid}/calls"), Some(&join)).await;
    println!("REVIEW join-purpose token on GET calls: {}", r.status);
    assert_eq!(r.status.as_u16(), 401);
    // Drive an escalation with webhooks-free orchestrator calls: simulate with no owner response
    // leaves no pending handoff, so use the orchestrator directly to reach HANDOFF_PENDING.
    let mut tx = c.t.app.store.begin().await.unwrap();
    let mut orch = ss_server::services::call_orchestration::CallOrchestrator::new(&c.t.app);
    let cs = ss_domain::integrations::telnyx::encode_client_state(&ss_domain::integrations::telnyx::client_state(&cid.to_string(), "counterparty"));
    let leg = call["provider_call_id"].as_str().unwrap();
    for (et, extra) in [("call.answered", json!({})), ("call.speak.ended", json!({})), ("call.transcription", json!({"transcription_data": {"transcript": "Will you need a guarantor?", "is_final": true}}))] {
        let mut p = json!({"call_control_id": leg, "client_state": cs});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        orch.handle_event(&mut *tx, &json!({"data": {"event_type": et, "payload": p}})).await.unwrap();
    }
    tx.commit().await.unwrap();
    let hs = c.t.get(&format!("/api/v1/projects/{pid}/handoffs"), Some(&c.token)).await;
    let hid = hs.body[0]["handoff_request_id"].as_str().unwrap().to_owned();
    // Another owner, under their own project; the same owner under another project.
    let c2 = Ctx { t: TestApp { app: c.t.app.clone(), router: c.t.router.clone(), kind: c.t.kind }, token: c.t.token_for(Uuid::new_v4()), owner: Uuid::new_v4(), fake: c.fake.clone() };
    let p2 = c2.t.create_project(&c2.token, &json!({"project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}})).await;
    let pid2 = p2["project"]["project_id"].as_str().unwrap();
    let p3 = c.t.create_project(&c.token, &json!({"project_name": "y", "market": {"city": "Los Angeles", "state": "CA"}})).await;
    let pid3 = p3["project"]["project_id"].as_str().unwrap();
    for (label, project, token) in [("stranger, own project", pid2.to_owned(), c2.token.clone()), ("stranger, victim's project", pid.to_string(), c2.token.clone()), ("owner, other project", pid3.to_owned(), c.token.clone())] {
        let r = c.t.post(&format!("/api/v1/projects/{project}/handoffs/{hid}/response"), Some(&token), &json!({"response": "ACCEPTED"})).await;
        println!("REVIEW handoff response, {label}: {} {}", r.status, r.body["error_code"]);
        assert_eq!(r.status.as_u16(), 404);
    }
    let mut tx = c.t.app.store.begin().await.unwrap();
    let still = CALLS.require(&mut *tx, cid).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(still.state.as_str(), "HANDOFF_PENDING");
    // Double answers and answers after the call ended.
    let ok = c.t.post(&format!("/api/v1/projects/{pid}/handoffs/{hid}/response"), Some(&c.token), &json!({"response": "DECLINED"})).await;
    let again = c.t.post(&format!("/api/v1/projects/{pid}/handoffs/{hid}/response"), Some(&c.token), &json!({"response": "ACCEPTED"})).await;
    let bad = c.t.post(&format!("/api/v1/projects/{pid}/handoffs/{hid}/response"), Some(&c.token), &json!({"response": "NOTIFIED"})).await;
    println!("REVIEW handoff DECLINED {} -> state {}; then ACCEPTED {} {}; then NOTIFIED {} {}", ok.status, ok.body["state"], again.status, again.body["error_code"], bad.status, bad.body["error_code"]);
    assert_eq!(again.status.as_u16(), 409);
});
