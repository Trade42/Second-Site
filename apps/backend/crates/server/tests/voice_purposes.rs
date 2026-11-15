//! voice-glue-1: a simulated call that carries what the owner approved (its own question, a
//! tour, rent limits), with the model's tools answered by the server's rules, the owner's
//! notices and the summary. The fixture bridge stands in for the live one; nothing reaches a
//! carrier or xAI, and nobody is called.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde_json::{json, Map, Value};
use ss_domain::communications::call_purposes as cp;
use ss_domain::communications::response_agent::CONCESSION_LINE;
use ss_domain::contracts::{CallSession, CallState, HandoffStatus};
use ss_server::services::call_orchestration::{CallOrchestrator, StartCall, REQUEST_DIGEST_KEY};
use ss_server::services::call_simulator::{CallSimulator, FixtureBridge};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::services::voice_bridge::{BridgeEvents, Speaker, ToolCall, TranscriptLine};
use ss_server::services::voice_session_config as vsc;
use ss_server::store::comms::CALLS;
use uuid::Uuid;

use common::{demo_project_body, TestApp, TestDb};

/// The owner's maximum in the synthetic request ($4,517.00): it must appear nowhere.
const MAX_CENTS: i64 = 451_700;
const MAX_FORMS: [&str; 4] = ["451700", "4,517", "$4517", "4517.0"];

struct P {
    t: TestApp,
    fake: Arc<RecordingNotifier>,
    bridge: FixtureBridge,
    token: String,
    owner: Uuid,
}

async fn setup(db: &TestDb, bridge_on: bool) -> P {
    let t = TestApp::new(db, &[]);
    let fake = Arc::new(RecordingNotifier::new());
    t.app
        .extensions
        .insert(Arc::new(OwnerNotify::new(fake.clone())));
    let bridge = FixtureBridge::new();
    let vc = t.app.voice_calls();
    if bridge_on {
        vc.set_launcher(Some(Arc::new(bridge.clone())));
    }
    vc.bind(&t.app);
    let (token, owner) = t.dev_token().await;
    P {
        t,
        fake,
        bridge,
        token,
        owner,
    }
}

impl P {
    async fn post(&self, path: &str, body: &Value) -> common::Resp {
        self.t.post(path, Some(&self.token), body).await
    }
    async fn get(&self, path: &str) -> common::Resp {
        self.t.get(path, Some(&self.token)).await
    }
    /// The demo project (its profile's monthly rent limit is $7,500) and one of its properties.
    async fn project(&self) -> (Uuid, String) {
        let p = self
            .t
            .create_project(&self.token, &demo_project_body())
            .await;
        let pid: Uuid = p["project"]["project_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let r = self
            .post(
                &format!("/api/v1/projects/{pid}/properties/search"),
                &json!({"city": "Los Angeles", "state": "CA"}),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
        let props = self
            .get(&format!("/api/v1/projects/{pid}/properties"))
            .await;
        let prop = props.body.as_array().unwrap()[0]["property_id"]
            .as_str()
            .unwrap()
            .to_owned();
        (pid, prop)
    }
    async fn approve(&self, appr: &Value) {
        let r = self
            .post(
                &format!(
                    "/api/v1/approvals/{}/decision",
                    appr["approval_id"].as_str().unwrap()
                ),
                &json!({"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]}),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    }
    async fn start(&self, pid: Uuid, out: &Value) -> common::Resp {
        self.post(
            &format!("/api/v1/projects/{pid}/calls"),
            &json!({"communication_id": out["communication"]["communication_id"]}),
        )
        .await
    }
    async fn call(&self, id: Uuid) -> CallSession {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let c = CALLS.require(&mut *tx, id).await.unwrap();
        tx.commit().await.unwrap();
        c
    }
    async fn events(
        &self,
        call: &CallSession,
        evs: Vec<(&str, Map<String, Value>)>,
    ) -> CallSession {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let mut sim = CallSimulator::new(&self.t.app).unwrap();
        let c = sim.leg_events(&mut *tx, call.clone(), evs).await.unwrap();
        tx.commit().await.unwrap();
        let notices = sim.orch.take_notices();
        self.t
            .app
            .owner_notify()
            .dispatch(self.t.app.clone(), notices);
        c
    }
    async fn texts(&self) -> Vec<String> {
        self.t.app.owner_notify().idle().await;
        self.fake.requests().into_iter().map(|r| r.text).collect()
    }
}

/// A tour window three days from now, 3 to 5 PM at -08:00 (the day as `YYYY-MM-DD`).
fn tour_day() -> String {
    (Utc::now() + Duration::days(3))
        .format("%Y-%m-%d")
        .to_string()
}

/// The synthetic request body that starts a call with a tour and rent limits.
fn request(prop: &str, max_cents: i64) -> Value {
    let day = tour_day();
    json!({
        "property_id": prop,
        "channel": "VOICE",
        "recipient": {"role": "broker", "name": "Dana Broker", "phone": "+15555550100"},
        "purpose": "Verify the space, set up a tour and discuss the rent",
        "questions": ["Is there a loading dock?"],
        "purposes": {
            "tour": {
                "windows": [{"start": format!("{day}T15:00:00-08:00"), "end": format!("{day}T17:00:00-08:00")}],
                "duration_minutes": 30
            },
            "negotiation": {
                "target_monthly_rent_cents": 400_000,
                "max_monthly_rent_cents": max_cents,
                "max_counters": 3
            }
        }
    })
}

fn line(text: &str, turn_id: u64) -> TranscriptLine {
    TranscriptLine {
        speaker: Speaker::Counterparty,
        text: text.into(),
        turn_id,
        at: Utc::now(),
    }
}

async fn tool(ev: &Arc<dyn BridgeEvents>, name: &str, arguments: Value, turn_id: u64) -> Value {
    let out = ev
        .tool_call(ToolCall {
            name: name.into(),
            arguments,
            turn_id,
        })
        .await;
    serde_json::from_str(&out).unwrap_or_else(|_| panic!("tool output is JSON: {out}"))
}

fn no_max(what: &str, text: &str) {
    for f in MAX_FORMS {
        assert!(!text.contains(f), "{what} holds the maximum ({f}): {text}");
    }
    assert!(!text.contains("max_monthly_rent"), "{what}: {text}");
}

both_backends!(a_simulated_call_carries_what_was_approved, |db| {
    let p = setup(db, true).await;
    let (pid, prop) = p.project().await;
    let path = format!("/api/v1/projects/{pid}/outreach");

    // A body that fails validation does not echo the limits.
    let mut bad = request(&prop, MAX_CENTS);
    bad.as_object_mut().unwrap().remove("purpose");
    let r = p.post(&path, &bad).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
    no_max("the 422 body", &r.text);
    let mut bad = request(&prop, MAX_CENTS);
    bad["purposes"]["negotiation"]["max_counters"] = json!("three");
    let r = p.post(&path, &bad).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
    no_max("the 422 body", &r.text);

    // The request: the answer holds no purposes; the approval is bound to them by a digest.
    let r = p.post(&path, &request(&prop, MAX_CENTS)).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    no_max("the outreach answer", &r.text);
    assert!(!r.text.contains("purposes"), "{}", r.text);
    let out = r.body.clone();
    let digest = out["approval"]["action_payload"][REQUEST_DIGEST_KEY]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(digest.len(), 64);
    let summary = out["approval"]["summary"].as_str().unwrap_or_default();
    assert!(
        r.text.contains("may set up a tour inside your times")
            && r.text.contains("may discuss the rent inside your limits"),
        "{summary} / {}",
        r.text
    );
    // The same recipient and property with other limits is another approval, not this one.
    let other = p.post(&path, &request(&prop, MAX_CENTS + 10_000)).await;
    assert_eq!(other.status, StatusCode::ACCEPTED, "{}", other.text);
    assert_ne!(
        other.body["approval"]["approval_id"],
        out["approval"]["approval_id"]
    );
    assert_ne!(
        other.body["approval"]["action_payload"][REQUEST_DIGEST_KEY],
        json!(digest)
    );
    p.approve(&out["approval"]).await;
    // The other request cannot ride on this approval: its own is still PENDING.
    let r = p.start(pid, &other.body).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.text);
    // Nothing can be supplied again at start.
    let r = p
        .post(
            &format!("/api/v1/projects/{pid}/calls"),
            &json!({"communication_id": out["communication"]["communication_id"],
                    "purposes": {"negotiation": {"target_monthly_rent_cents": 1, "max_monthly_rent_cents": 999_900}}}),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
    // A start that does not come from the stored request (here: no communication, so no
    // stored purposes) does not match the approved payload.
    {
        let appr: Uuid = out["approval"]["approval_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let recipient: indexmap::IndexMap<String, String> =
            serde_json::from_value(request(&prop, 0)["recipient"].clone()).unwrap();
        let mut tx = p.t.app.store.begin().await.unwrap();
        let err = CallOrchestrator::new(&p.t.app)
            .start(
                &mut *tx,
                StartCall {
                    project_id: pid,
                    owner_id: p.owner,
                    recipient,
                    property_id: Some(prop.parse().unwrap()),
                    approval_id: Some(appr),
                    intro_recording_reference: None,
                    communication_id: None,
                    simulated: true,
                },
            )
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("APPROVAL_PAYLOAD_MISMATCH"),
            "{err:?}"
        );
        drop(tx);
    }

    let r = p.start(pid, &out).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    no_max("the start answer", &r.text);
    assert_eq!(r.body["simulated"], true);
    let call_id: Uuid = r.body["call_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(p.t.app.voice_calls().pending_requests(), 1, "the other one");
    assert!(p.t.app.voice_calls().has_purposes_state(call_id));
    let call = p.call(call_id).await;
    let call = p.events(&call, vec![("call.answered", Map::new())]).await;
    assert_eq!(call.state, CallState::Active);

    // The session: the request's own question first; the purposes' section before the rules;
    // the four tools; no limit and no window anywhere.
    let (_, spec) = p.bridge.specs()[0].clone();
    let ins = spec.session.instructions.clone();
    assert!(ins.contains("\n1. Is there a loading dock?\n"), "{ins}");
    let (what, rules) = (
        ins.find("## What else this call is for").unwrap(),
        ins.find("## Rules").unwrap(),
    );
    assert!(what < rules);
    assert!(ins.contains(vsc::TOUR_RULE_EXCEPTION) && ins.contains(vsc::RENT_RULE_EXCEPTION));
    let names: Vec<_> = spec.session.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            vsc::TOOL_ESCALATE,
            vsc::TOOL_APPROVAL,
            cp::TOOL_CHECK_TOUR,
            cp::TOOL_EVALUATE_RENT
        ]
    );
    assert!(spec.session.tools[1]
        .description
        .ends_with(vsc::RENT_TOOL_EXCEPTION));
    let session = serde_json::to_string(&spec.session.session_update()).unwrap();
    no_max("the session", &session);
    assert!(
        !session.contains("4,000") && !session.contains("7,500"),
        "{session}"
    );
    assert!(!session.contains(&tour_day()), "{session}");
    let ev = spec.events.clone();
    ev.transcript(line("Yes, there is a loading dock at the back.", 1))
        .await;

    // A tour proposed outside the windows, then inside.
    let day = tour_day();
    let t1 = tool(
        &ev,
        cp::TOOL_CHECK_TOUR,
        json!({"start": format!("{day}T09:00:00")}),
        2,
    )
    .await;
    assert_eq!(t1["decision"], "offer_times", "{t1}");
    assert!(!p.texts().await.iter().any(|t| t.contains("Tour set")));
    let t2 = tool(
        &ev,
        cp::TOOL_CHECK_TOUR,
        json!({"start": format!("{day}T15:30:00")}),
        3,
    )
    .await;
    assert_eq!(t2["decision"], "book", "{t2}");
    assert!(t2["say"].as_str().unwrap().contains("3:30 PM"), "{t2}");
    let texts = p.texts().await;
    let tour = texts
        .iter()
        .find(|t| t.contains("Tour set on the call"))
        .expect("the tour notice");
    assert!(
        tour.starts_with("[Simulated call] ") && tour.contains("3:30 PM"),
        "{tour}"
    );
    assert!(
        tour.contains("nothing was sent to them in writing"),
        "{tour}"
    );

    // A rent above the maximum is countered. The transcript line arrives BEFORE the tool call:
    // the Response Agent's concession line is not spoken, and nothing is deferred.
    ev.transcript(line("Would the owner go to $4,600 a month?", 4))
        .await;
    let r1 = tool(
        &ev,
        cp::TOOL_EVALUATE_RENT,
        json!({"monthly_rent_dollars": 4600, "said_by": "counterparty"}),
        5,
    )
    .await;
    assert_eq!(r1["decision"], "counter", "{r1}");
    assert!(r1["say"].as_str().unwrap().contains("$4,000"), "{r1}");
    // One inside the limit goes to the owner. Here the tool call arrives BEFORE the line.
    let r2 = tool(
        &ev,
        cp::TOOL_EVALUATE_RENT,
        json!({"monthly_rent_dollars": 4450, "said_by": "counterparty"}),
        6,
    )
    .await;
    assert_eq!(r2["decision"], "take_to_owner", "{r2}");
    ev.transcript(line("Would the owner take that?", 7)).await;
    let log = p.bridge.log();
    assert!(
        !log.iter().any(|l| l.contains(CONCESSION_LINE)),
        "the concession line was spoken over the tool: {log:?}"
    );
    let c = p.call(call_id).await;
    assert_eq!(c.state, CallState::Active);
    assert!(
        !c.deferred_questions
            .iter()
            .any(|q| q.contains("Would the owner")),
        "{:?}",
        c.deferred_questions
    );
    let texts = p.texts().await;
    let rent = texts
        .iter()
        .find(|t| t.contains("Needs your decision"))
        .expect("the rent notice");
    assert!(
        rent.starts_with("[Simulated call] ")
            && rent.contains("Dana Broker (broker) said $4,450 a month")
            && rent.contains("Nothing is agreed"),
        "{rent}"
    );
    // Unreadable arguments: the module's own answer, nothing invented, nothing stored.
    let bad = tool(
        &ev,
        cp::TOOL_EVALUATE_RENT,
        json!({"monthly_rent_dollars": "lots", "said_by": "counterparty"}),
        8,
    )
    .await;
    assert_eq!(bad["decision"], "ask_again", "{bad}");
    assert!(bad["say"].is_null());

    // "Do we have a deal?" still holds the call for the owner, limits or not.
    ev.transcript(line("So do we have a deal at $4,450?", 9))
        .await;
    let c = p.call(call_id).await;
    assert_eq!(c.state, CallState::HandoffPending);
    // While the owner is asked, the tools decide nothing.
    let held = tool(
        &ev,
        cp::TOOL_EVALUATE_RENT,
        json!({"monthly_rent_dollars": 4400, "said_by": "counterparty"}),
        10,
    )
    .await;
    assert_eq!(held["decision"], "not_approved", "{held}");
    let mut tx = p.t.app.store.begin().await.unwrap();
    CallOrchestrator::new(&p.t.app)
        .owner_response(
            &mut *tx,
            c.handoff_request_id.unwrap(),
            p.owner,
            HandoffStatus::Deferred,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let c = p.call(call_id).await;
    assert_eq!(c.state, CallState::AiResumed);

    // The end: the state with the limits is dropped; a late tool call gets the refusal.
    let mut h = Map::new();
    h.insert("hangup_cause".into(), json!("normal_clearing"));
    let c = p.events(&c, vec![("call.hangup", h)]).await;
    assert_eq!(c.state, CallState::Completed);
    assert!(!p.t.app.voice_calls().has_purposes_state(call_id));
    let late = tool(
        &ev,
        cp::TOOL_CHECK_TOUR,
        json!({"start": format!("{day}T16:00:00")}),
        11,
    )
    .await;
    assert_eq!(late["decision"], "not_approved", "{late}");

    // The summary: labelled, the purposes' lines in it.
    let sum = c.outcome_summary.clone().unwrap();
    println!("SUMMARY {sum}");
    assert!(sum.starts_with("[Simulated call] "), "{sum}");
    assert!(sum.contains("Tour set by the assistant for"), "{sum}");
    assert!(
        sum.contains("inside your limit. Nothing is agreed. Your decision."),
        "{sum}"
    );
    assert!(sum.chars().count() <= 640, "{}", sum.chars().count());
    let texts = p.texts().await;
    for t in &texts {
        println!("NOTICE {t}");
    }
    assert!(texts.contains(&sum), "the summary notice");

    // Nowhere: the maximum, or the purposes themselves.
    no_max("the call", &serde_json::to_string(&c).unwrap());
    for t in &texts {
        no_max("a notice", t);
    }
    for path in ["calls", "communications", "handoffs", "events?limit=500"] {
        let r = p.get(&format!("/api/v1/projects/{pid}/{path}")).await;
        assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.text);
        no_max(path, &r.text);
    }
});

both_backends!(a_refused_negotiation_does_not_stop_the_call, |db| {
    // A maximum above the profile's limit ($7,500): the call runs with the tour only, and
    // the owner is told why, without the refused number.
    let p = setup(db, true).await;
    let (pid, prop) = p.project().await;
    let r = p
        .post(
            &format!("/api/v1/projects/{pid}/outreach"),
            &request(&prop, 912_300),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    p.approve(&r.body["approval"]).await;
    let s = p.start(pid, &r.body).await;
    assert_eq!(s.status, StatusCode::ACCEPTED, "{}", s.text);
    let call_id: Uuid = s.body["call_id"].as_str().unwrap().parse().unwrap();
    let texts = p.texts().await;
    let why = texts
        .iter()
        .find(|t| t.contains("will not negotiate the rent"))
        .expect("the refusal notice");
    println!("NOTICE {why}");
    assert!(
        why.starts_with("[Simulated call] ") && why.ends_with("The call goes ahead without it."),
        "{why}"
    );
    assert!(!why.contains("9,123") && !why.contains("912300"), "{why}");
    let call = p.call(call_id).await;
    let call = p.events(&call, vec![("call.answered", Map::new())]).await;
    assert_eq!(call.state, CallState::Active);
    let (_, spec) = p.bridge.specs()[0].clone();
    let names: Vec<_> = spec.session.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec![vsc::TOOL_ESCALATE, vsc::TOOL_APPROVAL, cp::TOOL_CHECK_TOUR]
    );
    assert!(!spec.session.instructions.contains(vsc::RENT_RULE_EXCEPTION));
    // The rent tool was not approved for this call: the module's refusal.
    let out = tool(
        &spec.events,
        cp::TOOL_EVALUATE_RENT,
        json!({"monthly_rent_dollars": 4000, "said_by": "counterparty"}),
        1,
    )
    .await;
    assert_eq!(out["decision"], "not_approved", "{out}");
});

both_backends!(nothing_approved_is_the_session_of_before, |db| {
    // No questions, no purposes: exactly the two tools and the unamended instructions.
    let p = setup(db, true).await;
    let (pid, prop) = p.project().await;
    let mut body = request(&prop, MAX_CENTS);
    body.as_object_mut().unwrap().remove("purposes");
    body.as_object_mut().unwrap().remove("questions");
    let r = p
        .post(&format!("/api/v1/projects/{pid}/outreach"), &body)
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert!(r.body["approval"]["action_payload"]
        .get(REQUEST_DIGEST_KEY)
        .is_none());
    p.approve(&r.body["approval"]).await;
    let s = p.start(pid, &r.body).await;
    assert_eq!(s.status, StatusCode::ACCEPTED, "{}", s.text);
    let call_id: Uuid = s.body["call_id"].as_str().unwrap().parse().unwrap();
    assert!(!p.t.app.voice_calls().has_purposes_state(call_id));
    let call = p.call(call_id).await;
    p.events(&call, vec![("call.answered", Map::new())]).await;
    let (_, spec) = p.bridge.specs()[0].clone();
    assert_eq!(spec.session.tools, vsc::tools());
    let ins = &spec.session.instructions;
    assert!(!ins.contains("What else this call is for"));
    assert!(
        ins.ends_with(
            "\n## Rules\n\
             - You are an AI assistant. Never claim to be a person, the owner or a recording.\n\
             - Never commit the owner to anything and never state agreement to any term, price, \
             date, hold or payment.\n\
             - Never give out personal, financial or account data.\n\
             - Before any commitment or sensitive disclosure, call request_owner_approval and wait \
             for its answer. Before anything outside your authority, call escalate_to_owner and wait.\n\
             - Never say the owner has joined, is joining or is listening. The system says that.\n\
             - When you do not know, say so and offer to ask the owner.\n\
             - Keep answers short. Do not invent facts about the property or the client.\n"
        ),
        "{ins}"
    );
    assert_eq!(
        spec.session.tools[1].description,
        "Required before any commitment (agreeing to terms, holding the space, payments, \
         signatures, offers) and before disclosing anything sensitive. Call it and wait; do not \
         commit or disclose unless the answer says approved."
    );
    assert!(p
        .texts()
        .await
        .iter()
        .all(|t| !t.contains("About the call")));
});

both_backends!(
    with_the_bridge_off_the_owner_is_told_the_purposes_are_not_used,
    |db| {
        let p = setup(db, false).await;
        let (pid, prop) = p.project().await;
        let r = p
            .post(
                &format!("/api/v1/projects/{pid}/outreach"),
                &request(&prop, MAX_CENTS),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
        p.approve(&r.body["approval"]).await;
        let s = p.start(pid, &r.body).await;
        assert_eq!(s.status, StatusCode::ACCEPTED, "{}", s.text);
        let texts = p.texts().await;
        let t = texts
            .iter()
            .find(|t| t.contains("the voice assistant is switched off"))
            .expect("the notice");
        println!("NOTICE {t}");
        assert!(t.starts_with("[Simulated call] "), "{t}");
        no_max("the notice", t);
    }
);

// ---- voice-glue-1-review: the reviewer's own tests -------------------------------------------

/// Sentinel limits that occur nowhere else: a target of $3,881.00 and a maximum of $4,729.00.
const R_TARGET: i64 = 388_100;
const R_MAX: i64 = 472_900;

/// Whether `text` holds the amount as a number: `4,729`, or `4729` / `472900` standing alone
/// (not inside an id or a timestamp, whose hex digits and dashes would match by chance).
fn holds(text: &str, cents: i64) -> bool {
    let d = cents / 100;
    let part = |c: char| c.is_ascii_alphanumeric() || c == '-';
    let alone = |needle: &str| {
        text.match_indices(needle).any(|(i, m)| {
            !text[..i].chars().next_back().is_some_and(part)
                && !text[i + m.len()..].chars().next().is_some_and(part)
        })
    };
    text.contains(&format!("{},{:03}", d / 1000, d % 1000))
        || alone(&d.to_string())
        || alone(&cents.to_string())
}

async fn started(p: &P, target: i64, max: i64) -> (Uuid, Uuid, Value, Arc<dyn BridgeEvents>) {
    let (pid, prop) = p.project().await;
    let mut body = request(&prop, max);
    body["purposes"]["negotiation"]["target_monthly_rent_cents"] = json!(target);
    let r = p
        .post(&format!("/api/v1/projects/{pid}/outreach"), &body)
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert!(
        !holds(&r.text, max) && !holds(&r.text, target),
        "{}",
        r.text
    );
    p.approve(&r.body["approval"]).await;
    let s = p.start(pid, &r.body).await;
    assert_eq!(s.status, StatusCode::ACCEPTED, "{}", s.text);
    assert!(
        !holds(&s.text, max) && !holds(&s.text, target),
        "{}",
        s.text
    );
    let call_id: Uuid = s.body["call_id"].as_str().unwrap().parse().unwrap();
    let call = p.call(call_id).await;
    let call = p.events(&call, vec![("call.answered", Map::new())]).await;
    assert_eq!(call.state, CallState::Active);
    let (_, spec) = p.bridge.specs()[0].clone();
    let session = serde_json::to_string(&spec.session.session_update()).unwrap();
    assert!(
        !holds(&session, max) && !holds(&session, target),
        "{session}"
    );
    (pid, call_id, r.body.clone(), spec.events.clone())
}

fn rent(dollars: i64) -> Value {
    json!({"monthly_rent_dollars": dollars, "said_by": "counterparty"})
}

// The same tool call delivered twice in one turn (a duplicated call id, or the model's two
// identical calls in one response) is answered once: the second gets the first's answer and
// the negotiation does not move a step by itself.
both_backends!(review_a_repeated_tool_call_advances_once, |db| {
    let p = setup(db, true).await;
    let (_pid, _call_id, _out, ev) = started(&p, R_TARGET, R_MAX).await;
    let a = tool(&ev, cp::TOOL_EVALUATE_RENT, rent(5000), 3).await;
    let b = tool(&ev, cp::TOOL_EVALUATE_RENT, rent(5000), 3).await;
    assert_eq!(a["decision"], "counter", "{a}");
    assert_eq!(a, b, "a repeated call moved the negotiation");
    // A later turn with the same number is a new ask: the next step.
    let c = tool(&ev, cp::TOOL_EVALUATE_RENT, rent(5000), 4).await;
    assert_ne!(a, c, "{c}");
    // Two different tools in one turn are each answered.
    let day = tour_day();
    let t = tool(
        &ev,
        cp::TOOL_CHECK_TOUR,
        json!({"start": format!("{day}T15:30:00")}),
        4,
    )
    .await;
    assert_eq!(t["decision"], "book", "{t}");
    let t2 = tool(
        &ev,
        cp::TOOL_CHECK_TOUR,
        json!({"start": format!("{day}T15:30:00")}),
        4,
    )
    .await;
    assert_eq!(t, t2);
    assert_eq!(
        p.texts()
            .await
            .iter()
            .filter(|t| t.contains("Tour set on the call"))
            .count(),
        1
    );
});

// Where the limits can be read. Before the last counter the maximum is nowhere; the target is
// only in our own counter. The schedule's last counter is the maximum itself (the purposes
// module's design): from then on it is in what the assistant says and in the owner's records.
both_backends!(review_where_the_limits_can_be_read, |db| {
    let p = setup(db, true).await;
    let (pid, call_id, out, ev) = started(&p, R_TARGET, R_MAX).await;
    let mut outputs = Vec::new();
    ev.transcript(line("We are at $5,000 a month.", 1)).await;
    outputs.push(tool(&ev, cp::TOOL_EVALUATE_RENT, rent(5000), 1).await);
    ev.transcript(line("I could do $4,950.", 2)).await;
    outputs.push(tool(&ev, cp::TOOL_EVALUATE_RENT, rent(4950), 2).await);
    let paths = [
        "calls",
        "communications",
        "handoffs",
        "approvals",
        "events?limit=500",
    ];
    let mut bodies = Vec::new();
    for path in &paths {
        let r = p.get(&format!("/api/v1/projects/{pid}/{path}")).await;
        assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.text);
        bodies.push((path.to_string(), r.text));
    }
    bodies.push(("approval".into(), out["approval"].to_string()));
    for t in p.texts().await {
        bodies.push(("notice".into(), t));
    }
    for o in &outputs {
        bodies.push(("tool output".into(), o.to_string()));
    }
    bodies.push(("bridge log".into(), p.bridge.log().join("\n")));
    for (what, text) in &bodies {
        assert!(!holds(text, R_MAX), "{what} holds the maximum: {text}");
        if holds(text, R_TARGET) {
            println!("TARGET in {what}");
        }
    }
    // The third counter.
    ev.transcript(line("$4,900 is my last word.", 3)).await;
    let third = tool(&ev, cp::TOOL_EVALUATE_RENT, rent(4900), 3).await;
    println!("THIRD {third}");
    println!(
        "THIRD holds the maximum: {}",
        holds(&third.to_string(), R_MAX)
    );
    let c = p.call(call_id).await;
    let mut h = Map::new();
    h.insert("hangup_cause".into(), json!("normal_clearing"));
    let c = p.events(&c, vec![("call.hangup", h)]).await;
    println!("SUMMARY {}", c.outcome_summary.clone().unwrap_or_default());
    for path in &paths {
        let r = p.get(&format!("/api/v1/projects/{pid}/{path}")).await;
        println!("AFTER max in {path}: {}", holds(&r.text, R_MAX));
    }
    for t in p.texts().await {
        println!("AFTER max in notice: {} :: {t}", holds(&t, R_MAX));
    }
});

// ---- voice-instruct-1: the Response Agent's silent directions reach the bridge ----------------

use ss_domain::communications::response_agent::{self as agent, CallContext, OwnerRequirement};
use ss_server::services::call_orchestration::{
    direction_text, Direction, DIRECTION_REFUSED_NOTE, DIRECTION_WITHHELD_NOTE,
};
use ss_server::services::voice_bridge::MAX_INSTRUCTION_CHARS;

const OPENING_DIRECTION: &str = "After the introduction and disclosure, Then ask exactly: \"";
const ACK_DIRECTION: &str =
    "Briefly acknowledge the answer without agreeing to any amount or term. Then ask exactly: \"";

/// The question the opening direction asks.
fn opening_question(p: &P) -> String {
    let all = p.bridge.instructions();
    assert_eq!(all.len(), 1, "one opening direction: {all:?}");
    let q = all[0]
        .strip_prefix(OPENING_DIRECTION)
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or_else(|| panic!("the opening direction: {}", all[0]));
    q.to_owned()
}

fn notes(call: &CallSession, note: &str) -> usize {
    call.transcript
        .iter()
        .filter(|t| t.speaker == "system" && t.text == note)
        .count()
}

// A counterparty line that makes the Response Agent decide `Instruct`: the bridge gets exactly
// one direction with the agent's text, nothing is spoken for it and nothing is stored as said.
// The owner's maximum and target are in no direction.
both_backends!(an_instruct_decision_reaches_the_bridge_once, |db| {
    let p = setup(db, true).await;
    let (_pid, call_id, _out, ev) = started(&p, R_TARGET, R_MAX).await;
    let q = opening_question(&p);
    let before = p.bridge.log();
    ev.transcript(line("Okay, give me a moment.", 1)).await;
    let log = p.bridge.log();
    let expected = format!("{ACK_DIRECTION}{q}\"");
    println!("DIRECTION {expected}");
    assert_eq!(
        log[before.len()..],
        [format!("instruct:{expected}")],
        "one direction, no speech"
    );
    let call = p.call(call_id).await;
    assert_eq!(call.state, CallState::Active);
    assert!(
        !call
            .transcript
            .iter()
            .any(|t| t.text.contains("Then ask exactly")),
        "a direction is not a spoken line: {:?}",
        call.transcript
    );
    assert_eq!(notes(&call, DIRECTION_REFUSED_NOTE), 0);
    // A rent conversation: the owner's limits stay out of every direction.
    ev.transcript(line("We are at $5,000 a month.", 2)).await;
    tool(&ev, cp::TOOL_EVALUATE_RENT, rent(5000), 2).await;
    ev.transcript(line("Would the owner go to $4,800?", 3))
        .await;
    ev.transcript(line("What rent is the owner looking for?", 4))
        .await;
    ev.transcript(line("Sure, go ahead.", 5)).await;
    let all = p.bridge.instructions();
    assert!(all.len() >= 2, "{all:?}");
    for d in &all {
        assert!(
            !holds(d, R_MAX) && !holds(d, R_TARGET) && !d.contains("7,500"),
            "a limit in a direction: {d}"
        );
        assert!(d.chars().count() <= MAX_INSTRUCTION_CHARS);
    }
});

// A hold decided in the same turn (the model's approval tool call after the line): the
// direction goes first, because the line is decided when it arrives; the pause follows. Once
// the call is held, a further line decides nothing and sends no direction.
both_backends!(a_direction_goes_before_a_hold_of_the_same_turn, |db| {
    let p = setup(db, true).await;
    let (_pid, call_id, _out, ev) = started(&p, R_TARGET, R_MAX).await;
    let q = opening_question(&p);
    let before = p.bridge.log();
    ev.transcript(line("Okay, give me a moment.", 1)).await;
    ev.tool_call(ToolCall {
        name: vsc::TOOL_APPROVAL.into(),
        arguments: json!({"kind": "commitment", "detail": "hold the space until Friday"}),
        turn_id: 1,
    })
    .await;
    let log = p.bridge.log()[before.len()..].to_vec();
    println!("HOLD LOG {log:?}");
    let direction = format!("instruct:{ACK_DIRECTION}{q}\"");
    assert_eq!(log.iter().filter(|l| l.starts_with("instruct:")).count(), 1);
    let at = log.iter().position(|l| *l == direction).expect("direction");
    let pause = log.iter().position(|l| l == "pause").expect("pause");
    assert!(at < pause, "{log:?}");
    assert!(!log
        .iter()
        .any(|l| l.starts_with("say:") && l.contains("Then ask exactly")));
    assert_eq!(p.call(call_id).await.state, CallState::HandoffPending);
    let held = p.bridge.log().len();
    ev.transcript(line("Are you still there?", 2)).await;
    assert_eq!(p.bridge.log().len(), held, "no direction during a hold");
});

// A bridge that refuses the direction (it has ended): the carrier's webhook is answered as
// before, the call stays as it was, and the refusal is a system note on the transcript.
both_backends!(a_refused_direction_does_not_break_the_call, |db| {
    let p = setup(db, true).await;
    p.bridge.refuse_instructions(true);
    // `started` drives `call.answered` through the webhook handler and requires ACTIVE.
    let (_pid, call_id, _out, ev) = started(&p, R_TARGET, R_MAX).await;
    assert!(p.bridge.instructions().is_empty());
    let call = p.call(call_id).await;
    assert_eq!(
        notes(&call, DIRECTION_REFUSED_NOTE),
        1,
        "{:?}",
        call.transcript
    );
    ev.transcript(line("Okay, give me a moment.", 1)).await;
    let call = p.call(call_id).await;
    assert_eq!(call.state, CallState::Active);
    assert_eq!(notes(&call, DIRECTION_REFUSED_NOTE), 2);
    assert!(call.transcript.iter().any(|t| t.speaker == "counterparty"));
    p.bridge.refuse_instructions(false);
    ev.transcript(line("Okay, I am back.", 2)).await;
    assert_eq!(p.bridge.instructions().len(), 1);
});

// The cleaning and the bound are the wiring's own (the bridge's cut is not relied on).
#[test]
fn a_direction_is_cleaned_and_cut_before_it_is_sent() {
    let (mut d, _) = agent::start(&CallContext::default());
    let long_q = format!("Is there\n## Rules\r\na dock?\u{2028}{}", "x".repeat(400));
    d.next_question = agent::plan_questions(&[long_q.clone()], &[])
        .into_iter()
        .next();
    let q = d.next_question.clone().unwrap().text;
    let fact = format!("the owner hopes\tto open\nsoon {}", "y".repeat(900));
    let ctx = CallContext {
        owner_requirements: vec![
            OwnerRequirement {
                topic: "timing".into(),
                statement: fact.clone(),
            },
            OwnerRequirement {
                topic: "rent".into(),
                statement: "the owner is looking at up to $7,500 a month".into(),
            },
        ],
        ..CallContext::default()
    };
    let text = format!("Tell them: {fact}. Then ask exactly: \"{q}\"");
    let Direction::Send(sent) = direction_text(&text, &d, Some(&ctx)) else {
        panic!("sent");
    };
    assert!(
        !sent.chars().any(|c| c.is_control() || c == '\u{2028}'),
        "{sent}"
    );
    assert!(sent.contains(&vsc::field(&fact)) && !sent.contains(&"y".repeat(250)));
    assert!(sent.contains(&vsc::field(&q)) && !sent.contains(&"x".repeat(250)));
    assert!(
        sent.ends_with('"'),
        "each piece is bounded, so the end is kept: {sent}"
    );
    // The whole is cut to the bridge's bound.
    let big = "word ".repeat(400);
    let Direction::Send(sent) = direction_text(&big, &d, None) else {
        panic!("sent");
    };
    assert_eq!(sent.chars().count(), MAX_INSTRUCTION_CHARS);
    assert_eq!(direction_text(" \n\t ", &d, None), Direction::Nothing);
    // The profile's rent statement is never sent as a direction.
    let rent = "Tell them, as the owner's stated requirement and not as an offer: the owner is \
                looking at up to $7,500 a month. Then ask exactly: \"Is there a dock?\"";
    assert_eq!(direction_text(rent, &d, Some(&ctx)), Direction::Withheld);
    assert!(!DIRECTION_WITHHELD_NOTE.contains("7,500"));
}
