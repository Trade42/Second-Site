//! The voice bridge (`services::voice_bridge`) against a mock xAI realtime server and a mock
//! Telnyx media-stream client, both local WebSockets. No real call, no provider.

// The mock's handshake callback must return tungstenite's own (large) error response type.
#![allow(clippy::result_large_err)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use ss_server::integrations::xai_realtime::{FunctionTool, SessionSpec, XaiConfig};
use ss_server::services::voice_bridge::{
    accept_carrier, BridgeEnd, BridgeEvents, BridgeHandle, BridgeOptions, BridgeRegistry,
    CallBridgeSpec, EndReason, Speaker, ToolCall, TranscriptLine, NO_ANSWER_OUTPUT,
    UNKNOWN_TOOL_OUTPUT,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response as HsResponse};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const KEY: &str = "xai-test-key-7f3e9c1d5b2a4e6f8091";
const TOKEN: &str = "tok_4b9d2e7f1a6c3e8b0d5f9a2c7e1b4d6f";
const CALL: &str = "v2:call-control-expected-123";
const OPENING: &str = "This is an automated assistant calling for Dana's bakery.";
const T: Duration = Duration::from_secs(5);

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}
fn unb64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .expect("base64")
}

// ---------------------------------------------------------------- mock xAI

struct MockXai {
    url: String,
    auth: Arc<Mutex<Vec<(String, String)>>>,
    request_uri: Arc<Mutex<String>>,
    rx: mpsc::UnboundedReceiver<Value>,
    tx: mpsc::UnboundedSender<Option<Value>>,
    closed: oneshot::Receiver<()>,
    seen: Vec<Value>,
}

impl MockXai {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let auth = Arc::new(Mutex::new(Vec::new()));
        let request_uri = Arc::new(Mutex::new(String::new()));
        let (in_tx, rx) = mpsc::unbounded_channel();
        let (tx, mut out_rx) = mpsc::unbounded_channel::<Option<Value>>();
        let (closed_tx, closed) = oneshot::channel();
        let (a2, u2) = (auth.clone(), request_uri.clone());
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let cb = |req: &Request, resp: HsResponse| {
                let mut h = a2.lock().expect("lock");
                for (k, v) in req.headers() {
                    h.push((k.to_string(), v.to_str().unwrap_or("").to_string()));
                }
                *u2.lock().expect("lock") = req.uri().to_string();
                Ok(resp)
            };
            let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await else {
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
            let _ = closed_tx.send(());
        });
        Self {
            url: format!("ws://{addr}/v1/realtime?model=grok-voice-latest"),
            auth,
            request_uri,
            rx,
            tx,
            closed,
            seen: Vec::new(),
        }
    }

    fn send(&self, v: Value) {
        self.tx.send(Some(v)).expect("mock send");
    }
    fn send_raw(&self, s: &str) {
        // A frame that is not a valid event; sent as text.
        self.tx
            .send(Some(Value::String(s.to_string())))
            .expect("mock send");
    }

    /// The next client event that is not an audio append (appends are kept in `seen`).
    async fn next_non_audio(&mut self) -> Value {
        loop {
            let v = tokio::time::timeout(T, self.rx.recv())
                .await
                .expect("xai event in time")
                .expect("xai channel");
            self.seen.push(v.clone());
            if v["type"] != "input_audio_buffer.append" {
                return v;
            }
        }
    }

    /// Collect appends until `n` audio bytes arrived.
    async fn audio(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < n {
            let v = tokio::time::timeout(T, self.rx.recv())
                .await
                .expect("append in time")
                .expect("xai channel");
            self.seen.push(v.clone());
            assert_eq!(v["type"], "input_audio_buffer.append", "unexpected {v}");
            out.extend(unb64(v["audio"].as_str().expect("audio")));
        }
        out
    }

    async fn nothing_for(&mut self, d: Duration) {
        if let Ok(Some(v)) = tokio::time::timeout(d, self.rx.recv()).await {
            panic!("unexpected client event {v}");
        }
    }

    fn response(&self, id: &str, audio: &[u8], transcript: &str) {
        self.send(json!({"type": "response.created", "response": {"id": id, "status": "in_progress", "output": []}}));
        for c in audio.chunks(240) {
            self.send(json!({"type": "response.output_audio.delta", "response_id": id, "item_id": "it", "output_index": 0, "content_index": 0, "delta": b64(c)}));
        }
        self.send(json!({"type": "response.output_audio_transcript.done", "response_id": id, "item_id": "it", "transcript": transcript}));
        self.send(json!({"type": "response.done", "response": {"id": id, "status": "completed"}}));
    }
}

// ---------------------------------------------------------------- recorder

#[derive(Default)]
struct Rec {
    transcripts: Mutex<Vec<TranscriptLine>>,
    calls: Mutex<Vec<ToolCall>>,
    tool_delay: Mutex<Duration>,
    ended: Mutex<Option<oneshot::Sender<BridgeEnd>>>,
}

#[async_trait::async_trait]
impl BridgeEvents for Rec {
    async fn transcript(&self, line: TranscriptLine) {
        self.transcripts.lock().expect("lock").push(line);
    }
    async fn tool_call(&self, call: ToolCall) -> String {
        let d = *self.tool_delay.lock().expect("lock");
        let name = call.name.clone();
        self.calls.lock().expect("lock").push(call);
        tokio::time::sleep(d).await;
        json!({"answer": format!("orchestrator answer for {name}")}).to_string()
    }
    async fn ended(&self, end: BridgeEnd) {
        if let Some(tx) = self.ended.lock().expect("lock").take() {
            let _ = tx.send(end);
        }
    }
}

// ---------------------------------------------------------------- harness

type Carrier = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct H {
    xai: MockXai,
    carrier: Carrier,
    handle: BridgeHandle,
    rec: Arc<Rec>,
    ended: oneshot::Receiver<BridgeEnd>,
    logs: Arc<Mutex<Vec<String>>>,
    base: String,
    id: String,
}

#[derive(Clone)]
struct AppState(BridgeRegistry);

#[derive(serde::Deserialize)]
struct Tok {
    token: Option<String>,
}

async fn route(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<Tok>,
    ws: WebSocketUpgrade,
) -> Response {
    accept_carrier(&s.0, &id, q.token.as_deref(), ws)
}

fn session() -> SessionSpec {
    SessionSpec {
        instructions: "You are the client's automated assistant.".into(),
        voice: "eve".into(),
        tools: vec![
            FunctionTool {
                name: "record_finding".into(),
                description: "Record a fact the counterparty stated.".into(),
                parameters: json!({"type": "object", "properties": {"fact": {"type": "string"}}}),
            },
            FunctionTool {
                name: "consult_second_site".into(),
                description: "Ask Second Site for a substantive answer.".into(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        ],
        input_transcription_model: None,
    }
}

fn media(chunk: u64, audio: &[u8]) -> String {
    json!({"event": "media", "sequence_number": (chunk + 2).to_string(), "media": {"track": "inbound", "chunk": chunk.to_string(), "timestamp": (chunk * 20).to_string(), "payload": b64(audio)}, "stream_id": "S1"}).to_string()
}

async fn carrier_next(c: &mut Carrier) -> Value {
    loop {
        let m = tokio::time::timeout(T, c.next())
            .await
            .expect("carrier frame in time")
            .expect("carrier open")
            .expect("carrier ok");
        if let Message::Text(t) = m {
            return serde_json::from_str(&t).expect("carrier json");
        }
    }
}

/// Media bytes the carrier receives until `n` bytes, skipping marks.
async fn carrier_audio(c: &mut Carrier, n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < n {
        let v = carrier_next(c).await;
        match v["event"].as_str() {
            Some("media") => out.extend(unb64(v["media"]["payload"].as_str().expect("payload"))),
            Some("mark") => {}
            other => panic!("unexpected carrier event {other:?}: {v}"),
        }
    }
    out
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

async fn server(registry: BridgeRegistry) -> String {
    let app = Router::new()
        .route("/voice/stream/{id}", get(route))
        .with_state(AppState(registry));
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = l.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    format!("ws://{addr}/voice/stream")
}

async fn connect_carrier(url: &str) -> Result<Carrier, u16> {
    match tokio_tungstenite::connect_async(url).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("connect: {e}"),
    }
}

async fn start(c: &mut Carrier, call: &str) {
    c.send(Message::text(
        json!({"event": "connected", "version": "1.0.0"}).to_string(),
    ))
    .await
    .expect("send");
    c.send(Message::text(json!({"event": "start", "sequence_number": "1", "start": {"user_id": "u", "call_control_id": call, "call_session_id": "cs", "from": "+15550000001", "to": "+15550000002", "media_format": {"encoding": "PCMU", "sample_rate": 8000, "channels": 1}}, "stream_id": "S1"}).to_string()))
        .await
        .expect("send");
}

fn opts(logs: &Arc<Mutex<Vec<String>>>, f: impl FnOnce(&mut BridgeOptions)) -> BridgeOptions {
    let l = logs.clone();
    let mut o = BridgeOptions {
        tool_timeout: Duration::from_millis(400),
        idle_timeout: Duration::from_secs(30),
        log: Arc::new(move |msg, fields| {
            l.lock().expect("lock").push(ss_server::logging::render(
                ss_server::logging::Level::Info,
                "app.voice_bridge",
                msg,
                fields,
            ));
        }),
        ..BridgeOptions::default()
    };
    f(&mut o);
    o
}

/// Through the opening line: returns a harness in the live phase.
async fn live(opening: bool, f: impl FnOnce(&mut BridgeOptions)) -> H {
    let mut xai = MockXai::start().await;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let rec = Arc::new(Rec::default());
    let (etx, ended) = oneshot::channel();
    *rec.ended.lock().expect("lock") = Some(etx);
    let registry = BridgeRegistry::new();
    let (id, handle) = registry.expect(CallBridgeSpec {
        call_control_id: CALL.into(),
        token: TOKEN.into(),
        session: session(),
        opening_line: opening.then(|| OPENING.to_string()),
        xai: XaiConfig::new(&xai.url, KEY),
        options: opts(&logs, f),
        events: rec.clone(),
    });
    let base = server(registry).await;
    let mut carrier = connect_carrier(&format!("{base}/{id}?token={TOKEN}"))
        .await
        .expect("carrier accepted");
    start(&mut carrier, CALL).await;

    let su = xai.next_non_audio().await;
    assert_eq!(
        su,
        session().session_update(),
        "session.update is the one given"
    );
    assert_eq!(
        su["session"]["audio"]["input"]["format"]["type"],
        "audio/pcmu"
    );
    assert_eq!(
        su["session"]["audio"]["output"]["format"]["type"],
        "audio/pcmu"
    );
    // Caller audio before the session is ready is not forwarded.
    carrier
        .send(Message::text(media(1, &[1u8; 160])))
        .await
        .expect("send");
    // The two sockets are not ordered against each other: let that frame land first.
    tokio::time::sleep(Duration::from_millis(150)).await;
    xai.send(json!({"type": "session.updated", "session": {}}));
    if opening {
        let fm = xai.next_non_audio().await;
        assert_eq!(fm["type"], "conversation.item.create");
        assert_eq!(fm["item"]["type"], "force_message");
        assert_eq!(fm["item"]["interruptible"], false);
        assert_eq!(fm["item"]["content"][0]["text"], OPENING);
        // Caller audio during the opening is not forwarded either.
        carrier
            .send(Message::text(media(2, &[2u8; 160])))
            .await
            .expect("send");
        let open_audio: Vec<u8> = (0..320u32).map(|i| (i % 251) as u8).collect();
        xai.response("resp_open", &open_audio, OPENING);
        assert_eq!(carrier_audio(&mut carrier, 320).await, open_audio);
        let mark = carrier_next(&mut carrier).await;
        assert_eq!(mark["event"], "mark");
        carrier.send(Message::text(json!({"event": "mark", "stream_id": "S1", "sequence_number": "9", "mark": mark["mark"]}).to_string())).await.expect("send");
    }
    // Wait until the bridge is live: a probe frame reaches xAI.
    carrier
        .send(Message::text(media(3, &[3u8; 160])))
        .await
        .expect("send");
    let probe = xai.audio(160).await;
    assert!(probe == vec![3u8; 160], "only the probe, not gated audio");
    H {
        xai,
        carrier,
        handle,
        rec,
        ended,
        logs,
        base,
        id,
    }
}

async fn ended(h: &mut H) -> BridgeEnd {
    tokio::time::timeout(T, &mut h.ended)
        .await
        .expect("bridge ended in time")
        .expect("ended sent")
}

fn assert_clean_logs(h: &H, audio: &[&[u8]], texts: &[&str]) {
    let logs = h.logs.lock().expect("lock").join("\n");
    assert!(!logs.is_empty(), "the bridge logs its end");
    assert!(!logs.contains(KEY), "key in logs");
    assert!(!logs.contains(TOKEN), "token in logs");
    for a in audio {
        assert!(!logs.contains(&b64(a)), "audio in logs");
    }
    for t in texts {
        assert!(!logs.contains(t), "transcript text in logs: {t}");
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn relays_audio_both_ways_in_order_after_the_opening_line() {
    let mut h = live(true, |_| {}).await;
    // Key: only in the Authorization header of the handshake.
    let auth = h.xai.auth.lock().expect("lock").clone();
    let hits: Vec<_> = auth.iter().filter(|(_, v)| v.contains(KEY)).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "authorization");
    assert_eq!(hits[0].1, format!("Bearer {KEY}"));
    assert!(!h.xai.request_uri.lock().expect("lock").contains(KEY));

    // Carrier -> xAI, byte for byte and in order.
    let inbound: Vec<u8> = (0..1600u32).map(|i| (i * 7 % 256) as u8).collect();
    for (i, c) in inbound.chunks(160).enumerate() {
        h.carrier
            .send(Message::text(media(10 + i as u64, c)))
            .await
            .expect("send");
    }
    // The carrier's own outbound track is not sent back to the model.
    h.carrier
        .send(Message::text(json!({"event": "media", "media": {"track": "outbound", "chunk": "99", "payload": b64(&[9u8; 160])}}).to_string()))
        .await
        .expect("send");
    assert_eq!(h.xai.audio(1600).await, inbound);

    // xAI -> carrier, byte for byte and in order (deltas of 240 regrouped in 160-byte frames).
    let outbound: Vec<u8> = (0..1440u32).map(|i| (i * 13 % 256) as u8).collect();
    h.xai
        .response("resp_1", &outbound, "Thanks, what is the monthly rent?");
    assert_eq!(carrier_audio(&mut h.carrier, 1440).await, outbound);
    assert_eq!(carrier_next(&mut h.carrier).await["event"], "mark");

    // No free response was requested before the opening; no session.update twice.
    let kinds: Vec<String> = h
        .xai
        .seen
        .iter()
        .map(|v| v["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(kinds.iter().filter(|k| *k == "session.update").count(), 1);
    assert!(!kinds.iter().any(|k| k == "response.create"));

    h.handle.stop().await.expect("stop");
    let end = ended(&mut h).await;
    assert_eq!(end.reason, EndReason::Stopped);
    carrier_closed(&mut h.carrier).await;
    let _ = tokio::time::timeout(T, &mut h.xai.closed)
        .await
        .expect("xai closed");
    let tr = h.rec.transcripts.lock().expect("lock").clone();
    assert_eq!(tr.len(), 2);
    assert_eq!((tr[0].speaker, tr[0].turn_id), (Speaker::Assistant, 1));
    assert_eq!(tr[1].text, "Thanks, what is the monthly rent?");
    assert_eq!(tr[1].turn_id, 2);
    assert_clean_logs(
        &h,
        &[&inbound[..160], &outbound[..160]],
        &[OPENING, "monthly rent"],
    );
}

#[tokio::test]
async fn speech_during_playback_clears_the_carrier_and_cancels_the_response() {
    let mut h = live(true, |_| {}).await;
    h.xai.send(
        json!({"type": "response.created", "response": {"id": "resp_a", "status": "in_progress"}}),
    );
    h.xai.send(json!({"type": "response.output_audio.delta", "response_id": "resp_a", "delta": b64(&[5u8; 320])}));
    assert_eq!(carrier_audio(&mut h.carrier, 320).await, vec![5u8; 320]);
    h.xai.send(json!({"type": "input_audio_buffer.speech_started", "item_id": "msg_u1", "audio_start_ms": 100}));
    assert_eq!(
        carrier_next(&mut h.carrier).await,
        json!({"event": "clear"})
    );
    let cancel = h.xai.next_non_audio().await;
    assert_eq!(
        cancel,
        json!({"type": "response.cancel", "response_id": "resp_a"})
    );
    // Late audio of the cancelled response is dropped.
    h.xai.send(json!({"type": "response.output_audio.delta", "response_id": "resp_a", "delta": b64(&[6u8; 320])}));
    h.xai.send(
        json!({"type": "response.done", "response": {"id": "resp_a", "status": "cancelled"}}),
    );
    h.xai.send(json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": "msg_u1", "transcript": "Sorry, the rent is four thousand.", "content_index": 0}));
    h.xai
        .response("resp_b", &[7u8; 160], "Got it, four thousand a month.");
    assert_eq!(
        carrier_audio(&mut h.carrier, 160).await,
        vec![7u8; 160],
        "no cancelled audio"
    );

    h.handle.stop().await.expect("stop");
    let end = ended(&mut h).await;
    assert_eq!(end.stats.interruptions, 1);
    let tr = h.rec.transcripts.lock().expect("lock").clone();
    let turns: Vec<(Speaker, u64)> = tr.iter().map(|l| (l.speaker, l.turn_id)).collect();
    // opening = 1, resp_a = 2 (cancelled: no transcript), counterparty = 3, resp_b = 4.
    assert_eq!(
        turns,
        vec![
            (Speaker::Assistant, 1),
            (Speaker::Counterparty, 3),
            (Speaker::Assistant, 4)
        ]
    );
    assert_clean_logs(&h, &[&[5u8; 320]], &["four thousand"]);
}

#[tokio::test]
async fn tool_call_is_answered_by_the_orchestrator_and_a_slow_one_gets_the_fallback() {
    let mut h = live(false, |_| {}).await;
    h.xai.send(
        json!({"type": "response.created", "response": {"id": "resp_t", "status": "in_progress"}}),
    );
    h.xai.send(json!({"type": "response.function_call_arguments.done", "response_id": "resp_t", "item_id": "fc1", "output_index": 0, "call_id": "call_001", "name": "record_finding", "arguments": "{\"fact\":\"rent 4000\"}"}));
    h.xai.send(
        json!({"type": "response.done", "response": {"id": "resp_t", "status": "completed"}}),
    );
    let out = h.xai.next_non_audio().await;
    assert_eq!(out["item"]["type"], "function_call_output");
    assert_eq!(out["item"]["call_id"], "call_001");
    assert_eq!(
        out["item"]["output"],
        json!({"answer": "orchestrator answer for record_finding"}).to_string()
    );
    assert_eq!(
        h.xai.next_non_audio().await,
        json!({"type": "response.create"})
    );
    {
        let calls = h.rec.calls.lock().expect("lock");
        assert_eq!(calls[0].name, "record_finding");
        assert_eq!(calls[0].arguments, json!({"fact": "rent 4000"}));
    }

    // A slow orchestrator: the bounded fallback, never an invented answer.
    *h.rec.tool_delay.lock().expect("lock") = Duration::from_secs(3);
    h.xai.send(
        json!({"type": "response.created", "response": {"id": "resp_u", "status": "in_progress"}}),
    );
    h.xai.send(json!({"type": "response.function_call_arguments.done", "response_id": "resp_u", "call_id": "call_002", "name": "consult_second_site", "arguments": "{}"}));
    h.xai.send(
        json!({"type": "response.done", "response": {"id": "resp_u", "status": "completed"}}),
    );
    let out = h.xai.next_non_audio().await;
    assert_eq!(out["item"]["call_id"], "call_002");
    assert_eq!(out["item"]["output"], NO_ANSWER_OUTPUT);
    assert_eq!(
        h.xai.next_non_audio().await,
        json!({"type": "response.create"})
    );
    h.handle.stop().await.expect("stop");
    assert_eq!(ended(&mut h).await.stats.tool_timeouts, 1);
}

#[tokio::test]
async fn pause_stops_audio_both_ways_and_resume_restores_it() {
    let mut h = live(false, |_| {}).await;
    h.handle.pause().await.expect("pause");
    h.carrier
        .send(Message::text(media(20, &[8u8; 160])))
        .await
        .expect("send");
    h.xai.send(
        json!({"type": "response.created", "response": {"id": "resp_p", "status": "in_progress"}}),
    );
    // The model may not speak while paused: the bridge cancels it and drops its audio.
    assert_eq!(
        h.xai.next_non_audio().await,
        json!({"type": "response.cancel", "response_id": "resp_p"})
    );
    h.xai.send(json!({"type": "response.output_audio.delta", "response_id": "resp_p", "delta": b64(&[4u8; 160])}));
    h.xai.nothing_for(Duration::from_millis(300)).await;
    h.handle.resume().await.expect("resume");
    h.carrier
        .send(Message::text(media(21, &[9u8; 160])))
        .await
        .expect("send");
    assert_eq!(
        h.xai.audio(160).await,
        vec![9u8; 160],
        "paused audio was not sent"
    );
    h.xai.response("resp_r", &[1u8; 160], "Thanks for holding.");
    assert_eq!(
        carrier_audio(&mut h.carrier, 160).await,
        vec![1u8; 160],
        "paused audio was not played"
    );
    h.handle
        .say("The owner will follow up by email.")
        .await
        .expect("say");
    h.carrier
        .send(Message::text(
            json!({"event": "mark", "mark": {"name": "x"}}).to_string(),
        ))
        .await
        .expect("send");
    let fm = h.xai.next_non_audio().await;
    assert_eq!(fm["item"]["type"], "force_message");
    assert_eq!(
        fm["item"]["content"][0]["text"],
        "The owner will follow up by email."
    );
    h.handle.stop().await.expect("stop");
    ended(&mut h).await;
}

#[tokio::test]
async fn carrier_close_ends_the_bridge_and_closes_xai() {
    let mut h = live(false, |_| {}).await;
    h.carrier
        .send(Message::text(json!({"event": "stop", "sequence_number": "5", "stop": {"user_id": "u", "call_control_id": CALL}, "stream_id": "S1"}).to_string()))
        .await
        .expect("send");
    assert_eq!(ended(&mut h).await.reason, EndReason::CarrierClosed);
    tokio::time::timeout(T, &mut h.xai.closed)
        .await
        .expect("xai closed")
        .expect("closed");
}

#[tokio::test]
async fn provider_close_ends_the_bridge_and_closes_the_carrier() {
    let mut h = live(false, |_| {}).await;
    h.xai.tx.send(None).expect("close");
    assert_eq!(ended(&mut h).await.reason, EndReason::ProviderClosed);
    carrier_closed(&mut h.carrier).await;
}

#[tokio::test]
async fn idle_timeout_ends_the_bridge() {
    let mut h = live(false, |o| o.idle_timeout = Duration::from_millis(400)).await;
    assert_eq!(ended(&mut h).await.reason, EndReason::IdleTimeout);
    carrier_closed(&mut h.carrier).await;
}

#[tokio::test]
async fn wrong_token_second_connection_and_wrong_call_id_are_refused() {
    let mut h = live(false, |_| {}).await;
    let wrong = connect_carrier(&format!("{}/{}?token=nope", h.base, h.id))
        .await
        .err();
    assert_eq!(wrong, Some(401));
    let none = connect_carrier(&format!("{}/{}", h.base, h.id)).await.err();
    assert_eq!(none, Some(401));
    let unknown = connect_carrier(&format!("{}/unknown?token={TOKEN}", h.base))
        .await
        .err();
    assert_eq!(unknown, Some(401));
    let second = connect_carrier(&format!("{}/{}?token={TOKEN}", h.base, h.id))
        .await
        .err();
    assert_eq!(second, Some(409));
    h.handle.stop().await.expect("stop");
    ended(&mut h).await;

    // A `start` for another call: refused, socket closed, and xAI never contacted.
    let xai = MockXai::start().await;
    let rec = Arc::new(Rec::default());
    let (etx, erx) = oneshot::channel();
    *rec.ended.lock().expect("lock") = Some(etx);
    let registry = BridgeRegistry::new();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let (id, _handle) = registry.expect(CallBridgeSpec {
        call_control_id: CALL.into(),
        token: TOKEN.into(),
        session: session(),
        opening_line: None,
        xai: XaiConfig::new(&xai.url, KEY),
        options: opts(&logs, |_| {}),
        events: rec.clone(),
    });
    let base = server(registry).await;
    let mut c = connect_carrier(&format!("{base}/{id}?token={TOKEN}"))
        .await
        .expect("accepted");
    start(&mut c, "v2:someone-else").await;
    let end = tokio::time::timeout(T, erx)
        .await
        .expect("ended")
        .expect("sent");
    assert_eq!(
        end.reason,
        EndReason::Error {
            kind: "call_id_mismatch".into()
        }
    );
    carrier_closed(&mut c).await;
    assert!(
        xai.auth.lock().expect("lock").is_empty(),
        "xAI was not contacted"
    );
}

#[tokio::test]
async fn malformed_frames_are_counted_and_skipped() {
    let mut h = live(false, |_| {}).await;
    h.carrier
        .send(Message::text("{not json"))
        .await
        .expect("send");
    h.carrier
        .send(Message::text(
            json!({"event": "media", "media": {"payload": "%%%"}}).to_string(),
        ))
        .await
        .expect("send");
    h.carrier
        .send(Message::binary(vec![0u8, 1, 2]))
        .await
        .expect("send");
    h.xai.send_raw("not an event");
    h.xai
        .send(json!({"type": "response.output_audio.delta", "delta": "***"}));
    h.carrier
        .send(Message::text(media(30, &[2u8; 160])))
        .await
        .expect("send");
    assert_eq!(h.xai.audio(160).await, vec![2u8; 160]);
    h.xai.response("resp_m", &[3u8; 160], "ok");
    assert_eq!(carrier_audio(&mut h.carrier, 160).await, vec![3u8; 160]);
    h.handle.stop().await.expect("stop");
    let end = ended(&mut h).await;
    assert_eq!(end.stats.malformed_carrier, 3);
    assert_eq!(end.stats.malformed_provider, 2);
}

#[tokio::test]
async fn xai_connect_failure_ends_with_a_reason_and_no_secret() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let rec = Arc::new(Rec::default());
    let (etx, erx) = oneshot::channel();
    *rec.ended.lock().expect("lock") = Some(etx);
    let registry = BridgeRegistry::new();
    // Nothing listens on this port.
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let dead = format!("ws://{}/v1/realtime", l.local_addr().expect("addr"));
    drop(l);
    let (id, _h) = registry.expect(CallBridgeSpec {
        call_control_id: CALL.into(),
        token: TOKEN.into(),
        session: session(),
        opening_line: None,
        xai: XaiConfig::new(dead, KEY),
        options: opts(&logs, |_| {}),
        events: rec,
    });
    let base = server(registry).await;
    let mut c = connect_carrier(&format!("{base}/{id}?token={TOKEN}"))
        .await
        .expect("accepted");
    start(&mut c, CALL).await;
    let end = tokio::time::timeout(T, erx)
        .await
        .expect("ended")
        .expect("sent");
    assert_eq!(
        end.reason,
        EndReason::Error {
            kind: "provider_handshake".into()
        }
    );
    carrier_closed(&mut c).await;
    let all = logs.lock().expect("lock").join("\n");
    assert!(!all.contains(KEY) && !format!("{end:?}").contains(KEY));
}

// ---------------------------------------------------------------- review additions

#[tokio::test]
async fn an_empty_expected_token_admits_no_one() {
    let xai = MockXai::start().await;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let registry = BridgeRegistry::new();
    let (id, _handle) = registry.expect(CallBridgeSpec {
        call_control_id: CALL.into(),
        token: String::new(),
        session: session(),
        opening_line: None,
        xai: XaiConfig::new(&xai.url, KEY),
        options: opts(&logs, |_| {}),
        events: Arc::new(Rec::default()),
    });
    let base = server(registry).await;
    assert_eq!(connect_carrier(&format!("{base}/{id}")).await.err(), Some(401));
    assert_eq!(
        connect_carrier(&format!("{base}/{id}?token=")).await.err(),
        Some(401)
    );
}

#[tokio::test]
async fn a_cut_opening_line_ends_the_call_before_the_model_speaks() {
    let mut xai = MockXai::start().await;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let rec = Arc::new(Rec::default());
    let (etx, erx) = oneshot::channel();
    *rec.ended.lock().expect("lock") = Some(etx);
    let registry = BridgeRegistry::new();
    let (id, _handle) = registry.expect(CallBridgeSpec {
        call_control_id: CALL.into(),
        token: TOKEN.into(),
        session: session(),
        opening_line: Some(OPENING.into()),
        xai: XaiConfig::new(&xai.url, KEY),
        options: opts(&logs, |_| {}),
        events: rec.clone(),
    });
    let base = server(registry).await;
    let mut c = connect_carrier(&format!("{base}/{id}?token={TOKEN}"))
        .await
        .expect("accepted");
    start(&mut c, CALL).await;
    assert_eq!(xai.next_non_audio().await["type"], "session.update");
    xai.send(json!({"type": "session.updated", "session": {}}));
    assert_eq!(xai.next_non_audio().await["item"]["type"], "force_message");
    xai.send(json!({"type": "response.created", "response": {"id": "resp_open"}}));
    xai.send(json!({"type": "response.done", "response": {"id": "resp_open", "status": "failed"}}));
    let end = tokio::time::timeout(T, erx).await.expect("ended").expect("sent");
    assert_eq!(
        end.reason,
        EndReason::Error {
            kind: "opening_line_not_spoken".into()
        }
    );
    carrier_closed(&mut c).await;
}

#[tokio::test]
async fn an_undeclared_tool_is_refused_without_asking_the_orchestrator() {
    let mut h = live(false, |_| {}).await;
    h.xai.send(json!({"type": "response.created", "response": {"id": "resp_x"}}));
    h.xai.send(json!({"type": "response.function_call_arguments.done", "response_id": "resp_x", "call_id": "call_x", "name": "wire_money", "arguments": "{}"}));
    h.xai.send(json!({"type": "response.done", "response": {"id": "resp_x", "status": "completed"}}));
    let out = h.xai.next_non_audio().await;
    assert_eq!(out["item"]["call_id"], "call_x");
    assert_eq!(out["item"]["output"], UNKNOWN_TOOL_OUTPUT);
    assert_eq!(h.xai.next_non_audio().await, json!({"type": "response.create"}));
    assert!(h.rec.calls.lock().expect("lock").is_empty(), "orchestrator not asked");
    h.handle.stop().await.expect("stop");
    ended(&mut h).await;
}

#[tokio::test]
async fn a_tool_answer_while_paused_continues_only_after_resume() {
    let mut h = live(false, |_| {}).await;
    *h.rec.tool_delay.lock().expect("lock") = Duration::from_millis(200);
    h.xai.send(json!({"type": "response.created", "response": {"id": "resp_t"}}));
    h.xai.send(json!({"type": "response.function_call_arguments.done", "response_id": "resp_t", "call_id": "call_p", "name": "record_finding", "arguments": "{}"}));
    h.xai.send(json!({"type": "response.done", "response": {"id": "resp_t", "status": "completed"}}));
    // Pause only once the orchestrator holds the call (its answer takes 200 ms).
    tokio::time::timeout(T, async {
        while h.rec.calls.lock().expect("lock").is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("tool call reached the orchestrator");
    h.handle.pause().await.expect("pause");
    let out = h.xai.next_non_audio().await;
    assert_eq!(out["item"]["type"], "function_call_output");
    h.xai.nothing_for(Duration::from_millis(300)).await;
    h.handle.resume().await.expect("resume");
    assert_eq!(h.xai.next_non_audio().await, json!({"type": "response.create"}));
    h.handle.stop().await.expect("stop");
    ended(&mut h).await;
}
