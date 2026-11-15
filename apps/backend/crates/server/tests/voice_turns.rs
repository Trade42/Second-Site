//! voice-turns-1: one speaker per turn on a bridged call. The orchestrator and the real voice
//! bridge run against a mock xAI realtime server and a mock carrier media stream (both local
//! WebSockets), on both stores. The first test replays the second real call (2026-11-15
//! 12:56Z): the same counterparty sentences in the same order. The mock model behaves as xAI's
//! does under server VAD (it answers every counterparty sentence by itself) and follows its
//! instructions and the server's latest direction. No real call, no provider.

#![allow(clippy::result_large_err)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query as AxQuery, State, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use ss_domain::contracts::{CallQuestion, CallSession, CallState};
use ss_domain::integrations::telnyx::TelephonyCommand;
use ss_server::contracts::ActionType;
use ss_server::integrations::xai_realtime::{SessionSpec, XaiConfig};
use ss_server::services::approvals::ApprovalSpec;
use ss_server::services::call_orchestration::{
    asks_for_time, closing_line, CallOrchestrator, LiveBridgeLauncher, StartCall, APOLOGY_LINE,
    CLOSED_NOTE, HUNG_UP_NOTE, STILL_HERE_LINE,
};
use ss_server::services::call_simulator::CallSimulator;
use ss_server::services::notifier::{OwnerNotify, RecordingNotifier};
use ss_server::services::voice_bridge::{
    accept_carrier, BridgeEnd, BridgeEvents, BridgeOptions, BridgeRegistry, CallBridgeSpec,
    CheckIn, EndReason, ToolCall, TranscriptLine, INSTRUCTION_PREFIX,
};
use ss_server::store::comms::CALLS;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use common::{TestApp, TestDb};

const KEY: &str = "xai-test-key-voice-turns-0123456789";
const T: Duration = Duration::from_secs(10);

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

// ---------------------------------------------------------------- mock xAI

struct MockXai {
    url: String,
    rx: mpsc::UnboundedReceiver<Value>,
    tx: mpsc::UnboundedSender<Option<Value>>,
    /// The latest server direction the "model" was given.
    direction: String,
    /// Every non-audio client event, in order.
    seen: Vec<Value>,
    responses: u32,
}

impl MockXai {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (in_tx, rx) = mpsc::unbounded_channel();
        let (tx, mut out_rx) = mpsc::unbounded_channel::<Option<Value>>();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            loop {
                tokio::select! {
                    m = ws.next() => match m {
                        Some(Ok(Message::Text(t))) => {
                            let v: Value = serde_json::from_str(&t).expect("client json");
                            let _ = in_tx.send(v);
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        _ => {}
                    },
                    o = out_rx.recv() => match o {
                        Some(Some(v)) => { let _ = ws.send(Message::text(v.to_string())).await; }
                        Some(None) => { let _ = ws.close(None).await; break; }
                        None => break,
                    },
                }
            }
        });
        Self {
            url: format!("ws://{addr}/v1/realtime?model=grok-voice-latest"),
            rx,
            tx,
            direction: String::new(),
            seen: Vec::new(),
            responses: 0,
        }
    }

    fn send(&self, v: Value) {
        let _ = self.tx.send(Some(v));
    }

    fn note(&mut self, v: &Value) {
        if v["type"] == "input_audio_buffer.append" {
            return;
        }
        if v["type"] == "conversation.item.create" && v["item"]["role"] == "system" {
            let text = v["item"]["content"][0]["text"].as_str().unwrap_or("");
            self.direction = text
                .strip_prefix(INSTRUCTION_PREFIX)
                .unwrap_or(text)
                .to_owned();
        }
        self.seen.push(v.clone());
    }

    /// The next client event that is not an audio append.
    async fn next(&mut self) -> Value {
        loop {
            let v = tokio::time::timeout(T, self.rx.recv())
                .await
                .expect("xai event in time")
                .expect("xai channel");
            self.note(&v);
            if v["type"] != "input_audio_buffer.append" {
                return v;
            }
        }
    }

    /// Wait until a client event satisfies `f`.
    async fn until(&mut self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let end = tokio::time::Instant::now() + T;
        loop {
            let v = tokio::time::timeout_at(end, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("no {what} in time"))
                .expect("xai channel");
            self.note(&v);
            if f(&v) {
                return v;
            }
        }
    }

    /// Everything the client sends within `d`.
    async fn drain(&mut self, d: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(Some(v)) = tokio::time::timeout(d, self.rx.recv()).await {
            self.note(&v);
            if v["type"] != "input_audio_buffer.append" {
                out.push(v);
            }
        }
        out
    }

    /// One whole response: created, audio (one second of mu-law), its transcript, done.
    fn respond(&mut self, transcript: &str) -> String {
        self.responses += 1;
        let id = format!("resp_{}", self.responses);
        self.send(json!({"type": "response.created", "response": {"id": id, "status": "in_progress", "output": []}}));
        for c in vec![0x7Fu8; 8000].chunks(1600) {
            self.send(json!({"type": "response.output_audio.delta", "response_id": id, "item_id": "it", "output_index": 0, "content_index": 0, "delta": b64(c)}));
        }
        self.send(json!({"type": "response.output_audio_transcript.done", "response_id": id, "item_id": "it", "transcript": transcript}));
        self.send(json!({"type": "response.done", "response": {"id": id, "status": "completed"}}));
        id
    }

    /// The counterparty's sentence as xAI reports it: speech start and stop, its transcript.
    fn heard(&mut self, n: usize, text: &str) {
        let item = format!("item_{n}");
        self.send(json!({"type": "input_audio_buffer.speech_started", "item_id": item}));
        self.send(json!({"type": "input_audio_buffer.speech_stopped", "item_id": item}));
        self.send(json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": item, "transcript": text}));
    }

    /// The numbers the direction lists after `marker`, up to the next full stop.
    fn numbers_after(&self, marker: &str) -> Vec<usize> {
        let Some(i) = self.direction.find(marker) else {
            return Vec::new();
        };
        let rest = &self.direction[i + marker.len()..];
        rest.split('.')
            .next()
            .unwrap_or("")
            .split(',')
            .filter_map(|n| n.trim().parse().ok())
            .collect()
    }

    /// The model's reply to a counterparty sentence, as its instructions and the server's
    /// direction tell it: a request for time gets one short acknowledgement; anything else an
    /// acknowledgement and the next question still to ask (by its number in the instructions),
    /// or only the acknowledgement when none is left.
    fn model_reply(&self, instructions: &str, sentence: &str, ack: &str) -> String {
        if asks_for_time(sentence) {
            return "Of course, take your time. I'm here whenever you're ready.".into();
        }
        match self.numbers_after("in this order: ").first() {
            Some(n) => {
                let q = instructions
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{n}. ")))
                    .expect("the numbered question");
                format!("{ack} {q}")
            }
            None => ack.to_owned(),
        }
    }
}

// ---------------------------------------------------------------- carrier and app

type Carrier = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone)]
struct Reg(BridgeRegistry);

#[derive(serde::Deserialize)]
struct Tok {
    token: Option<String>,
}

async fn route(
    State(s): State<Reg>,
    Path(id): Path<String>,
    AxQuery(q): AxQuery<Tok>,
    ws: WebSocketUpgrade,
) -> Response {
    accept_carrier(&s.0, &id, q.token.as_deref(), ws)
}

async fn serve(registry: BridgeRegistry) -> String {
    let app = Router::new()
        .route("/api/v1/voice/stream/{id}", get(route))
        .with_state(Reg(registry));
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = l.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    format!("ws://{addr}")
}

async fn carrier_start(url: &str, call_control_id: &str) -> Carrier {
    let (mut c, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("carrier accepted");
    c.send(Message::text(
        json!({"event": "connected", "version": "1.0.0"}).to_string(),
    ))
    .await
    .expect("send");
    c.send(Message::text(json!({"event": "start", "sequence_number": "1", "start": {"call_control_id": call_control_id, "media_format": {"encoding": "PCMU", "sample_rate": 8000, "channels": 1}}, "stream_id": "S1"}).to_string()))
        .await
        .expect("send");
    c
}

/// What the carrier got: media bytes and marks, read until the mark arrives (echoed back, as
/// Telnyx does when the audio before it was played).
async fn carrier_plays(c: &mut Carrier) -> usize {
    let mut bytes = 0;
    loop {
        let m = tokio::time::timeout(T, c.next())
            .await
            .expect("carrier frame in time")
            .expect("carrier open")
            .expect("carrier ok");
        let Message::Text(t) = m else { continue };
        let v: Value = serde_json::from_str(&t).expect("carrier json");
        match v["event"].as_str() {
            Some("media") => {
                bytes += base64::engine::general_purpose::STANDARD
                    .decode(v["media"]["payload"].as_str().expect("payload"))
                    .expect("base64")
                    .len();
            }
            Some("mark") => {
                c.send(Message::text(
                    json!({"event": "mark", "stream_id": "S1", "mark": v["mark"]}).to_string(),
                ))
                .await
                .expect("send");
                // Let the bridge take the mark before anyone speaks (speech during playback
                // is a barge-in, which is not what these tests are about).
                tokio::time::sleep(Duration::from_millis(60)).await;
                return bytes;
            }
            Some("clear") => panic!("the carrier was told to drop audio: a cut-off"),
            _ => {}
        }
    }
}

async fn carrier_closed(c: &mut Carrier) {
    loop {
        match tokio::time::timeout(T, c.next())
            .await
            .expect("close in time")
        {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
            Some(Ok(_)) => {}
        }
    }
}

struct V {
    t: TestApp,
    base: String,
    fake: Arc<RecordingNotifier>,
}

async fn setup(db: &TestDb, xai_url: &str) -> V {
    let t = TestApp::new(db, &[("XAI_REALTIME_URL", xai_url), ("XAI_API_KEY", KEY)]);
    let fake = Arc::new(RecordingNotifier::new());
    t.app
        .extensions
        .insert(Arc::new(OwnerNotify::new(fake.clone())));
    let vc = t.app.voice_calls();
    vc.set_launcher(Some(Arc::new(LiveBridgeLauncher {
        registry: vc.registry.clone(),
    })));
    vc.bind(&t.app);
    let base = serve(vc.registry.clone()).await;
    V { t, base, fake }
}

impl V {
    /// The texts the owner was sent (the call's summary among them).
    async fn texts(&self) -> Vec<String> {
        self.t.app.owner_notify().idle().await;
        self.fake.requests().into_iter().map(|r| r.text).collect()
    }
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
    /// Wait until the stored call satisfies `f`.
    async fn wait_call(
        &self,
        id: Uuid,
        what: &str,
        f: impl Fn(&CallSession) -> bool,
    ) -> CallSession {
        let end = tokio::time::Instant::now() + T;
        loop {
            let c = self.call(id).await;
            if f(&c) {
                return c;
            }
            assert!(
                tokio::time::Instant::now() < end,
                "{what} never happened; transcript: {:#?}",
                lines(&c)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn lines(c: &CallSession) -> Vec<String> {
    c.transcript
        .iter()
        .map(|t| format!("{:<12} {}", t.speaker, t.text))
        .collect()
}

/// The six questions of the second call. The order differs from that call's (there the server
/// pushed the amps question while the model was speaking) so that each sentence of the replay
/// answers the question put before it: the amps question comes fifth.
const QUESTIONS: [(&str, &str); 6] = [
    (
        "What is the usable square footage of the suite?",
        "usable_area_sqft",
    ),
    (
        "Is there existing plumbing inside the suite, like a sink or a floor drain?",
        "in_unit_plumbing",
    ),
    (
        "Could the HVAC be zoned so that separate small rooms each get supply and return air?",
        "hvac_zonable",
    ),
    (
        "When would the space be available to start a lease?",
        "available_from",
    ),
    (
        "How many amps is the electrical service to the suite's own panel, and roughly how much is in use?",
        "tenant_subpanel_amps",
    ),
    ("Is there a loading dock?", "loading_dock"),
];

/// An approved, answered, bridged call in the live phase: the opening line was played.
async fn live_call(v: &V, xai: &mut MockXai) -> (CallSession, Carrier, String) {
    let (token, owner) = v.t.dev_token().await;
    let p =
        v.t.create_project(
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
    recipient.insert("phone".to_owned(), "+15555550100".to_owned());
    let payload = ss_domain::communications::call_payload(pid, &recipient, None);
    let mut tx = v.t.app.store.begin().await.unwrap();
    let a =
        v.t.app
            .approvals()
            .request(
                &mut *tx,
                ApprovalSpec::new(
                    pid,
                    owner,
                    "t",
                    ActionType::PlaceCall,
                    payload,
                    "call",
                    format!("c-{}", Uuid::new_v4()),
                ),
            )
            .await
            .unwrap();
    v.t.app
        .approvals()
        .decide(
            &mut *tx,
            a.approval_id,
            owner,
            ss_server::contracts::ApprovalStatus::Approved,
            &a.action_payload_hash,
        )
        .await
        .unwrap();
    let mut call = CallOrchestrator::new(&v.t.app)
        .start(
            &mut *tx,
            StartCall {
                project_id: pid,
                owner_id: owner,
                recipient,
                property_id: None,
                approval_id: Some(a.approval_id),
                intro_recording_reference: None,
                communication_id: None,
                simulated: false,
            },
        )
        .await
        .unwrap();
    call.script = QUESTIONS
        .iter()
        .map(|(q, c)| CallQuestion {
            question: (*q).into(),
            claim_type: (*c).into(),
            requirement_id: None,
        })
        .collect();
    CALLS.upsert(&mut *tx, &call).await.unwrap();
    tx.commit().await.unwrap();

    let call = v.events(&call, vec![("call.answered", Map::new())]).await;
    assert_eq!(call.state, CallState::Active);
    let stream = v
        .commands()
        .iter()
        .find_map(|c| match c {
            TelephonyCommand::StreamingStart { stream, .. } => Some(stream.stream_url.clone()),
            _ => None,
        })
        .expect("streaming_start");
    let path = &stream[stream.find("/api/v1/voice/stream/").expect("path")..];
    let mut carrier = carrier_start(
        &format!("{}{path}", v.base),
        call.provider_call_id.as_deref().expect("leg"),
    )
    .await;

    let su = xai.next().await;
    assert_eq!(su["type"], "session.update");
    // 7a: xAI documents two turn-detection settings (`type`, `idle_timeout_ms`) and no
    // end-of-turn silence; the session asks for server VAD and nothing undocumented.
    assert_eq!(
        su["session"]["turn_detection"],
        json!({"type": "server_vad"})
    );
    let instructions = su["session"]["instructions"]
        .as_str()
        .expect("instructions")
        .to_owned();
    xai.send(json!({"type": "session.updated", "session": {}}));
    let fm = xai.next().await;
    assert_eq!(fm["item"]["type"], "force_message");
    assert_eq!(fm["item"]["interruptible"], false);
    let opening = fm["item"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    xai.respond(&opening);
    assert_eq!(carrier_plays(&mut carrier).await, 8000);
    // Live: the first direction (the questions' state: none asked) reaches the model.
    xai.until("the first direction", |v| v["item"]["role"] == "system")
        .await;
    assert!(
        xai.direction
            .starts_with("No approved question has been asked yet."),
        "{}",
        xai.direction
    );
    (call, carrier, instructions)
}

/// One counterparty sentence and the model's own reply to it, heard by the carrier in full.
/// Returns the reply. The server's direction for the next turn is awaited when the reply put a
/// question (that is what changes the direction).
#[allow(clippy::too_many_arguments)]
async fn turn(
    v: &V,
    xai: &mut MockXai,
    carrier: &mut Carrier,
    call: &CallSession,
    n: usize,
    instructions: &str,
    sentence: &str,
    ack: &str,
) -> String {
    let reply = xai.model_reply(instructions, sentence, ack);
    let before = xai.direction.clone();
    xai.heard(n, sentence);
    xai.respond(&reply);
    assert_eq!(
        carrier_plays(carrier).await,
        8000,
        "the reply is heard whole"
    );
    v.wait_call(call.call_id, "the reply is stored", |c| {
        c.transcript.iter().any(|t| t.text == reply)
    })
    .await;
    if reply.contains('?') {
        // The direction that follows the assistant's line: its question is the open one. (On
        // a real call it is in place seconds before the other side has answered.)
        xai.until("the direction after the assistant's line", |v| {
            v["item"]["role"] == "system"
                && v["item"]["content"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains(" is open"))
        })
        .await;
        assert_ne!(xai.direction, before);
    }
    reply
}

/// The counterparty's sentences of the second real call, in its order, each with the model's
/// acknowledgement of it on that call.
const SENTENCES: [(&str, &str); 8] = [
    (
        "Yes, so this, um, this property is well-maintained, has about three bedrooms, and is a very good location.",
        "Great to hear.",
    ),
    ("Do you have a moment that I can like look at, look this up?", ""),
    (
        "So, that I have to also get back to you. I do not know this off the top of my head.",
        "No problem at all.",
    ),
    (
        "Yes, there's two sinks and four floor drains.",
        "Two sinks and four floor drains, noted.",
    ),
    ("Uh, yes.", "Good, zoned HVAC is possible."),
    ("Uh, around mid-summer.", "Mid-summer, understood."),
    (
        "You, you, you just asked me that.",
        "You're right, sorry about that.",
    ),
    ("No.", "Okay, no loading dock."),
];

both_backends!(the_second_call_replayed_has_one_speaker_per_turn, |db| {
    let mut xai = MockXai::start().await;
    let v = setup(db, &xai.url).await;
    let (call, mut carrier, instructions) = live_call(&v, &mut xai).await;
    // The instructions number the six questions; the model leads from them.
    for (i, (q, _)) in QUESTIONS.iter().enumerate() {
        assert!(
            instructions.contains(&format!("{}. {q}", i + 1)),
            "{instructions}"
        );
    }

    // Sentences 1 to 7: each gets exactly one reply, the model's; the server says nothing.
    for (n, (sentence, ack)) in SENTENCES.iter().take(7).enumerate() {
        let before = v.call(call.call_id).await.transcript.len();
        turn(
            &v,
            &mut xai,
            &mut carrier,
            &call,
            n,
            &instructions,
            sentence,
            ack,
        )
        .await;
        let c = v
            .wait_call(call.call_id, "both lines stored", |c| {
                c.transcript.len() >= before + 2
            })
            .await;
        let new: Vec<&str> = c.transcript[before..]
            .iter()
            .map(|t| t.speaker.as_str())
            .collect();
        let mut sorted = new.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec!["assistant", "counterparty"],
            "sentence {n}: one line each, got {:?}",
            &lines(&c)[before..]
        );
        if n == 1 {
            // 7b: the request for time got one acknowledgement and nothing else: no
            // cannot-answer line, no next question, no follow-up for the owner, and the
            // question that was open stays the open one (7c).
            let said = &lines(&c)[before..];
            assert!(said
                .iter()
                .all(|l| !l.contains('?') || l.starts_with("counterparty")));
            assert!(
                c.deferred_questions.is_empty(),
                "{:?}",
                c.deferred_questions
            );
            assert!(
                xai.direction.contains("Question 1 is open"),
                "{}",
                xai.direction
            );
        }
    }
    // No force_message since the opening line: nothing was pushed while the model spoke.
    let forced = |x: &MockXai| {
        x.seen
            .iter()
            .filter(|v| v["item"]["type"] == "force_message")
            .count()
    };
    assert_eq!(forced(&xai), 1, "only the opening line so far");
    assert!(
        xai.direction.contains("No approved question is left"),
        "{}",
        xai.direction
    );

    // Sentence 8 answers the last question. The server closes: the closing line, verbatim and
    // not interruptible. The model's own reply to the same sentence is created after it (as
    // xAI would) and is not let through: one speaker on this turn too.
    xai.heard(7, SENTENCES[7].0);
    let fm = xai
        .until("the closing line", |v| v["item"]["type"] == "force_message")
        .await;
    assert_eq!(fm["item"]["interruptible"], false);
    let closing = closing_line(false);
    assert_eq!(fm["item"]["content"][0]["text"], closing.as_str());
    let closing_id = xai.respond(&closing);
    let late =
        xai.respond("Okay, no loading dock. When would the space be available to start a lease?");
    let cancel = xai
        .until("the late reply is cancelled", |v| {
            v["type"] == "response.cancel"
        })
        .await;
    assert_eq!(cancel["response_id"], late.as_str());
    assert_ne!(late, closing_id);
    // The carrier hears the closing line whole (8000 bytes, not 16000) and then the stream ends.
    assert_eq!(carrier_plays(&mut carrier).await, 8000);
    carrier_closed(&mut carrier).await;
    let c = v
        .wait_call(call.call_id, "the closed note", |c| {
            c.transcript.iter().any(|t| t.text == CLOSED_NOTE)
        })
        .await;
    assert_eq!(v.cmd_names().last().map(String::as_str), Some("hangup"));
    assert!(
        !v.cmd_names().iter().any(|n| n == "speak"),
        "{:?}",
        v.cmd_names()
    );
    let c = v.events(&c, vec![("call.hangup", Map::new())]).await;
    assert_eq!(c.state, CallState::Completed, "{:#?}", lines(&c));

    // The transcript: nothing after the closing line but the system note.
    let all = lines(&c);
    println!("REPLAY TRANSCRIPT\n{}", all.join("\n"));
    let at = c
        .transcript
        .iter()
        .position(|t| t.text == closing)
        .expect("closing line stored");
    assert!(
        c.transcript[at + 1..].iter().all(|t| t.speaker == "system"),
        "{all:#?}"
    );
    // Each of the six questions was put exactly once.
    for (q, _) in QUESTIONS {
        let n = c
            .transcript
            .iter()
            .filter(|t| t.speaker == "assistant" && t.text.contains(q))
            .count();
        assert_eq!(n, 1, "{q}: {all:#?}");
    }
    // One assistant line per counterparty line (8 and 8), plus the opening line.
    let count = |s: &str| c.transcript.iter().filter(|t| t.speaker == s).count();
    assert_eq!(
        (count("counterparty"), count("assistant")),
        (8, 9),
        "{all:#?}"
    );
    assert!(
        all.iter().all(|l| !l.contains("technical problem")),
        "{all:#?}"
    );
    assert!(
        all.iter().all(|l| !l.contains("I don't have that")),
        "{all:#?}"
    );

    // The summary: no technical problem, and no server line raised as a commitment.
    let texts = v.texts().await;
    println!("REPLAY SUMMARY\n{}", texts.join("\n"));
    assert!(!texts.is_empty());
    for t in &texts {
        assert!(!t.contains("technical problem"), "{t}");
        assert!(!t.contains("sounds like a commitment"), "{t}");
    }
});

// Item 5: the server's own lines are marked where they are written and are not raised as
// possible commitments; the model's own words are still judged.
both_backends!(the_summary_judges_the_models_words_not_the_servers, |db| {
    let mut xai = MockXai::start().await;
    let v = setup(db, &xai.url).await;
    let (call, mut carrier, instructions) = live_call(&v, &mut xai).await;
    // The other side asks something the call cannot answer. The server says nothing (on the
    // second call it pushed its cannot-answer line here); the MODEL answers, in words that
    // sound like a commitment, and puts the first question. The owner gets a follow-up.
    const MODEL_LINE: &str = "I don't have that; I can ask the owner and get back to you.";
    turn(
        &v,
        &mut xai,
        &mut carrier,
        &call,
        0,
        &instructions,
        "Who else is looking at the space?",
        MODEL_LINE,
    )
    .await;
    for n in 1..6 {
        turn(
            &v,
            &mut xai,
            &mut carrier,
            &call,
            n,
            &instructions,
            "I do not know that.",
            "Okay.",
        )
        .await;
    }
    // The last answer: the server closes, and its closing line says the owner will follow up.
    xai.heard(6, "I do not know that.");
    let fm = xai
        .until("the closing line", |v| v["item"]["type"] == "force_message")
        .await;
    let closing = closing_line(true);
    assert!(closing.contains("The owner will follow up directly on the other items."));
    assert_eq!(fm["item"]["content"][0]["text"], closing.as_str());
    xai.respond(&closing);
    carrier_plays(&mut carrier).await;
    let c = v
        .wait_call(call.call_id, "the closed note", |c| {
            c.transcript.iter().any(|t| t.text == CLOSED_NOTE)
        })
        .await;
    // The stored transcript keeps the contract's shape: the closing line is an assistant turn.
    assert!(c
        .transcript
        .iter()
        .any(|t| t.speaker == "assistant" && t.text == closing));
    assert!(!c.deferred_questions.is_empty());
    let c = v.events(&c, vec![("call.hangup", Map::new())]).await;
    assert_eq!(c.state, CallState::Completed);
    let texts = v.texts().await.join("\n");
    println!("MARKS SUMMARY\n{texts}");
    assert!(texts.contains("sounds like a commitment"), "{texts}");
    assert!(
        texts.contains("I can ask the owner and get back to you"),
        "{texts}"
    );
    assert!(!texts.contains("That's everything I needed"), "{texts}");
    assert!(
        !texts.contains("an automated system, calling on behalf"),
        "{texts}"
    );
});

both_backends!(
    a_hang_up_is_a_normal_end_and_a_provider_failure_still_apologises,
    |db| {
        // The other side hangs up mid-call: the carrier closes the stream.
        let mut xai = MockXai::start().await;
        let v = setup(db, &xai.url).await;
        let (call, mut carrier, instructions) = live_call(&v, &mut xai).await;
        turn(
            &v,
            &mut xai,
            &mut carrier,
            &call,
            0,
            &instructions,
            SENTENCES[0].0,
            SENTENCES[0].1,
        )
        .await;
        carrier
            .send(Message::text(json!({"event": "stop"}).to_string()))
            .await
            .unwrap();
        let c = v
            .wait_call(call.call_id, "the hang-up note", |c| {
                c.transcript.iter().any(|t| t.text == HUNG_UP_NOTE)
            })
            .await;
        assert_eq!(c.transcript.last().unwrap().text, HUNG_UP_NOTE);
        assert!(
            !v.cmd_names().iter().any(|n| n == "speak"),
            "{:?}",
            v.cmd_names()
        );
        let all = lines(&c);
        assert!(all.iter().all(|l| !l.contains(APOLOGY_LINE)), "{all:#?}");
        assert!(
            all.iter().all(|l| !l.contains("voice assistant ended")),
            "{all:#?}"
        );
        let c = v.events(&c, vec![("call.hangup", Map::new())]).await;
        assert_eq!(c.state, CallState::Completed);
        assert!(lines(&c).iter().all(|l| !l.contains("technical problem")));

        // The provider fails mid-call: the apology on the carrier, as before.
        let mut xai = MockXai::start().await;
        let v = setup(db, &xai.url).await;
        let (call, _carrier, _) = live_call(&v, &mut xai).await;
        let _ = xai.tx.send(None);
        let c = v
            .wait_call(call.call_id, "the apology", |c| {
                c.transcript.iter().any(|t| t.text == APOLOGY_LINE)
            })
            .await;
        assert!(lines(&c)
            .iter()
            .any(|l| l.contains("the voice assistant ended (provider_closed)")));
        assert!(v.cmd_names().iter().any(|n| n == "speak"));
    }
);

// ---------------------------------------------------------------- the bridge alone

struct Quiet(Mutex<Option<oneshot::Sender<BridgeEnd>>>);

#[async_trait::async_trait]
impl BridgeEvents for Quiet {
    async fn transcript(&self, _line: TranscriptLine) {}
    async fn tool_call(&self, _call: ToolCall) -> String {
        "{}".into()
    }
    async fn ended(&self, end: BridgeEnd) {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(end);
        }
    }
}

// 7b: after a long silence one gentle line, at most twice, and again only after the other side
// has spoken; then the existing idle rule ends the bridge.
#[tokio::test]
async fn a_long_silence_gets_the_still_here_line_at_most_twice() {
    let mut xai = MockXai::start().await;
    let registry = BridgeRegistry::new();
    let (etx, ended) = oneshot::channel();
    let (id, _handle) = registry.expect(CallBridgeSpec {
        call_control_id: "leg-1".into(),
        token: "tok_0123456789abcdef0123456789abcdef".into(),
        session: SessionSpec {
            instructions: "x".into(),
            voice: "eve".into(),
            tools: Vec::new(),
            input_transcription_model: None,
        },
        opening_line: None,
        xai: XaiConfig::new(&xai.url, KEY),
        options: BridgeOptions {
            idle_timeout: Duration::from_millis(1500),
            check_in: Some(CheckIn {
                after: Duration::from_millis(300),
                line: STILL_HERE_LINE.into(),
                max: 2,
            }),
            log: Arc::new(|_, _| {}),
            ..BridgeOptions::default()
        },
        events: Arc::new(Quiet(Mutex::new(Some(etx)))),
    });
    let base = serve(registry).await;
    let mut carrier = carrier_start(
        &format!("{base}/api/v1/voice/stream/{id}?token=tok_0123456789abcdef0123456789abcdef"),
        "leg-1",
    )
    .await;
    assert_eq!(xai.next().await["type"], "session.update");
    xai.send(json!({"type": "session.updated", "session": {}}));
    // The model asks; the other side says "hold on"; the model acknowledges once.
    xai.respond("What is the usable square footage of the suite?");
    carrier_plays(&mut carrier).await;
    xai.heard(1, "Hold on, let me check.");
    xai.respond("Of course, take your time.");
    carrier_plays(&mut carrier).await;
    // Silence: the line, twice, each after the bound and not before.
    for _ in 0..2 {
        let started = tokio::time::Instant::now();
        let fm = xai.next().await;
        assert!(started.elapsed() >= Duration::from_millis(200), "too early");
        assert_eq!(fm["item"]["type"], "force_message");
        assert_eq!(fm["item"]["content"][0]["text"], STILL_HERE_LINE);
        xai.respond(STILL_HERE_LINE);
        carrier_plays(&mut carrier).await;
    }
    // No third: the idle rule ends the bridge.
    let rest = xai.drain(Duration::from_millis(900)).await;
    assert!(
        rest.iter().all(|v| v["item"]["type"] != "force_message"),
        "{rest:?}"
    );
    let end = tokio::time::timeout(T, ended).await.unwrap().unwrap();
    assert_eq!(end.reason, EndReason::IdleTimeout);
    assert_eq!(end.stats.check_ins, 2);
    assert_eq!(end.stats.superseded, 0);
}

#[test]
fn a_request_for_time_is_told_from_a_question_and_an_answer() {
    for s in [
        "Do you have a moment that I can like look at, look this up?",
        "Hold on.",
        "Let me check that for you.",
        "One second.",
    ] {
        assert!(asks_for_time(s), "{s}");
    }
    for s in [
        "Uh, yes.",
        "No.",
        "What is this about?",
        "Around mid-summer.",
    ] {
        assert!(!asks_for_time(s), "{s}");
    }
}
