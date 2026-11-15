//! voice-wire-1: the recording first, then the Grok Voice bridge, with the server deciding.
//! A fixture bridge (`call_simulator::FixtureBridge`) stands in for the live one and drives the
//! same orchestrator code through the spec's `BridgeEvents`. Nothing reaches a carrier or xAI.

mod common;

use std::sync::Arc;

use chrono::Utc;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use ss_domain::contracts::{CallSession, CallState, HandoffStatus};
use ss_domain::integrations::telnyx::TelephonyCommand;
use ss_server::contracts::ActionType;
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::call_orchestration::{
    stream_url, BridgeInput, CallOrchestrator, StartCall, APOLOGY_LINE, NOT_APPROVED_OUTPUT,
    OWNER_JOINED_LINE,
};
use ss_server::services::call_simulator::{CallSimulator, FixtureBridge};
use ss_server::services::intro_recording::{self, NewRecording};
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::services::voice_bridge::{Speaker, ToolCall, TranscriptLine};
use ss_server::services::voice_session_config as vsc;
use ss_server::store::comms::{CALLS, HANDOFFS};
use ss_server::store::repo::EVIDENCE;
use ss_server::store::Query;
use uuid::Uuid;

use common::{TestApp, TestDb};

struct V {
    t: TestApp,
    fake: Arc<RecordingNotifier>,
    bridge: FixtureBridge,
}

fn setup(db: &TestDb, extra: &[(&str, &str)]) -> V {
    let t = TestApp::new(db, extra);
    let fake = Arc::new(RecordingNotifier::new());
    t.app
        .extensions
        .insert(Arc::new(OwnerNotify::new(fake.clone())));
    let bridge = FixtureBridge::new();
    let vc = t.app.voice_calls();
    vc.set_launcher(Some(Arc::new(bridge.clone())));
    vc.bind(&t.app);
    V { t, fake, bridge }
}

impl V {
    fn commands(&self) -> Vec<TelephonyCommand> {
        self.t.app.voice().fixture.clone().unwrap().commands()
    }
    fn cmd_names(&self) -> Vec<String> {
        self.commands().iter().map(|c| c.cmd().to_owned()).collect()
    }
    async fn call(&self, id: Uuid) -> CallSession {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let c = CALLS.require(&mut *tx, id).await.unwrap();
        tx.commit().await.unwrap();
        c
    }
    async fn events(&self, call: &CallSession, evs: Vec<(&str, Map<String, Value>)>) -> CallSession {
        let mut tx = self.t.app.store.begin().await.unwrap();
        let mut sim = CallSimulator::new(&self.t.app).unwrap();
        let c = sim.leg_events(&mut *tx, call.clone(), evs).await.unwrap();
        tx.commit().await.unwrap();
        c
    }
}

/// An owner who exists as a user, a project, and an APPROVED PLACE_CALL approval.
async fn approved(v: &V) -> (Uuid, Uuid, IndexMap<String, String>, Uuid) {
    let (token, owner) = v.t.dev_token().await;
    let p = v
        .t
        .create_project(
            &token,
            &json!({"project_name": "t", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await;
    let pid: Uuid = p["project"]["project_id"].as_str().unwrap().parse().unwrap();
    let mut recipient = IndexMap::new();
    recipient.insert("role".to_owned(), "broker".to_owned());
    recipient.insert("name".to_owned(), "Dana".to_owned());
    recipient.insert("phone".to_owned(), "+15555550100".to_owned());
    let payload = ss_domain::communications::call_payload(pid, &recipient, None);
    let mut tx = v.t.app.store.begin().await.unwrap();
    let a = v
        .t
        .app
        .approvals()
        .request(
            &mut *tx,
            ApprovalSpec::new(pid, owner, "t", ActionType::PlaceCall, payload, "call", format!("c-{}", Uuid::new_v4())),
        )
        .await
        .unwrap();
    v.t.app
        .approvals()
        .decide(&mut *tx, a.approval_id, owner, ss_server::contracts::ApprovalStatus::Approved, &a.action_payload_hash)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (owner, pid, recipient, a.approval_id)
}

async fn start(v: &V, simulated: bool) -> (Uuid, CallSession) {
    let (owner, pid, recipient, approval_id) = approved(v).await;
    let mut tx = v.t.app.store.begin().await.unwrap();
    let mut orch = CallOrchestrator::new(&v.t.app);
    let call = orch
        .start(
            &mut *tx,
            StartCall {
                project_id: pid,
                owner_id: owner,
                recipient,
                property_id: None,
                approval_id: Some(approval_id),
                intro_recording_reference: Some("ignored-request-key".into()),
                communication_id: None,
                simulated,
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (owner, call)
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

async fn record_intro(v: &V, owner: Uuid) {
    let data = wav(3.0);
    let info = intro_recording::check_wav(&data).unwrap();
    intro_recording::store(
        &v.t.app,
        NewRecording { owner_id: owner, data, source: "api_upload", link_token: None },
        info,
    )
    .await
    .unwrap();
}

fn line(speaker: Speaker, text: &str, turn_id: u64) -> TranscriptLine {
    TranscriptLine { speaker, text: text.into(), turn_id, at: Utc::now() }
}

fn says(log: &[String]) -> Vec<String> {
    log.iter().filter_map(|l| l.strip_prefix("say:").map(str::to_owned)).collect()
}

fn st(status: &str, url: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("status".into(), json!(status));
    m.insert("media_url".into(), json!(url));
    m
}

both_backends!(recording_first_then_the_bridge, |db| {
    let v = setup(db, &[]);
    let (owner, pid, recipient, approval_id) = approved(&v).await;
    record_intro(&v, owner).await;
    let mut tx = v.t.app.store.begin().await.unwrap();
    let call = CallOrchestrator::new(&v.t.app)
        .start(&mut *tx, StartCall {
            project_id: pid, owner_id: owner, recipient, property_id: None,
            approval_id: Some(approval_id), intro_recording_reference: Some("from-request".into()),
            communication_id: None, simulated: false,
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // The reference is the owner's ACTIVE recording, not the request's.
    let key = call.intro_recording_reference.clone().unwrap();
    assert!(key.starts_with("intro-recordings/"), "{key}");

    let call = v.events(&call, vec![("call.answered", Map::new())]).await;
    assert_eq!(call.state, CallState::Intro);
    assert_eq!(v.cmd_names(), vec!["dial", "playback_start"], "the recording and nothing else");
    let url = v.commands().iter().find_map(|c| match c {
        TelephonyCommand::PlaybackStart { audio_url, .. } => Some(audio_url.clone()),
        _ => None,
    }).unwrap();
    // Neither a speak end, nor another playback's end, nor a transcription moves the call.
    let mut tr = Map::new();
    tr.insert("transcription_data".into(), json!({"transcript": "hello?", "is_final": true}));
    let call = v.events(&call, vec![
        ("call.speak.ended", Map::new()),
        ("call.playback.ended", st("completed", "https://elsewhere/other.wav")),
        ("call.playback.ended", Map::new()),
        ("call.transcription", tr),
    ]).await;
    assert_eq!(call.state, CallState::Intro);
    assert_eq!(v.cmd_names(), vec!["dial", "playback_start"]);
    assert!(v.bridge.specs().is_empty());
    // Our playback's end: ACTIVE, one bridge, the stream.
    let call = v.events(&call, vec![("call.playback.ended", st("completed", &url))]).await;
    assert_eq!(call.state, CallState::Active);
    assert_eq!(v.cmd_names(), vec!["dial", "playback_start", "streaming_start"]);
    let specs = v.bridge.specs();
    assert_eq!(specs.len(), 1);
    let (bid, spec) = &specs[0];
    assert!(spec.token.len() >= 32);
    let stream = v.commands().iter().find_map(|c| match c {
        TelephonyCommand::StreamingStart { stream, .. } => Some(stream.stream_url.clone()),
        _ => None,
    }).unwrap();
    assert_eq!(stream, stream_url(&v.t.app.settings.public_base_url, bid, &spec.token));
    let opening = spec.opening_line.clone().unwrap();
    assert!(opening.starts_with("Hello, this is an AI assistant, an automated system, calling on behalf of "), "{opening}");
    assert!(opening.ends_with(&call.automation_disclosure), "{opening}");
    // A repeated ended event does not open a second bridge.
    let call = v.events(&call, vec![("call.playback.ended", st("completed", &url))]).await;
    assert_eq!(v.bridge.specs().len(), 1);
    assert_eq!(call.state, CallState::Active);
});

both_backends!(intro_failures_end_politely, |db| {
    // A failed playback: the fixed apology on the carrier, then hangup; no bridge.
    let v = setup(db, &[]);
    let (owner, call) = {
        let (owner, pid, recipient, approval_id) = approved(&v).await;
        record_intro(&v, owner).await;
        let mut tx = v.t.app.store.begin().await.unwrap();
        let c = CallOrchestrator::new(&v.t.app).start(&mut *tx, StartCall {
            project_id: pid, owner_id: owner, recipient, property_id: None,
            approval_id: Some(approval_id), intro_recording_reference: None,
            communication_id: None, simulated: false,
        }).await.unwrap();
        tx.commit().await.unwrap();
        (owner, c)
    };
    let call = v.events(&call, vec![("call.answered", Map::new())]).await;
    let url = v.commands().iter().find_map(|c| match c {
        TelephonyCommand::PlaybackStart { audio_url, .. } => Some(audio_url.clone()),
        _ => None,
    }).unwrap();
    let call = v.events(&call, vec![("call.playback.ended", st("failed", &url))]).await;
    assert_eq!(call.state, CallState::Intro);
    assert_eq!(v.cmd_names(), vec!["dial", "playback_start", "speak", "hangup"]);
    assert_eq!(call.transcript.last().unwrap().text, APOLOGY_LINE);
    assert!(v.bridge.specs().is_empty());

    // No ended event in time: the watchdog's input ends it the same way.
    let v = setup(db, &[]);
    let (_o, call) = {
        let (owner, pid, recipient, approval_id) = approved(&v).await;
        record_intro(&v, owner).await;
        let mut tx = v.t.app.store.begin().await.unwrap();
        let c = CallOrchestrator::new(&v.t.app).start(&mut *tx, StartCall {
            project_id: pid, owner_id: owner, recipient, property_id: None,
            approval_id: Some(approval_id), intro_recording_reference: None,
            communication_id: None, simulated: false,
        }).await.unwrap();
        tx.commit().await.unwrap();
        (owner, c)
    };
    let call = v.events(&call, vec![("call.answered", Map::new())]).await;
    ss_server::services::call_orchestration::bridge_input(&v.t.app, call.call_id, BridgeInput::IntroTimeout).await.unwrap();
    let call = v.call(call.call_id).await;
    assert_eq!(v.cmd_names(), vec!["dial", "playback_start", "speak", "hangup"]);
    assert!(call.transcript.iter().any(|t| t.text.contains("did not end within its bound")));

    // Revoked between approval and answer: hung up without a word.
    let v = setup(db, &[]);
    let (owner2, pid, recipient, approval_id) = approved(&v).await;
    record_intro(&v, owner2).await;
    let mut tx = v.t.app.store.begin().await.unwrap();
    let call = CallOrchestrator::new(&v.t.app).start(&mut *tx, StartCall {
        project_id: pid, owner_id: owner2, recipient, property_id: None,
        approval_id: Some(approval_id), intro_recording_reference: None,
        communication_id: None, simulated: false,
    }).await.unwrap();
    tx.commit().await.unwrap();
    intro_recording::revoke(&v.t.app, owner2).await.unwrap();
    let call = v.events(&call, vec![("call.answered", Map::new())]).await;
    assert_eq!(v.cmd_names(), vec!["dial", "hangup"]);
    assert!(call.transcript.iter().all(|t| t.speaker != "assistant"));
    let _ = owner;
});

both_backends!(the_server_decides_on_a_simulated_bridged_call, |db| {
    let v = setup(db, &[]);
    let (owner, call) = start(&v, true).await;
    // A simulated call without a recording goes straight to the bridge, labelled simulated.
    let call = v.events(&call, vec![("call.answered", Map::new())]).await;
    assert_eq!(call.state, CallState::Active);
    assert!(!v.cmd_names().iter().any(|c| c == "speak" || c == "transcription_start"), "{:?}", v.cmd_names());
    let (_, spec) = v.bridge.specs()[0].clone();
    let ins = &spec.session.instructions;
    for needle in ["AI assistant", "Never say the owner has joined", "request_owner_approval", "Approved questions", "may not"] {
        assert!(ins.contains(needle), "{needle}");
    }
    assert!(!ins.to_lowercase().contains("agent_id"));
    let names: Vec<_> = spec.session.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec![vsc::TOOL_ESCALATE, vsc::TOOL_APPROVAL]);
    if std::env::var("VOICE_WIRE_PRINT").is_ok() {
        println!("SESSION_UPDATE {}", serde_json::to_string_pretty(&spec.session.session_update()).unwrap());
        println!("OPENING {}", spec.opening_line.clone().unwrap());
    }
    let ev = spec.events.clone();

    // A counterparty answer: a finding recorded as a reported claim, labelled simulated.
    ev.transcript(line(Speaker::Counterparty, "It's about 1,250 usable square feet with a 200 amp panel.", 1)).await;
    let c = v.call(call.call_id).await;
    assert!(!c.findings.is_empty(), "{:?}", c.findings);
    assert!(c.findings.iter().all(|f| f.classification == "OWNER_REPORTED"));
    // The model's own words are kept once; our opening line is not repeated.
    ev.transcript(line(Speaker::Assistant, &spec.opening_line.clone().unwrap(), 2)).await;
    let c2 = v.call(call.call_id).await;
    assert_eq!(c2.transcript.len(), c.transcript.len());

    // A commitment asked through the tool: not approved, the owner is asked, the bridge pauses.
    let out = ev.tool_call(ToolCall { name: vsc::TOOL_APPROVAL.into(), arguments: json!({"kind": "commitment", "detail": "hold the space until Friday"}), turn_id: 3 }).await;
    assert_eq!(out, NOT_APPROVED_OUTPUT);
    let c = v.call(call.call_id).await;
    assert_eq!(c.state, CallState::HandoffPending);
    assert!(v.t.app.voice_calls().is_paused(call.call_id));
    assert_eq!(v.bridge.log().last().map(String::as_str), Some("pause"));
    v.t.app.owner_notify().idle().await;
    assert!(v.fake.requests().iter().any(|r| r.text.starts_with("[Simulated call]")));
    // During the handoff nothing is decided or said through the bridge.
    let before = v.bridge.log().len();
    ev.transcript(line(Speaker::Counterparty, "Hello? Are you still there?", 4)).await;
    assert_eq!(v.bridge.log().len(), before);
    // The owner defers: AI_RESUMED, the bridge resumes, the deferred line is said through it.
    let hid = c.handoff_request_id.unwrap();
    let mut tx = v.t.app.store.begin().await.unwrap();
    CallOrchestrator::new(&v.t.app).owner_response(&mut *tx, hid, owner, HandoffStatus::Deferred).await.unwrap();
    tx.commit().await.unwrap();
    let c = v.call(call.call_id).await;
    assert_eq!(c.state, CallState::AiResumed);
    let log = v.bridge.log();
    let i = log.iter().rposition(|l| l == "resume").unwrap();
    assert!(log[i + 1].starts_with("say:The owner can't join right now"), "{log:?}");

    // A sensitive-data request: the Response Agent holds the call; nothing sensitive is stored.
    ev.transcript(line(Speaker::Counterparty, "Before we continue, what is the owner's date of birth and social security number 078-05-1120?", 5)).await;
    let c = v.call(call.call_id).await;
    assert_eq!(c.state, CallState::HandoffPending, "{:?}", c.transcript);
    let all = serde_json::to_string(&c).unwrap();
    assert!(!all.contains("078-05-1120") && !all.contains("078051120"));
    let mut tx = v.t.app.store.begin().await.unwrap();
    let h = HANDOFFS.require(&mut *tx, c.handoff_request_id.unwrap()).await.unwrap();
    tx.commit().await.unwrap();
    assert!(!h.question.contains("078"));

    // The owner joins: only on HUMAN_JOINED does the server say so, on the carrier.
    let mut tx = v.t.app.store.begin().await.unwrap();
    let r = CallOrchestrator::new(&v.t.app).owner_response(&mut *tx, h.handoff_request_id, owner, HandoffStatus::Accepted).await.unwrap();
    tx.commit().await.unwrap();
    // No phone on file for this owner: the existing deferred path, no "joined" line anywhere.
    assert_eq!(r.state, CallState::AiResumed);
    assert!(!says(&v.bridge.log()).iter().any(|s| s.contains("joined") || s.contains("is joining")));
    assert!(!r.transcript.iter().any(|t| t.text == OWNER_JOINED_LINE));

    // Hang up: terminal closes the bridge; the summary is labelled; later events do nothing.
    let mut h = Map::new();
    h.insert("hangup_cause".into(), json!("normal_clearing"));
    let c = v.events(&r, vec![("call.hangup", h)]).await;
    assert_eq!(c.state, CallState::Completed);
    assert_eq!(v.bridge.log().last().map(String::as_str), Some("stop"));
    assert_eq!(v.bridge.removed().len(), 1);
    assert!(!v.t.app.voice_calls().is_bridged(call.call_id));
    let out = ev.tool_call(ToolCall { name: vsc::TOOL_APPROVAL.into(), arguments: json!({"kind": "sensitive_disclosure", "detail": "x"}), turn_id: 9 }).await;
    assert_eq!(out, NOT_APPROVED_OUTPUT);
    let mut tx = v.t.app.store.begin().await.unwrap();
    let ev_rows = EVIDENCE.list(&mut *tx, Query::new()).await.unwrap();
    tx.commit().await.unwrap();
    for e in ev_rows.iter().filter(|e| e.provider == "broker_call") {
        assert_eq!(e.data_origin, ss_core::DataOrigin::FixtureSynthetic);
        assert!(e.limitations[0].starts_with("SIMULATED CALL"));
    }
    v.t.app.owner_notify().idle().await;
    assert!(v.fake.requests().iter().any(|r| r.purpose == "call_summary" && r.text.starts_with("[Simulated call]")));
});

both_backends!(a_bridge_that_ends_by_itself_apologises, |db| {
    let v = setup(db, &[]);
    let (_owner, call) = start(&v, true).await;
    let call = v.events(&call, vec![("call.answered", Map::new())]).await;
    let (_, spec) = v.bridge.specs()[0].clone();
    spec.events.ended(ss_server::services::voice_bridge::BridgeEnd {
        reason: ss_server::services::voice_bridge::EndReason::IdleTimeout,
        stats: Default::default(),
    }).await;
    let c = v.call(call.call_id).await;
    assert!(c.transcript.iter().any(|t| t.text.contains("the voice assistant ended (idle_timeout)")));
    assert_eq!(c.transcript.last().unwrap().text, APOLOGY_LINE);
    let names = v.cmd_names();
    assert_eq!(&names[names.len() - 2..], ["speak", "hangup"]);
    assert_eq!(v.bridge.removed().len(), 1);
});

both_backends!(the_bridge_is_off_by_default, |db| {
    // No launcher (the settings' default): no bridge, the carrier's speak and transcription.
    let t = TestApp::new(db, &[]);
    assert!(!t.app.voice_calls().enabled());
    assert!(!t.app.settings.voice_bridge_active());
    let t2 = TestApp::new(db, &[("VOICE_BRIDGE_ENABLED", "true"), ("XAI_API_KEY", "x")]);
    assert!(!t2.app.settings.voice_bridge_active(), "fixture voice mode keeps it off");
});

both_backends!(the_stream_route_refuses_without_the_secret, |db| {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    let t = TestApp::new(db, &[]);
    let req = |uri: &str| {
        Request::builder()
            .uri(uri)
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Body::empty())
            .unwrap()
    };
    for uri in ["/api/v1/voice/stream/abc?token=wrong", "/api/v1/voice/stream/abc"] {
        let r = t.send(req(uri)).await;
        // Without an upgradable connection axum may refuse earlier; never a 101.
        assert_ne!(r.status, StatusCode::SWITCHING_PROTOCOLS);
        assert!(r.status.is_client_error(), "{uri}: {}", r.status);
    }
});
