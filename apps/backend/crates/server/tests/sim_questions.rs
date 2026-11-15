//! sim-questions-1: a simulated call the server leads itself asks the approved request's own
//! questions first, then the bank's. A question the script does not know gets one fixed line
//! from the simulated contact and stays unanswered: no value, no evidence. A request without
//! questions gives the call it gave before. Fixture voice only: nobody is called.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use ss_domain::communications::detect_escalation;
use ss_domain::contracts::{CallSession, CallState};
use ss_server::services::call_orchestration::{
    is_request_question, simulated_script, SIMULATED_LABEL,
};
use ss_server::services::call_simulator::{
    CallSimulator, FixtureBridge, DEFAULT_ANSWERS, MAX_TURNS, NO_SCRIPTED_ANSWER,
};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::store::comms::CALLS;
use ss_server::store::repo::EVIDENCE;
use ss_server::store::Query;
use uuid::Uuid;

use common::{demo_project_body, TestApp, TestDb};

const OWN: &str = "Is there a loading dock?";
const OWN_2: &str = "Who pays for the sign on the street?";
const BANK_AREA: &str = "What is the usable square footage of the suite?";
const AREA_ANSWER: &str = "It's about 1,250 usable square feet.";

struct S {
    t: TestApp,
    fake: Arc<RecordingNotifier>,
    token: String,
    pid: Uuid,
    prop: String,
}

async fn setup(db: &TestDb, bridge: Option<FixtureBridge>) -> S {
    let t = TestApp::new(db, &[]);
    let fake = Arc::new(RecordingNotifier::new());
    t.app
        .extensions
        .insert(Arc::new(OwnerNotify::new(fake.clone())));
    if let Some(b) = bridge {
        let vc = t.app.voice_calls();
        vc.set_launcher(Some(Arc::new(b)));
        vc.bind(&t.app);
    }
    let (token, _) = t.dev_token().await;
    let p = t.create_project(&token, &demo_project_body()).await;
    let pid: Uuid = p["project"]["project_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
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
    let prop = props.body[0]["property_id"].as_str().unwrap().to_owned();
    S {
        t,
        fake,
        token,
        pid,
        prop,
    }
}

impl S {
    async fn post(&self, path: &str, body: &Value) -> common::Resp {
        self.t
            .post(
                &format!("/api/v1/projects/{}{path}", self.pid),
                Some(&self.token),
                body,
            )
            .await
    }

    /// Outreach with these questions, the owner's approval, and the start of the simulated call.
    async fn started(&self, questions: &[&str]) -> CallSession {
        let mut body = json!({
            "property_id": self.prop,
            "channel": "VOICE",
            "recipient": {"role": "broker", "name": "Dana Broker", "phone": "+15555550100"},
            "purpose": "Verify the space",
        });
        if !questions.is_empty() {
            body["questions"] = json!(questions);
        }
        let out = self.post("/outreach", &body).await;
        assert_eq!(out.status, StatusCode::ACCEPTED, "{}", out.text);
        let appr = &out.body["approval"];
        let ok = self
            .t
            .post(
                &format!(
                    "/api/v1/approvals/{}/decision",
                    appr["approval_id"].as_str().unwrap()
                ),
                Some(&self.token),
                &json!({"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]}),
            )
            .await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        let comm = out.body["communication"]["communication_id"].clone();
        // Item 4: questions supplied at start are refused, and change nothing.
        let bad = self
            .post(
                "/calls",
                &json!({"communication_id": comm, "questions": ["Will you take $1?"]}),
            )
            .await;
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", bad.text);
        let call = self
            .post("/calls", &json!({"communication_id": comm}))
            .await;
        assert_eq!(call.status, StatusCode::ACCEPTED, "{}", call.text);
        assert_eq!(call.body["simulated"], true);
        serde_json::from_value(call.body).unwrap()
    }

    async fn simulate(&self, call: &CallSession, body: Value) -> CallSession {
        let done = self
            .post(&format!("/calls/{}/simulate", call.call_id), &body)
            .await;
        assert_eq!(done.status, StatusCode::OK, "{}", done.text);
        serde_json::from_value(done.body).unwrap()
    }

    async fn summary_text(&self) -> String {
        self.t.app.owner_notify().idle().await;
        let texts: Vec<String> = self
            .fake
            .requests()
            .into_iter()
            .filter(|r| r.purpose == "call_summary")
            .map(|r| r.text)
            .collect();
        assert_eq!(texts.len(), 1, "{texts:?}");
        texts[0].clone()
    }

    /// The observation types of the evidence stored for the call.
    async fn evidence_types(&self, call: &CallSession) -> Vec<String> {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let rows = EVIDENCE
            .list(&mut *tx, Query::new().eq("project_id", self.pid))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let id = call.call_id.to_string();
        rows.into_iter()
            .filter(|e| e.source_record_id.as_deref() == Some(id.as_str()))
            .inspect(|e| {
                assert!(e.limitations[0].starts_with("SIMULATED CALL"), "{e:?}");
                assert_eq!(e.data_origin, ss_core::DataOrigin::FixtureSynthetic);
            })
            .map(|e| e.observation_type)
            .collect()
    }
}

fn lines(call: &CallSession) -> Vec<String> {
    call.transcript
        .iter()
        .map(|t| format!("{}: {}", t.speaker, t.text))
        .collect()
}

fn show(what: &str, call: &CallSession) {
    println!("--- {what} ---");
    for l in lines(call) {
        println!("{l}");
    }
    println!("summary: {}", call.outcome_summary.as_deref().unwrap_or(""));
}

/// The position of `needle` among the transcript's lines.
fn at(call: &CallSession, needle: &str) -> usize {
    lines(call)
        .iter()
        .position(|l| l == needle)
        .unwrap_or_else(|| panic!("no line {needle:?} in {:#?}", lines(call)))
}

#[test]
fn the_fixed_line_is_no_escalation_and_the_script_has_no_duplicates() {
    assert!(detect_escalation(NO_SCRIPTED_ANSWER).is_none());
    assert_eq!(MAX_TURNS, 200);
    let bank = ss_domain::communications::build_script(&[]);
    assert_eq!(simulated_script(&[], bank.clone()), bank);
    let request: Vec<String> = [
        OWN,
        " ",
        "  what is the USABLE square footage of the suite? ",
        OWN,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let script = simulated_script(&request, bank.clone());
    let texts: Vec<&str> = script.iter().map(|q| q.question.as_str()).collect();
    // The bank of a property without open assessments is the availability question alone; the
    // area question is a bank question all the same and keeps its claim type.
    assert_eq!(
        texts,
        [
            OWN,
            BANK_AREA,
            "When would the space be available to start a lease?"
        ]
    );
    assert_eq!(script[0].claim_type, "outreach_question_0");
    assert!(is_request_question(&script[0]));
    assert_eq!(script[1].claim_type, "usable_area_sqft");
    assert!(!is_request_question(&script[1]));
}

// Items 1, 2, 3, 5: the transcript before (no questions) and after (one bank question, one not).
both_backends!(own_questions_first_then_the_bank, |db| {
    let s = setup(db, None).await;
    let before = s.started(&[]).await;
    let bank: Vec<String> = before.script.iter().map(|q| q.question.clone()).collect();
    assert!(!before.script.iter().any(is_request_question));
    let before = s.simulate(&before, json!({"owner_action": "later"})).await;
    show("before: no questions of its own", &before);
    let before_summary = s.summary_text().await;
    assert!(
        !before_summary.contains("your own questions"),
        "{before_summary}"
    );
    assert!(!lines(&before)
        .iter()
        .any(|l| l.contains(NO_SCRIPTED_ANSWER)));

    let s = setup(db, None).await;
    let call = s.started(&[OWN, BANK_AREA]).await;
    let script: Vec<&str> = call.script.iter().map(|q| q.question.as_str()).collect();
    let mut expected = vec![OWN, BANK_AREA];
    expected.extend(bank.iter().map(String::as_str).filter(|q| *q != BANK_AREA));
    assert_eq!(
        script, expected,
        "the request's own first, then the bank's, no duplicate"
    );
    assert_eq!(call.script[0].claim_type, "outreach_question_0");
    assert_eq!(call.script[1].claim_type, "usable_area_sqft");

    // A caller's answer for the request's own question is not used: the fixed line is said.
    let call = s
        .simulate(
            &call,
            json!({"owner_action": "later",
                   "answers": {"outreach_question_0": "Yes, two docks and 400 amps."}}),
        )
        .await;
    show("after: two questions of its own", &call);
    assert_eq!(call.state, CallState::Completed);
    assert!(call.simulated);
    let asked_own = at(&call, &format!("assistant: {OWN}"));
    assert_eq!(
        lines(&call)[asked_own + 1],
        format!("counterparty: {NO_SCRIPTED_ANSWER}")
    );
    let asked_area = at(&call, &format!("assistant: {BANK_AREA}"));
    assert!(asked_own < asked_area);
    assert!(asked_area < at(&call, &format!("counterparty: {AREA_ANSWER}")));
    assert!(!lines(&call).iter().any(|l| l.contains("two docks")));

    // The question is recorded as left unanswered; the bank question as before.
    let own = &call.findings[0];
    assert_eq!(own.claim_type.as_deref(), Some("outreach_question_0"));
    assert_eq!(own.question, OWN);
    assert_eq!(own.answer.as_deref(), Some(NO_SCRIPTED_ANSWER));
    assert!(own.value.is_null() && own.unit.is_none(), "{own:?}");
    let area = call
        .findings
        .iter()
        .find(|f| f.claim_type.as_deref() == Some("usable_area_sqft"))
        .unwrap();
    assert_eq!(area.value, json!(1250));
    assert_eq!(call.findings.len(), call.script.len());

    // Nothing from the unanswered turn is evidence; what is stored carries the simulated label.
    let types = s.evidence_types(&call).await;
    assert!(types.contains(&"usable_area_sqft".to_owned()), "{types:?}");
    assert!(
        !types.iter().any(|t| t.starts_with("outreach_question")),
        "{types:?}"
    );

    let text = s.summary_text().await;
    assert!(text.starts_with(SIMULATED_LABEL), "{text}");
    assert!(
        text.contains(&format!(
            "Of your own questions, the simulated contact has no scripted answer for: \"{OWN}\"."
        )),
        "{text}"
    );
    assert!(!text.contains("Not asked"), "{text}");
    assert!(!text.contains("outreach_question"), "{text}");
});

// Item 2 at the server: whatever a simulated contact says to a question the script does not
// know, no value is kept and nothing becomes evidence.
both_backends!(an_own_question_never_becomes_evidence, |db| {
    let s = setup(db, None).await;
    let call = s.started(&[OWN]).await;
    let mut tx = s.t.app.store.begin().await.unwrap();
    let mut sim = CallSimulator::new(&s.t.app).unwrap();
    let mut status = Map::new();
    status.insert("status".into(), Value::from("completed"));
    let call = sim
        .leg_events(
            &mut *tx,
            call,
            vec![("call.answered", Map::new()), ("call.speak.ended", status)],
        )
        .await
        .unwrap();
    assert_eq!(call.transcript.last().unwrap().text, OWN);
    let call = sim
        .say_broker(
            &mut *tx,
            call,
            "Yes, there is a dock, and the panel is 400 amps.",
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(call.question_index, 1);
    assert!(call.findings[0].value.is_null(), "{:?}", call.findings[0]);
    assert!(s.evidence_types(&call).await.is_empty());
});

// Item 1, the bound: past it questions are not asked, and the summary names the request's own.
both_backends!(the_summary_names_what_was_not_asked, |db| {
    // The bound on broker turns (200 in `run`; 2 here): one answer and the escalation line.
    let s = setup(db, None).await;
    let call = s.started(&[OWN, OWN_2, BANK_AREA]).await;
    let mut tx = s.t.app.store.begin().await.unwrap();
    let mut sim = CallSimulator::new(&s.t.app).unwrap();
    let call = sim
        .run_bounded(&mut *tx, call.call_id, "later", &IndexMap::new(), 1, 2)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let notices = sim.orch.take_notices();
    s.t.app.owner_notify().dispatch(s.t.app.clone(), notices);
    show("stopped at the bound", &call);
    let text = s.summary_text().await;
    assert!(
        text.contains(&format!("no scripted answer for: \"{OWN}\"."))
            && text.contains(&format!(
                "Asked, with no answer before the call ended: \"{OWN_2}\"."
            ))
            && text.contains(&format!(
                "Not asked before the call ended: \"{BANK_AREA}\"."
            )),
        "{text}"
    );
    let mut tx = s.t.app.store.begin().await.unwrap();
    let stored = CALLS.require(&mut *tx, call.call_id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stored.outcome_summary, call.outcome_summary);
    assert_eq!(
        call.outcome_summary.as_deref(),
        text.strip_prefix(SIMULATED_LABEL)
    );
    // A bank question the request did not ask is not listed.
    assert!(!text.contains("available to start a lease"), "{text}");
});

// With the voice bridge on the request's questions are planned at the bridge, as before: the
// stored script stays the bank's.
both_backends!(a_bridged_simulated_call_keeps_the_bank_script, |db| {
    let s = setup(db, Some(FixtureBridge::new())).await;
    let call = s.started(&[OWN, BANK_AREA]).await;
    assert!(!call.script.iter().any(is_request_question));
    assert!(call.script.len() <= 4);
    let claims: Vec<&str> = DEFAULT_ANSWERS.iter().map(|(k, _)| *k).collect();
    assert!(call
        .script
        .iter()
        .all(|q| claims.contains(&q.claim_type.as_str())));
});
