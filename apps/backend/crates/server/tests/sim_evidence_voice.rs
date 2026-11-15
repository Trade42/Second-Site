//! A call's "known facts" never include a simulated answer about a real listing (the review's
//! F1 on sim-evidence-1).
//!
//! The one reader of stored evidence in the voice path is `voice_session_config::build`: its
//! `known_facts` go into the session instructions, into the Response Agent's `CallContext`
//! (which answers a counterparty's question from them) and from there into its directions. It
//! now applies the engine's rule (`ss_domain::feasibility::engine::counts_for`): a
//! `FIXTURE_SYNTHETIC` row is a known fact only of a listing that is itself `FIXTURE_SYNTHETIC`.
//! Fixture telephony only: nothing is sent anywhere.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use ss_core::DataOrigin;
use ss_domain::communications::call_purposes::Approved;
use ss_domain::communications::response_agent::{topic_of_claim, KnownFact};
use ss_domain::contracts::{CallSession, SourceEvidence};
use ss_server::services::voice_session_config as vsc;
use ss_server::store::comms::CALLS;
use ss_server::store::repo::{EVIDENCE, PROPERTIES};
use ss_server::store::Query;
use uuid::Uuid;

use common::{demo_project_body, TestApp, TestDb};

struct Flow {
    t: TestApp,
    token: String,
    pid: String,
    prop: Uuid,
}

/// The demo project with its first fixture listing set to `origin`, evaluated.
async fn project(db: &TestDb, origin: DataOrigin) -> Flow {
    let t = TestApp::new(db, &[]);
    let (token, _user) = t.dev_token().await;
    let p = t.create_project(&token, &demo_project_body()).await;
    let pid = p["project"]["project_id"].as_str().unwrap().to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&token),
            &json!({"city": "Los Angeles", "state": "CA"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&token))
        .await;
    let prop: Uuid = props.body[0]["property_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        props.body[0]["commercial_listing"]["data_origin"],
        "FIXTURE_SYNTHETIC"
    );
    if origin != DataOrigin::FixtureSynthetic {
        let mut tx = t.app.store.begin().await.unwrap();
        let mut stored = PROPERTIES.require(&mut *tx, prop).await.unwrap();
        stored.commercial_listing.as_mut().unwrap().data_origin = origin;
        PROPERTIES.upsert(&mut *tx, &stored).await.unwrap();
        tx.commit().await.unwrap();
        let r = t
            .post(
                &format!("/api/v1/projects/{pid}/properties/{prop}/analyze"),
                Some(&token),
                &json!({}),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    }
    Flow {
        t,
        token,
        pid,
        prop,
    }
}

impl Flow {
    /// Outreach, approval and a simulated call, not yet run. Returns the stored call.
    async fn new_call(&self, purpose: &str, phone: &str) -> CallSession {
        let (t, pid, token) = (&self.t, &self.pid, &self.token);
        let body = json!({
            "property_id": self.prop, "channel": "VOICE", "purpose": purpose,
            "recipient": {"role": "broker", "name": "Dana Broker", "phone": phone},
        });
        let out = t
            .send(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/v1/projects/{pid}/outreach"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await;
        assert_eq!(out.status, StatusCode::ACCEPTED, "{}", out.text);
        let appr = &out.body["approval"];
        let ok = t
            .post(
                &format!(
                    "/api/v1/approvals/{}/decision",
                    appr["approval_id"].as_str().unwrap()
                ),
                Some(token),
                &json!({"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]}),
            )
            .await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        let call = t
            .post(
                &format!("/api/v1/projects/{pid}/calls"),
                Some(token),
                &json!({"communication_id": out.body["communication"]["communication_id"]}),
            )
            .await;
        assert_eq!(call.status, StatusCode::ACCEPTED, "{}", call.text);
        assert_eq!(call.body["simulated"], true);
        let id: Uuid = call.body["call_id"].as_str().unwrap().parse().unwrap();
        let mut tx = t.app.store.begin().await.unwrap();
        let c = CALLS.require(&mut *tx, id).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(c.property_id, Some(self.prop));
        c
    }

    /// The simulated call run to its end; the finished call as the route gives it.
    async fn simulate(&self, call: &CallSession) -> Value {
        let done = self
            .t
            .post(
                &format!(
                    "/api/v1/projects/{}/calls/{}/simulate",
                    self.pid, call.call_id
                ),
                Some(&self.token),
                &json!({"owner_action": "accept"}),
            )
            .await;
        assert_eq!(done.status, StatusCode::OK, "{}", done.text);
        assert_eq!(done.body["state"], "COMPLETED");
        done.body
    }

    async fn evidence(&self) -> Vec<SourceEvidence> {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let ev = EVIDENCE
            .list(&mut *tx, Query::new().eq("property_id", self.prop))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        ev
    }

    /// The session the server would give a bridged call of `call`, as it stands in the store.
    async fn session(&self, call: &CallSession) -> vsc::BuiltSession {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let b = vsc::build(
            &mut *tx,
            call,
            "Avery Owner",
            "ara",
            &[],
            &Approved::default(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        b
    }
}

/// The known facts as `build` made them at `HEAD` (before the rule): every stored row of the
/// property with a value, in store order, at most `MAX_FACTS`. A recorded copy of that loop.
fn facts_at_head(rows: &[SourceEvidence]) -> Vec<KnownFact> {
    rows.iter()
        .filter(|e| !e.extracted_value.is_null())
        .take(vsc::MAX_FACTS)
        .map(|e| KnownFact {
            topic: topic_of_claim(&e.observation_type).to_owned(),
            statement: format!(
                "{}: {}{}",
                e.observation_type,
                ss_domain::py::value_str(&e.extracted_value),
                e.unit
                    .as_deref()
                    .map(|u| format!(" {u}"))
                    .unwrap_or_default()
            ),
            classification: e.evidence_classification,
        })
        .collect()
}

/// The whole session as the model gets it, as one string.
fn wire(b: &vsc::BuiltSession) -> String {
    serde_json::to_string(&b.session.session_update()).unwrap()
}

fn claim_types(c: &CallSession) -> Vec<String> {
    c.script.iter().map(|q| q.claim_type.clone()).collect()
}

/// The simulated answers the script gives (stored as `FIXTURE_SYNTHETIC` broker-call evidence).
fn simulated(rows: &[SourceEvidence]) -> Vec<&SourceEvidence> {
    rows.iter()
        .filter(|e| e.data_origin == DataOrigin::FixtureSynthetic && e.provider == "broker_call")
        .collect()
}

both_backends!(
    a_call_on_a_real_listing_is_not_told_a_simulated_answer,
    |db| {
        let f = project(db, DataOrigin::FixtureReal).await;
        let first = f.new_call("Verify electrical", "+15555550100").await;
        let before = f.session(&first).await;
        let rows_before = f.evidence().await;
        // Item 3: with no simulated answer stored, the facts are HEAD's, row for row.
        assert_eq!(before.context.known_facts, facts_at_head(&rows_before));

        let done = f.simulate(&first).await;
        let rows = f.evidence().await;
        let sim = simulated(&rows);
        assert!(sim.len() >= 2, "the script's answers are stored: {done}");
        assert!(sim.iter().any(|e| e.observation_type == "in_unit_plumbing"));

        // Item 1: the next call's session (the same call record, so nothing else differs) is
        // byte for byte the one from before the simulated answers existed.
        let after = f.session(&first).await;
        assert_eq!(before, after);
        assert_eq!(wire(&before), wire(&after));
        let said = facts_at_head(&sim.iter().map(|e| (*e).clone()).collect::<Vec<_>>());
        assert!(!said.is_empty());
        for fact in &said {
            assert!(
                !after.context.known_facts.contains(fact),
                "{}",
                fact.statement
            );
            assert!(
                !wire(&after).contains(&fact.statement),
                "{}",
                fact.statement
            );
            assert!(!after.opening_line.contains(&fact.statement));
        }
        assert!(!wire(&after).contains("in_unit_plumbing: True"));
        // At HEAD one was there (as many as fit under `MAX_FACTS` after the permit rows): the
        // recorded loop lists it for this very store.
        let at_head = facts_at_head(&rows);
        assert!(said.iter().any(|x| at_head.contains(x)), "{at_head:?}");
        // Item 3 again, with the simulated rows present: every other row is listed as at HEAD.
        let real: Vec<SourceEvidence> = rows
            .iter()
            .filter(|e| e.data_origin != DataOrigin::FixtureSynthetic)
            .cloned()
            .collect();
        assert_eq!(after.context.known_facts, facts_at_head(&real));
        println!(
            "FIXTURE_REAL: {} rows, {} simulated, {} known facts before and after",
            rows.len(),
            sim.len(),
            after.context.known_facts.len()
        );

        // The Response Agent answers "is there plumbing?" from the context's facts only: with
        // the simulated answer left out it has no record to cite on that topic.
        let topic = topic_of_claim("in_unit_plumbing");
        assert!(
            !after
                .context
                .known_facts
                .iter()
                .any(|k| k.topic == topic && k.statement.starts_with("in_unit_plumbing")),
            "{:?}",
            after.context.known_facts
        );

        // Item 4: a second simulated call asks what the first asked, and runs to its end.
        let second = f.new_call("Verify plumbing", "+15555550101").await;
        assert!(!first.script.is_empty());
        assert_eq!(claim_types(&first), claim_types(&second));
        let done2 = f.simulate(&second).await;
        assert_eq!(
            done["findings"].as_array().map(Vec::len),
            done2["findings"].as_array().map(Vec::len)
        );
        // And a live call's session after both is still without them.
        let live = CallSession {
            simulated: false,
            ..second.clone()
        };
        let s = wire(&f.session(&live).await);
        assert!(said.iter().all(|x| !s.contains(&x.statement)));
    }
);

both_backends!(the_demo_listing_s_session_is_what_it_was_at_head, |db| {
    let f = project(db, DataOrigin::FixtureSynthetic).await;
    let first = f.new_call("Verify electrical", "+15555550100").await;
    let before = f.session(&first).await;
    f.simulate(&first).await;
    let rows = f.evidence().await;
    let sim = simulated(&rows);
    assert!(sim.len() >= 2);
    let after = f.session(&first).await;
    // Item 2: the facts are HEAD's loop over the same store, simulated answers included;
    // everything else `build` makes is untouched, so the whole session is HEAD's.
    let head = facts_at_head(&rows);
    assert_eq!(after.context.known_facts, head);
    let rebuilt = vsc::BuiltSession {
        context: ss_domain::communications::response_agent::CallContext {
            known_facts: head.clone(),
            ..after.context.clone()
        },
        ..after.clone()
    };
    assert_eq!(rebuilt, after);
    assert_ne!(wire(&before), wire(&after), "the answers are now known");
    for e in &sim {
        let line = facts_at_head(std::slice::from_ref(*e));
        if rows
            .iter()
            .filter(|r| !r.extracted_value.is_null())
            .position(|r| r.evidence_id == e.evidence_id)
            .is_some_and(|i| i < vsc::MAX_FACTS)
        {
            assert!(wire(&after).contains(&line[0].statement), "{line:?}");
        }
    }
    println!(
        "FIXTURE_SYNTHETIC: {} rows, {} simulated, {} known facts",
        rows.len(),
        sim.len(),
        after.context.known_facts.len()
    );
});

/// REVIEW ADDITION (sim-evidence-2-review): a sentinel that occurs nowhere else. A stored
/// `FIXTURE_SYNTHETIC` row with it reaches nothing built for a real listing's call (context,
/// instructions, session JSON, opening line) and takes no place under `MAX_FACTS`; the same row
/// as `LIVE` evidence, or on a demo listing, is listed.
async fn add_row(f: &Flow, origin: DataOrigin, claim: &str, value: &str) {
    let mut ev = f.evidence().await.pop().expect("a stored permit row");
    ev.evidence_id = Uuid::new_v4();
    ev.provider = "broker_call".into();
    ev.data_origin = origin;
    ev.observation_type = claim.into();
    ev.extracted_value = json!(value);
    ev.unit = None;
    let mut tx = f.t.app.store.begin().await.unwrap();
    EVIDENCE.add(&mut *tx, &ev).await.unwrap();
    tx.commit().await.unwrap();
}

fn everything(b: &vsc::BuiltSession) -> String {
    format!("{b:?}\n{}\n{}", wire(b), b.session.instructions)
}

both_backends!(a_sentinel_simulated_answer_reaches_nothing, |db| {
    for listing in [
        DataOrigin::FixtureReal,
        DataOrigin::Live,
        DataOrigin::UserProvided,
    ] {
        let f = project(db, listing).await;
        let call = f.new_call("Verify electrical", "+15555550100").await;
        let before = f.session(&call).await;
        add_row(&f, DataOrigin::FixtureSynthetic, "zq_claim", "ZQX-7391").await;
        let after = f.session(&call).await;
        assert_eq!(before, after, "{listing:?}");
        let all = everything(&after);
        assert!(!all.contains("ZQX-7391") && !all.contains("zq_claim"));
        // The control: real evidence stored after it is listed, so the left-out row took no
        // place under the cut (19 permit facts, 20 allowed).
        add_row(&f, DataOrigin::Live, "zr_claim", "ZRX-7392").await;
        let live = CallSession {
            simulated: false,
            ..call.clone()
        };
        let all = everything(&f.session(&live).await);
        assert!(all.contains("zr_claim: ZRX-7392"), "{listing:?}");
        assert!(!all.contains("ZQX-7391") && !all.contains("zq_claim"));
    }
    let demo = project(db, DataOrigin::FixtureSynthetic).await;
    let call = demo.new_call("Verify electrical", "+15555550100").await;
    add_row(&demo, DataOrigin::FixtureSynthetic, "zq_claim", "ZQX-7391").await;
    assert!(everything(&demo.session(&call).await).contains("zq_claim: ZQX-7391"));
});
