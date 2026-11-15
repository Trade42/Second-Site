//! The first real call (2026-11-15 12:15Z, to the owner's own phone) was hung up one second
//! after it was answered: the carrier's answering-machine detection said "machine". A test call
//! (`LIVE_CALLS_ONLY_TO`) no longer asks for the detection and no longer acts on it; a contact's
//! call is unchanged. The server's own fixed apology is not judged as the model's commitment.
//! The carrier here is a local mock server; no test places a call or sends anything to a provider.

mod common;

use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use ss_domain::communications::call_summary::{self as csum, CallEnd};
use ss_domain::contracts::{CallSession, CallState};
use ss_server::contracts::{ActionType, ApprovalStatus};
use ss_server::integrations::telnyx::{Voice, AMD_OFF};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::call_orchestration::{
    amd_ignored_note, bridge_input, is_test_call, BridgeInput, CallOrchestrator, StartCall,
    APOLOGY_LINE, VOICEMAIL_NOTE,
};
use ss_server::services::call_simulator::FixtureBridge;
use ss_server::services::intro_recording::{self, NewRecording};
use ss_server::services::voice_bridge::{BridgeEnd, EndReason};
use ss_server::store::comms::CALLS;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer, Recorded};
use common::{test_settings, TestApp, TestDb};

const OWNER_TEST: &str = "+15555550177";
const CONTACT: &str = "+15555550100";
const FROM: &str = "+15555550111";
const HOOK: &str = "https://calls.example.com/webhooks/telnyx";
const JUDGED: &str = "sounds like a commitment";

const LIVE: [(&str, &str); 7] = [
    ("VOICE_PROVIDER_MODE", "live"),
    ("ALLOW_LIVE_OUTBOUND_CALLS", "true"),
    ("TELNYX_API_KEY", "KEY-secret-value-1"),
    ("TELNYX_CONNECTION_ID", "conn-value-3"),
    ("TELNYX_FROM_NUMBER", FROM),
    ("TELNYX_PUBLIC_KEY", "PUBKEY-value-2"),
    ("PUBLIC_BASE_URL", "https://calls.example.com"),
];
const TEST_NUMBER: [(&str, &str); 1] = [("LIVE_CALLS_ONLY_TO", OWNER_TEST)];
const CONTACTS: [(&str, &str); 1] = [("LIVE_CALLS_TO_CONTACTS", "true")];

fn vars(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    LIVE.iter().copied().chain(extra.iter().copied()).collect()
}

/// A mock carrier; each has its own call id (a store keeps provider call ids unique).
fn carrier() -> MockServer {
    let id = format!("v3:{}", Uuid::new_v4());
    MockServer::start_with(move |_req: &Recorded| Canned {
        status: 200,
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&json!({"data": {"call_control_id": id, "id": "conf1"}})).unwrap(),
    })
}

/// The last path segment of every request the mock carrier received (`calls`, `speak`, ...).
fn sent(server: &MockServer) -> Vec<String> {
    server
        .requests()
        .iter()
        .map(|r| r.path.rsplit('/').next().unwrap_or("").to_owned())
        .collect()
}

fn dial_bodies(server: &MockServer) -> Vec<String> {
    server
        .requests()
        .iter()
        .filter(|r| r.path.ends_with("/calls"))
        .map(|r| String::from_utf8(r.body.clone()).unwrap())
        .collect()
}

// Item 1: the dial's body. The contact's is byte for byte what HEAD sent.
#[test]
fn a_test_dial_does_not_ask_for_machine_detection() {
    assert_eq!(AMD_OFF, "disabled");
    let server = carrier();
    let base = format!("{}/v2", server.base);
    let voice = Voice::from_settings_with_base_url(&test_settings(&vars(&TEST_NUMBER)), &base);
    voice
        .telephony
        .dial(CONTACT, FROM, HOOK, "k1", None)
        .unwrap();
    assert_eq!(
        dial_bodies(&server),
        vec![
            r#"{"answering_machine_detection":"disabled","connection_id":"conn-value-3","from":"+15555550111","timeout_secs":40,"to":"+15555550177","webhook_url":"https://calls.example.com/webhooks/telnyx"}"#
        ]
    );

    let server = carrier();
    let base = format!("{}/v2", server.base);
    let voice = Voice::from_settings_with_base_url(&test_settings(&vars(&CONTACTS)), &base);
    voice
        .telephony
        .dial(CONTACT, FROM, HOOK, "k1", None)
        .unwrap();
    assert_eq!(
        dial_bodies(&server),
        vec![
            r#"{"answering_machine_detection":"detect","connection_id":"conn-value-3","from":"+15555550111","timeout_secs":40,"to":"+15555550100","webhook_url":"https://calls.example.com/webhooks/telnyx"}"#
        ]
    );
}

struct Ctx {
    t: TestApp,
    server: MockServer,
    bridge: FixtureBridge,
    start: StartCall,
}

fn wav(secs: f64) -> Vec<u8> {
    let rate = 16000u32;
    let n = (f64::from(rate) * secs) as usize;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + n as u32 * 2).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(n as u32 * 2).to_le_bytes());
    for i in 0..n {
        let s = ((i as f64 * 0.05).sin() * 3000.0) as i16;
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

impl Ctx {
    /// A project with an APPROVED call to the listing contact. `live`: the live voice client
    /// against the mock carrier (otherwise the fixture carrier). `bridged`: the fixture voice
    /// bridge and the owner's recorded introduction.
    async fn new(
        db: &TestDb,
        live: Option<&[(&'static str, &'static str)]>,
        bridged: bool,
    ) -> Self {
        let server = carrier();
        let t = match live {
            Some(extra) => {
                let t = TestApp::new(db, &vars(extra));
                let voice = Voice::from_settings_with_base_url(
                    &t.app.settings,
                    &format!("{}/v2", server.base),
                );
                assert_eq!(voice.mode(), "live");
                t.app.extensions.insert(Arc::new(voice));
                t
            }
            None => TestApp::new(db, &[]),
        };
        let bridge = FixtureBridge::new();
        if bridged {
            let vc = t.app.voice_calls();
            vc.set_launcher(Some(Arc::new(bridge.clone())));
            vc.bind(&t.app);
        }
        let (token, owner) = t.dev_token().await;
        let p = t
            .create_project(
                &token,
                &json!({"project_name": "t", "market": {"city": "Los Angeles", "state": "CA"}}),
            )
            .await;
        let pid: Uuid = p["project"]["project_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let mut recipient = IndexMap::new();
        recipient.insert("role".to_owned(), "broker".to_owned());
        recipient.insert("name".to_owned(), "Dana".to_owned());
        recipient.insert("phone".to_owned(), CONTACT.to_owned());
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
                    ss_domain::communications::call_payload(pid, &recipient, None),
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
        if bridged {
            let data = wav(3.0);
            let info = intro_recording::check_wav(&data).unwrap();
            intro_recording::store(
                &t.app,
                NewRecording {
                    owner_id: owner,
                    data,
                    source: "api_upload",
                    link_token: None,
                },
                info,
            )
            .await
            .unwrap();
        }
        let start = StartCall {
            project_id: pid,
            owner_id: owner,
            recipient,
            property_id: None,
            approval_id: Some(a.approval_id),
            intro_recording_reference: None,
            communication_id: None,
            simulated: false,
        };
        Self {
            t,
            server,
            bridge,
            start,
        }
    }

    async fn start(&self) -> CallSession {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let call = CallOrchestrator::new(&self.t.app)
            .start(&mut *tx, self.start.clone())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        call
    }

    /// One carrier event of the dialed leg, through the orchestrator (as the webhook does).
    async fn event(&self, call: &CallSession, et: &str, payload: Value) -> CallSession {
        let mut p: Map<String, Value> = payload.as_object().cloned().unwrap_or_default();
        p.insert(
            "call_control_id".into(),
            json!(call.provider_call_id.clone().unwrap()),
        );
        let mut tx = self.t.app.store.begin().await.unwrap();
        let mut orch = CallOrchestrator::new(&self.t.app);
        orch.handle_event(&mut *tx, &json!({"data": {"event_type": et, "payload": p}}))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        self.call(call.call_id).await
    }

    async fn call(&self, id: Uuid) -> CallSession {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let c = CALLS.require(&mut *tx, id).await.unwrap();
        tx.commit().await.unwrap();
        c
    }
}

fn notes(c: &CallSession, text: &str) -> usize {
    c.transcript
        .iter()
        .filter(|t| t.speaker == "system" && t.text == text)
        .count()
}

const MACHINE_NOTE: &str = "[test call: answering-machine detection reported machine; ignored, \
                            the person on the line is the owner]";

// Item 2, the carrier's own speech (no bridge): a test call is not hung up and goes on; a
// contact's call gets the short message and the hangup, as before.
both_backends!(a_machine_event_does_not_end_a_test_call, |db| {
    assert_eq!(amd_ignored_note(Some("machine")), MACHINE_NOTE);
    assert_eq!(
        amd_ignored_note(Some("x]\n[y z")),
        "[test call: answering-machine detection reported xyz; ignored, the person on the line \
         is the owner]"
    );
    let c = Ctx::new(db, Some(&TEST_NUMBER), false).await;
    let call = c.start().await;
    assert!(is_test_call(&call));
    assert_eq!(sent(&c.server), vec!["calls"]);
    let machine = json!({"result": "machine"});
    let call = c
        .event(&call, "call.machine.detection.ended", machine.clone())
        .await;
    // The carrier repeats an event: still one line.
    let call = c
        .event(&call, "call.machine.detection.ended", machine.clone())
        .await;
    assert_eq!(notes(&call, MACHINE_NOTE), 1);
    assert_eq!(notes(&call, VOICEMAIL_NOTE), 0);
    assert_eq!(call.state, CallState::Dialing);
    assert_eq!(sent(&c.server), vec!["calls"], "nothing spoken, no hangup");
    // The call goes on: answered, the introduction is spoken.
    let call = c.event(&call, "call.answered", json!({})).await;
    assert_eq!(call.state, CallState::Intro);
    let call = c
        .event(&call, "call.machine.detection.ended", machine.clone())
        .await;
    assert_eq!(call.state, CallState::Intro);
    assert!(!sent(&c.server).iter().any(|p| p == "hangup"));
    let call = c.event(&call, "call.speak.ended", json!({})).await;
    assert_eq!(call.state, CallState::Active);

    // A contact's call: the existing rule.
    let c = Ctx::new(db, Some(&CONTACTS), false).await;
    let call = c.start().await;
    assert!(!is_test_call(&call));
    let call = c.event(&call, "call.answered", json!({})).await;
    let before = sent(&c.server).len();
    let call = c
        .event(&call, "call.machine.detection.ended", machine)
        .await;
    assert_eq!(notes(&call, MACHINE_NOTE), 0);
    assert_eq!(&sent(&c.server)[before..], ["speak", "hangup"]);
    assert!(call
        .transcript
        .last()
        .unwrap()
        .text
        .contains("We'll follow up by email"));
});

// Items 2 and 3, the shape of the first real call: a bridged test call, answered, the detection
// says machine one second later. The bridge is not paused, the call is not hung up, the
// assistant starts after the recording. When the bridge then fails, the server's apology is in
// the transcript and is not in the summary as a commitment.
both_backends!(a_bridged_test_call_survives_machine_detection, |db| {
    let c = Ctx::new(db, Some(&TEST_NUMBER), true).await;
    let call = c.start().await;
    assert!(is_test_call(&call));
    let call = c.event(&call, "call.answered", json!({})).await;
    assert_eq!(call.state, CallState::Intro);
    let url = c
        .server
        .requests()
        .iter()
        .find(|r| r.path.ends_with("/playback_start"))
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["audio_url"].clone())
        .expect("the recording is played");
    let call = c
        .event(
            &call,
            "call.machine.detection.ended",
            json!({"result": "machine"}),
        )
        .await;
    assert_eq!(notes(&call, MACHINE_NOTE), 1);
    assert_eq!(notes(&call, VOICEMAIL_NOTE), 0);
    assert!(!sent(&c.server).iter().any(|p| p == "hangup"));
    assert!(!c.bridge.log().iter().any(|l| l == "pause"));
    let call = c
        .event(
            &call,
            "call.playback.ended",
            json!({"status": "completed", "media_url": url}),
        )
        .await;
    assert_eq!(call.state, CallState::Active, "the assistant starts");
    assert!(sent(&c.server).iter().any(|p| p == "streaming_start"));

    // The bridge fails: the fixed apology on the carrier, then the hangup.
    let (_, spec) = c.bridge.specs()[0].clone();
    spec.events
        .ended(BridgeEnd {
            reason: EndReason::IdleTimeout,
            stats: Default::default(),
        })
        .await;
    let failed = c.call(call.call_id).await;
    assert_eq!(failed.transcript.last().unwrap().text, APOLOGY_LINE);
    let ended = c
        .event(
            &failed,
            "call.hangup",
            json!({"hangup_cause": "normal_clearing"}),
        )
        .await;
    let summary = ended.outcome_summary.clone().unwrap_or_default();
    assert!(!summary.is_empty());
    assert!(!summary.contains(JUDGED), "{summary}");
    assert!(!summary.contains("technical problem"), "{summary}");
    // The control: the same transcript, unexempted, is judged (the first call's summary).
    let raw = csum::summarize(&csum::SummaryInput::from_call(
        &ended,
        CallEnd::Completed,
        Vec::new(),
        None,
    ));
    assert!(raw
        .needs_owner_attention
        .iter()
        .any(|i| i.statement.contains(JUDGED)));
});

// Item 3 is by where the line was written, not by its words: the model saying the apology's
// words on a bridged call is still judged; the server's own apology at the end is not.
both_backends!(the_models_same_words_are_still_judged, |db| {
    for model_says_it in [false, true] {
        let c = Ctx::new(db, None, true).await;
        let call = c.start().await;
        let call = c.event(&call, "call.answered", json!({})).await;
        let url =
            c.t.app
                .voice()
                .fixture
                .clone()
                .unwrap()
                .commands()
                .iter()
                .find_map(|cmd| match cmd {
                    ss_domain::integrations::telnyx::TelephonyCommand::PlaybackStart {
                        audio_url,
                        ..
                    } => Some(audio_url.clone()),
                    _ => None,
                })
                .unwrap();
        let call = c
            .event(
                &call,
                "call.playback.ended",
                json!({"status": "completed", "media_url": url}),
            )
            .await;
        assert_eq!(call.state, CallState::Active);
        if model_says_it {
            bridge_input(
                &c.t.app,
                call.call_id,
                BridgeInput::Assistant(APOLOGY_LINE.into()),
            )
            .await
            .unwrap();
        }
        let (_, spec) = c.bridge.specs()[0].clone();
        spec.events
            .ended(BridgeEnd {
                reason: EndReason::IdleTimeout,
                stats: Default::default(),
            })
            .await;
        let failed = c.call(call.call_id).await;
        let said = failed
            .transcript
            .iter()
            .filter(|t| t.speaker == "assistant" && t.text == APOLOGY_LINE)
            .count();
        assert_eq!(said, if model_says_it { 2 } else { 1 });
        let ended = c
            .event(
                &failed,
                "call.hangup",
                json!({"hangup_cause": "normal_clearing"}),
            )
            .await;
        let summary = ended.outcome_summary.clone().unwrap_or_default();
        assert_eq!(summary.contains(JUDGED), model_says_it, "{summary}");
    }
});
