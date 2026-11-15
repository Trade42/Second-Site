//! In live mode every call rings one number only, the owner's own test number
//! (`LIVE_CALLS_ONLY_TO`), and nobody else can be dialed (owner, 11:45Z: "Can they just call me
//! only?"). The carrier here is a local counting mock server; no test places a call or sends
//! anything to a provider.

mod common;

use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_domain::contracts::{CallQuestion, CallState, HandoffReason, HandoffStatus};
use ss_domain::integrations::telnyx::{StreamOptions, StreamTrack, TelephonyError};
use ss_server::config::{mask_number, LiveCallTarget};
use ss_server::contracts::{ActionType, ApprovalStatus};
use ss_server::integrations::telnyx::{Voice, ONLY_TO_REFUSAL};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::call_orchestration::{
    is_test_call, CallOrchestrator, StartCall, TEST_CALL_OWNER_LINE, TEST_CALL_PREFIX,
};
use ss_server::store::comms::{CALLS, HANDOFFS};
use ss_server::store::repo::{APPROVALS, EVIDENCE};
use ss_server::store::Query;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer, Recorded};
use common::{demo_project_body, test_settings, TestApp, TestDb};

/// The owner's test number: private, shown only as "number ending in 77".
const OWNER_TEST: &str = "+15555550177";
/// The listing contact's number: never sent to the carrier while a test number is set.
const CONTACT: &str = "+15555550100";
const FROM: &str = "+15555550111";

/// Live voice, the outbound switch on, everything a live call needs except whom it may ring.
const LIVE: [(&str, &str); 7] = [
    ("VOICE_PROVIDER_MODE", "live"),
    ("ALLOW_LIVE_OUTBOUND_CALLS", "true"),
    ("TELNYX_API_KEY", "KEY-secret-value-1"),
    ("TELNYX_CONNECTION_ID", "conn-value-3"),
    ("TELNYX_FROM_NUMBER", FROM),
    ("TELNYX_PUBLIC_KEY", "PUBKEY-value-2"),
    ("PUBLIC_BASE_URL", "https://calls.example.com"),
];

type Extra = Vec<(&'static str, &'static str)>;

fn vars(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    LIVE.iter().copied().chain(extra.iter().copied()).collect()
}

fn carrier() -> MockServer {
    MockServer::start_with(|_req: &Recorded| Canned {
        status: 200,
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&json!({"data": {"call_control_id": "v3:abc", "id": "conf1"}}))
            .unwrap(),
    })
}

/// The `to` of every dial the mock carrier received.
fn dialed(server: &MockServer) -> Vec<String> {
    server
        .requests()
        .iter()
        .filter(|r| r.path.ends_with("/calls"))
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["to"].as_str().unwrap().to_owned()
        })
        .collect()
}

/// No request of any kind carried this text.
fn never_sent(server: &MockServer, text: &str) {
    let digits = text.trim_start_matches('+');
    for r in server.requests() {
        let body = String::from_utf8_lossy(&r.body).into_owned();
        assert!(!body.contains(digits), "sent to the carrier: {body}");
        assert!(!r.path.contains(digits), "in a path: {}", r.path);
    }
}

fn not_printed(text: &str) {
    assert!(
        !text.contains(OWNER_TEST) && !text.contains(OWNER_TEST.trim_start_matches('+')),
        "the test number is printed: {text}"
    );
}

#[test]
fn the_setting_combinations() {
    let target = |extra: &[(&'static str, &'static str)]| test_settings(extra).live_call_target();
    assert_eq!(target(&[]), LiveCallTarget::Nobody);
    assert_eq!(
        target(&[("LIVE_CALLS_ONLY_TO", "  ")]),
        LiveCallTarget::Nobody
    );
    assert_eq!(
        target(&[("LIVE_CALLS_TO_CONTACTS", "true")]),
        LiveCallTarget::Contacts
    );
    let test = LiveCallTarget::TestNumber(OWNER_TEST.into());
    assert_eq!(target(&[("LIVE_CALLS_ONLY_TO", OWNER_TEST)]), test);
    // Together, the test number wins.
    assert_eq!(
        target(&[
            ("LIVE_CALLS_ONLY_TO", OWNER_TEST),
            ("LIVE_CALLS_TO_CONTACTS", "true")
        ]),
        test
    );
    // Set, but not a phone number: nobody, even with the contacts switch on.
    for bad in ["5555550177", "+1555", "+1555abc0177", "+1 555 555 0177"] {
        assert_eq!(
            target(&[
                ("LIVE_CALLS_ONLY_TO", bad),
                ("LIVE_CALLS_TO_CONTACTS", "true")
            ]),
            LiveCallTarget::Nobody,
            "{bad}"
        );
    }
    // The test number applies in live mode only; fixture mode has none.
    assert_eq!(
        test_settings(&[("LIVE_CALLS_ONLY_TO", OWNER_TEST)]).live_test_number(),
        None
    );
    assert_eq!(
        test_settings(&vars(&[("LIVE_CALLS_ONLY_TO", OWNER_TEST)])).live_test_number(),
        Some(OWNER_TEST.to_owned())
    );
    // Masked to the last two digits; never printed by Debug.
    assert_eq!(mask_number(OWNER_TEST), "number ending in 77");
    let s = test_settings(&vars(&[("LIVE_CALLS_ONLY_TO", OWNER_TEST)]));
    not_printed(&format!("{s:?} {:?}", s.live_call_target()));
    // The existing preflight list is what it was (the target is a separate refusal).
    assert!(s.voice_live_missing().is_empty());
}

#[test]
fn every_live_dial_goes_to_the_test_number_or_nowhere() {
    let stream = StreamOptions {
        stream_url: "wss://calls.example.com/api/v1/voice/stream/b?token=t".into(),
        track: StreamTrack::Inbound,
        stream_codec: None,
        bidirectional: None,
        auth_token: None,
    };
    let hook = "https://calls.example.com/webhooks/telnyx";
    // (settings, what both kinds of dial send as `to`; None = refused, nothing sent)
    let cases: [(Extra, Option<&str>); 5] = [
        (vec![("LIVE_CALLS_ONLY_TO", OWNER_TEST)], Some(OWNER_TEST)),
        (
            vec![
                ("LIVE_CALLS_ONLY_TO", OWNER_TEST),
                ("LIVE_CALLS_TO_CONTACTS", "true"),
            ],
            Some(OWNER_TEST),
        ),
        (vec![], None),
        (vec![("LIVE_CALLS_TO_CONTACTS", "true")], Some(CONTACT)),
        (
            vec![
                ("LIVE_CALLS_ONLY_TO", "5555550177"),
                ("LIVE_CALLS_TO_CONTACTS", "true"),
            ],
            None,
        ),
    ];
    for (extra, want) in cases {
        let server = carrier();
        let base = format!("{}/v2", server.base);
        let voice = Voice::from_settings_with_base_url(&test_settings(&vars(&extra)), &base);
        assert_eq!(voice.mode(), "live");
        let results = [
            voice.telephony.dial(CONTACT, FROM, hook, "k1", None),
            voice
                .telephony
                .dial_streaming(CONTACT, FROM, hook, "k2", None, &stream),
        ];
        match want {
            Some(to) => {
                for r in results {
                    assert_eq!(r.unwrap().call_control_id, "v3:abc", "{extra:?}");
                }
                assert_eq!(dialed(&server), vec![to, to], "{extra:?}");
                if to == OWNER_TEST {
                    never_sent(&server, CONTACT);
                }
            }
            None => {
                for r in results {
                    let Err(TelephonyError::Forbidden(m)) = r else {
                        panic!("{extra:?}: not refused: {r:?}");
                    };
                    assert_eq!(m, ONLY_TO_REFUSAL);
                    assert!(m.contains("LIVE_CALLS_ONLY_TO"));
                    not_printed(&m);
                }
                assert!(server.requests().is_empty(), "{extra:?}");
            }
        }
    }

    // The outbound switch off is still the first refusal, and a missing setting the second.
    let server = carrier();
    let base = format!("{}/v2", server.base);
    for (extra, text) in [
        (
            vec![("ALLOW_LIVE_OUTBOUND_CALLS", "false")],
            "ALLOW_LIVE_OUTBOUND_CALLS=false",
        ),
        (
            vec![("TELNYX_PUBLIC_KEY", "")],
            "unusable: TELNYX_PUBLIC_KEY (",
        ),
    ] {
        match Voice::from_settings_with_base_url(&test_settings(&vars(&extra)), &base)
            .telephony
            .dial(CONTACT, FROM, hook, "k", None)
        {
            Err(TelephonyError::Forbidden(m)) => assert!(m.contains(text), "{m}"),
            other => panic!("{other:?}"),
        }
    }
    assert!(server.requests().is_empty());

    // A carrier error that repeats the number dialed: masked before it is stored or shown.
    let failing = MockServer::start_with(|req: &Recorded| Canned {
        status: 422,
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&json!({"errors": [{"detail": format!(
            "invalid destination {}",
            String::from_utf8_lossy(&req.body)
        )}]}))
        .unwrap(),
    });
    let s = test_settings(&vars(&[("LIVE_CALLS_ONLY_TO", OWNER_TEST)]));
    let err = Voice::from_settings_with_base_url(&s, &format!("{}/v2", failing.base))
        .telephony
        .dial(CONTACT, FROM, hook, "k", None)
        .unwrap_err();
    not_printed(&format!("{err:?}"));
    assert_eq!(dialed(&failing), vec![OWNER_TEST]);
}

struct Ctx {
    t: TestApp,
    server: MockServer,
    owner: Uuid,
    token: String,
    pid: Uuid,
    approval: Uuid,
    recipient: IndexMap<String, String>,
    property: Uuid,
}

impl Ctx {
    /// A project with an APPROVED call to the listing contact, and a live voice client built
    /// from the settings against the mock carrier.
    async fn new(db: &TestDb, extra: &[(&'static str, &'static str)]) -> Self {
        let server = carrier();
        let t = TestApp::new(db, &vars(extra));
        let voice =
            Voice::from_settings_with_base_url(&t.app.settings, &format!("{}/v2", server.base));
        assert_eq!(voice.mode(), "live");
        t.app.extensions.insert(Arc::new(voice));
        let owner = Uuid::new_v4();
        let token = t.token_for(owner);
        let p = t.create_project(&token, &demo_project_body()).await;
        let pid: Uuid = p["project"]["project_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        // A stored property (the fixture search), so that evidence about it could be stored.
        let r = t
            .post(
                &format!("/api/v1/projects/{pid}/properties/search"),
                Some(&token),
                &json!({"city": "Los Angeles", "state": "CA"}),
            )
            .await;
        assert!(r.status.is_success(), "{}", r.text);
        let props = t
            .get(&format!("/api/v1/projects/{pid}/properties"), Some(&token))
            .await;
        let property: Uuid = props.body[0]["property_id"]
            .as_str()
            .expect("a property")
            .parse()
            .unwrap();
        let mut recipient = IndexMap::new();
        recipient.insert("phone".to_owned(), CONTACT.to_owned());
        recipient.insert("name".to_owned(), "Dana Broker".to_owned());
        let mut tx = t.app.store.begin().await.unwrap();
        let a = t
            .app
            .approvals()
            .request(
                &mut *tx,
                ApprovalSpec::new(
                    pid,
                    owner,
                    "t",
                    ActionType::PlaceCall,
                    ss_domain::communications::call_payload(pid, &recipient, Some(property)),
                    "call",
                    format!("c-{}", Uuid::new_v4()),
                ),
            )
            .await
            .unwrap();
        t.app
            .approvals()
            .decide(
                &mut *tx,
                a.approval_id,
                owner,
                ApprovalStatus::Approved,
                &a.action_payload_hash,
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        Self {
            t,
            server,
            owner,
            token,
            pid,
            approval: a.approval_id,
            recipient,
            property,
        }
    }

    fn start(&self) -> StartCall {
        StartCall {
            project_id: self.pid,
            owner_id: self.owner,
            recipient: self.recipient.clone(),
            property_id: Some(self.property),
            approval_id: Some(self.approval),
            intro_recording_reference: None,
            communication_id: None,
            simulated: false,
        }
    }

    /// One carrier event for the contact's leg, through the orchestrator.
    async fn event(&self, orch: &mut CallOrchestrator<'_>, event_type: &str) {
        let mut tx = self.t.app.store.begin().await.unwrap();
        orch.handle_event(
            &mut *tx,
            &json!({"data": {"event_type": event_type,
                             "payload": {"call_control_id": "v3:abc"}}}),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    /// Answer the call and let the person on the line answer the one question of its script.
    async fn talk(&self, orch: &mut CallOrchestrator<'_>, call_id: Uuid) {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let mut call = CALLS.require(&mut *tx, call_id).await.unwrap();
        call.script = vec![CallQuestion {
            question: "What is the amperage of the tenant subpanel?".into(),
            claim_type: "tenant_subpanel_amps".into(),
            requirement_id: None,
        }];
        CALLS.upsert(&mut *tx, &call).await.unwrap();
        tx.commit().await.unwrap();
        self.event(orch, "call.answered").await;
        self.event(orch, "call.speak.ended").await;
        for _ in 0..12 {
            let mut tx = self.t.app.store.begin().await.unwrap();
            let call = CALLS.require(&mut *tx, call_id).await.unwrap();
            if call.state != CallState::Active || !call.findings.is_empty() {
                tx.commit().await.unwrap();
                break;
            }
            orch.on_utterance(&mut *tx, call, "The tenant subpanel is 400 amps.")
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    }

    async fn evidence_count(&self) -> usize {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let n = EVIDENCE
            .list(&mut *tx, Query::new().eq("property_id", self.property))
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.provider == "broker_call")
            .count();
        tx.commit().await.unwrap();
        n
    }

    async fn events_text(&self) -> String {
        self.t
            .get(
                &format!("/api/v1/projects/{}/events?limit=500", self.pid),
                Some(&self.token),
            )
            .await
            .text
    }
}

// With the test number set: the contact's approved call rings the owner's test number, the
// contact's number never reaches the carrier, the record says so everywhere, and nothing said on
// the call becomes evidence about the property. An accepted handoff dials nobody.
both_backends!(a_live_call_rings_the_test_number_and_says_so, |db| {
    let c = Ctx::new(
        db,
        &[
            ("LIVE_CALLS_ONLY_TO", OWNER_TEST),
            ("LIVE_CALLS_TO_CONTACTS", "true"),
        ],
    )
    .await;
    let mut orch = CallOrchestrator::new(&c.t.app);
    let mut tx = c.t.app.store.begin().await.unwrap();
    let call = orch.start(&mut *tx, c.start()).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(dialed(&c.server), vec![OWNER_TEST]);
    never_sent(&c.server, CONTACT);
    assert!(is_test_call(&call));
    assert_eq!(call.transcript[0].speaker, "system");
    assert_eq!(
        call.transcript[0].text,
        "TEST CALL: this call was dialed to the owner's test number (number ending in 77) \
         instead of the listing's contact. Nothing was sent to the contact. The person \
         answering is the owner, so nothing said on this call is stored as evidence about the \
         property."
    );
    // The record keeps the approved recipient; the test number is nowhere in it.
    assert_eq!(
        call.recipient.get("phone").map(String::as_str),
        Some(CONTACT)
    );
    not_printed(&serde_json::to_string(&call).unwrap());

    // The conversation: findings stay on the call's events, labelled; no evidence is stored.
    c.talk(&mut orch, call.call_id).await;
    assert_eq!(c.evidence_count().await, 0);
    let mut tx = c.t.app.store.begin().await.unwrap();
    let talked = CALLS.require(&mut *tx, call.call_id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(talked.findings.len(), 1, "the finding stays on the call");
    assert!(!talked.findings[0].value.is_null());
    assert!(c.events_text().await.contains(
        "TEST CALL: said by the owner on the test number, not by the listing's contact; not \
         stored as evidence."
    ));

    // A handoff the owner accepts: no second dial, the accepted handoff and one system line.
    let mut tx = c.t.app.store.begin().await.unwrap();
    let live = CALLS.require(&mut *tx, call.call_id).await.unwrap();
    tx.commit().await.unwrap();
    if matches!(live.state, CallState::Active | CallState::AiResumed) {
        let before = c.server.requests().len();
        let mut tx = c.t.app.store.begin().await.unwrap();
        let h = orch
            .request_handoff(
                &mut *tx,
                call.call_id,
                HandoffReason::FinancialDetails,
                "Can you commit to a five year term?",
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = c.t.app.store.begin().await.unwrap();
        let after = orch
            .owner_response(
                &mut *tx,
                h.handoff_request_id,
                c.owner,
                HandoffStatus::Accepted,
            )
            .await
            .unwrap();
        let stored = HANDOFFS
            .require(&mut *tx, h.handoff_request_id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(stored.status, HandoffStatus::Accepted);
        assert_eq!(dialed(&c.server), vec![OWNER_TEST], "no owner leg");
        assert!(c.server.requests()[before..]
            .iter()
            .all(|r| !r.path.contains("conferences")));
        assert_eq!(
            after
                .transcript
                .iter()
                .filter(|t| t.speaker == "system" && t.text == TEST_CALL_OWNER_LINE)
                .count(),
            1
        );
        assert!(after.participant_states.iter().all(|p| p.role != "owner"));
        not_printed(&serde_json::to_string(&after).unwrap());
    } else {
        panic!(
            "the call is {:?}: the handoff was not exercised",
            live.state
        );
    }

    // Every owner text and every call event starts with the test-call sentence.
    let notices = orch.take_notices();
    assert!(!notices.is_empty());
    for n in &notices {
        assert!(n.text.starts_with(TEST_CALL_PREFIX), "{}", n.text);
        not_printed(&n.text);
    }
    let events = c.events_text().await;
    not_printed(&events);
    let body: Value = serde_json::from_str(&events).unwrap();
    let list = body
        .as_array()
        .or_else(|| body["events"].as_array())
        .or_else(|| body["items"].as_array())
        .expect("an event list");
    let mut call_events = 0;
    for e in list {
        let text = e.to_string();
        // The handoff-requested event has a fixed payload without any text (contract): it
        // cannot carry the sentence and is the one call event without it.
        if !text.contains(&call.call_id.to_string())
            || e["event_type"] == "call.owner_handoff_requested"
        {
            continue;
        }
        call_events += 1;
        assert!(
            text.contains("TEST CALL: "),
            "an event of the test call without the label: {text}"
        );
    }
    assert!(call_events >= 2, "{events}");
    assert!(events.contains(
        "TEST CALL: dialed to the owner's test number instead of the listing's contact; \
         nothing was sent to the contact. dial initiated (not yet answered)"
    ));
    not_printed(&c.t.get("/ready", None).await.text);
});

// The control for the evidence rule: the same conversation on a call that did ring the contact
// (the second switch, no test number) does store what the contact said.
both_backends!(a_call_to_the_contact_still_stores_what_was_said, |db| {
    let c = Ctx::new(db, &[("LIVE_CALLS_TO_CONTACTS", "true")]).await;
    let mut orch = CallOrchestrator::new(&c.t.app);
    let mut tx = c.t.app.store.begin().await.unwrap();
    let call = orch.start(&mut *tx, c.start()).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(dialed(&c.server), vec![CONTACT]);
    assert!(!is_test_call(&call));
    assert!(call.transcript.is_empty());
    c.talk(&mut orch, call.call_id).await;
    assert!(c.evidence_count().await > 0);
    assert!(!c.events_text().await.contains("TEST CALL"));
});

// Without the test number (and without the second switch): 403 before any request, no call
// stored, the approval still APPROVED; once the number is set, the same approval dials it once.
both_backends!(without_the_test_number_nobody_is_dialed, |db| {
    let c = Ctx::new(db, &[]).await;
    let mut tx = c.t.app.store.begin().await.unwrap();
    let err = CallOrchestrator::new(&c.t.app)
        .start(&mut *tx, c.start())
        .await
        .unwrap_err();
    drop(tx);
    assert_eq!(err.status().as_u16(), 403);
    assert!(format!("{err:?}").contains("LIVE_CALLS_ONLY_TO"));
    assert!(c.server.requests().is_empty());
    let mut tx = c.t.app.store.begin().await.unwrap();
    assert!(CALLS
        .list(&mut *tx, Query::new().eq("project_id", c.pid))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        APPROVALS
            .require(&mut *tx, c.approval)
            .await
            .unwrap()
            .approval_status,
        ApprovalStatus::Approved
    );
    tx.commit().await.unwrap();
    let ready = c.t.get("/ready", None).await;
    assert_eq!(
        ready.body["checks"]["live_call_target"],
        "nobody: every live dial is refused until LIVE_CALLS_ONLY_TO is set to the owner's \
         test number (+ and 8 to 15 digits)"
    );
});

both_backends!(ready_says_whether_the_number_is_set_never_its_value, |db| {
    // Fixture mode: no such check.
    let fixture = TestApp::new(db, &[("LIVE_CALLS_ONLY_TO", OWNER_TEST)])
        .get("/ready", None)
        .await;
    assert!(fixture.body["checks"].get("live_call_target").is_none());
    not_printed(&fixture.text);
    for (extra, want) in [
        (
            vec![("LIVE_CALLS_ONLY_TO", OWNER_TEST)],
            "the owner's test number only (LIVE_CALLS_ONLY_TO is set); no listing contact is \
             dialed",
        ),
        (
            vec![("LIVE_CALLS_TO_CONTACTS", "true")],
            "each listing contact's own number (LIVE_CALLS_TO_CONTACTS=true, \
             LIVE_CALLS_ONLY_TO is not set)",
        ),
    ] {
        let ready = TestApp::new(db, &vars(&extra)).get("/ready", None).await;
        assert_eq!(ready.body["checks"]["live_call_target"], want);
        not_printed(&ready.text);
    }
});
