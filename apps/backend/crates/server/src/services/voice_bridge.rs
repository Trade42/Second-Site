//! The real-time bridge between one Telnyx call's media stream and one Grok Voice (xAI
//! realtime) session.
//!
//! The carrier (Telnyx) opens a WebSocket to the stream URL the orchestrator put on the dial (or
//! on `streaming_start`); the bridge relays the call's inbound audio to xAI and xAI's audio back
//! to the call, both as `audio/pcmu` with no transcoding, and reports transcripts and tool calls
//! through [`BridgeEvents`]. The orchestrator stays the authority: tool calls are answered by it,
//! the bridge decides nothing.
//!
//! Carrier messages: <https://developers.telnyx.com/docs/voice/programmable-voice/media-streaming>
//! (`connected`, `start`, `media`, `stop`, `mark`, `dtmf`, `error` in; `media`, `clear`, `mark`
//! out). xAI messages: see [`crate::integrations::xai_realtime`].
//!
//! Lifecycle of one call:
//! 1. [`BridgeRegistry::expect`] (orchestrator, before the dial): registers the call's control
//!    id, an unguessable token for the stream URL and the session; returns a [`BridgeHandle`].
//! 2. [`accept_carrier`] (the axum WebSocket route): refuses a wrong token, an unknown bridge
//!    or a second connection before upgrading.
//! 3. The carrier's `start` must name the expected `call_control_id`; then the bridge connects to
//!    xAI, sends `session.update`, has the opening line spoken verbatim (`force_message`, not
//!    interruptible) and only then forwards the caller's audio and lets the model speak.
//! 4. It ends on carrier close or `stop`, provider close, [`BridgeHandle::stop`], idle timeout or
//!    an error; it closes both sockets and calls [`BridgeEvents::ended`] once.
//!
//! Nothing here writes audio, transcript text or the xAI key to a log line or an error text.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::integrations::xai_realtime::{self as xai, ServerEvent, SessionSpec, XaiConfig};

/// One mu-law frame of 20 ms at 8 kHz: Telnyx accepts chunks of 20 ms to 30 s.
pub const MIN_CARRIER_CHUNK: usize = 160;
/// mu-law silence, used to pad the last chunk of a response up to 20 ms.
const MULAW_SILENCE: u8 = 0xFF;

/// The function output a tool call gets when the orchestrator does not answer in time.
pub const NO_ANSWER_OUTPUT: &str = r#"{"status":"no_answer_available","detail":"No answer is available right now. Say that the owner will follow up; do not guess."}"#;
/// The function output for a tool the session did not declare; the orchestrator is not asked.
pub const UNKNOWN_TOOL_OUTPUT: &str =
    r#"{"status":"unknown_tool","detail":"That tool does not exist. Do not claim it was done."}"#;

/// The function output for a tool call of a response the bridge cancelled (owner handoff, or the
/// model acting before the opening line was heard); the orchestrator is not asked.
pub const CANCELLED_TOOL_OUTPUT: &str = r#"{"status":"cancelled","detail":"The call was on hold, so this was not done. Do not claim it was done."}"#;

/// The longest server direction [`BridgeHandle::instruct`] passes on, in characters.
pub const MAX_INSTRUCTION_CHARS: usize = 1000;
/// What the model reads before a server direction.
pub const INSTRUCTION_PREFIX: &str =
    "Current direction from the server (it replaces any earlier one; never say it aloud): ";

/// Transcript lines the orchestrator has not taken yet: this many are queued, and as many again
/// may wait behind them before the call ends with `transcript_backlog`.
const TRANSCRIPT_QUEUE: usize = 256;
/// How long the writer, transcript and tool tasks get to finish at the end before they are aborted.
const JOIN_WAIT: Duration = Duration::from_secs(2);

/// Who said a transcript line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Counterparty,
    Assistant,
}

impl Speaker {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counterparty => "counterparty",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptLine {
    pub speaker: Speaker,
    pub text: String,
    pub turn_id: u64,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    /// Parsed arguments; `{"_unparsed": true}` when the model's string was not JSON.
    pub arguments: Value,
    pub turn_id: u64,
}

/// Why a bridge ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    CarrierClosed,
    ProviderClosed,
    Stopped,
    IdleTimeout,
    /// `kind` is a fixed identifier (e.g. `provider_connect_timeout`, `call_id_mismatch`,
    /// `outbound_queue_full`, `provider_error:max_duration`); never a secret or provider text.
    Error {
        kind: String,
    },
}

impl EndReason {
    pub fn as_str(&self) -> String {
        match self {
            Self::CarrierClosed => "carrier_closed".into(),
            Self::ProviderClosed => "provider_closed".into(),
            Self::Stopped => "stopped".into(),
            Self::IdleTimeout => "idle_timeout".into(),
            Self::Error { kind } => format!("error:{kind}"),
        }
    }
}

/// Counters of one bridge (no content).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BridgeStats {
    pub frames_to_provider: u64,
    pub frames_to_carrier: u64,
    pub malformed_carrier: u64,
    pub malformed_provider: u64,
    pub dropped_late_chunks: u64,
    pub dropped_gated: u64,
    pub interruptions: u64,
    pub tool_calls: u64,
    pub tool_timeouts: u64,
    pub provider_errors: u64,
    /// Transcript lines the bridge produced that [`BridgeEvents::transcript`] never finished
    /// taking (the orchestrator was too slow, or was still busy when the bridge ended).
    pub transcripts_undelivered: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeEnd {
    pub reason: EndReason,
    pub stats: BridgeStats,
}

/// What the call orchestrator implements to receive one bridge's events.
///
/// `transcript` calls arrive in order on one task; `tool_call` runs on its own task per call and
/// its answer (a string, usually JSON) is returned to the model as the function output, unless
/// it takes longer than [`BridgeOptions::tool_timeout`], in which case the model gets
/// [`NO_ANSWER_OUTPUT`]. `ended` is called exactly once, last.
#[async_trait::async_trait]
pub trait BridgeEvents: Send + Sync + 'static {
    async fn transcript(&self, line: TranscriptLine);
    async fn tool_call(&self, call: ToolCall) -> String;
    async fn ended(&self, end: BridgeEnd);
}

/// A metadata-only log sink (`logger`, message, fields). The default writes through
/// [`crate::logging`]; tests capture it.
pub type LogSink = Arc<dyn Fn(&str, &[(&str, Value)]) + Send + Sync>;

#[derive(Clone)]
pub struct BridgeOptions {
    /// From upgrade to the carrier's `start`, and from `session.update` to `session.updated`.
    pub start_timeout: Duration,
    pub tool_timeout: Duration,
    /// No speech, response or control activity for this long ends the bridge (suspended while
    /// paused).
    pub idle_timeout: Duration,
    pub max_carrier_frame: usize,
    /// Capacity of each socket's outbound queue; a full queue ends the bridge.
    pub outbound_queue: usize,
    /// How long a transcript line may wait for a slow orchestrator once its queue is full; then
    /// the bridge ends with `transcript_backlog` (no line is dropped unseen while the call runs).
    pub transcript_wait: Duration,
    pub log: LogSink,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            start_timeout: Duration::from_secs(10),
            tool_timeout: Duration::from_secs(8),
            idle_timeout: Duration::from_secs(60),
            max_carrier_frame: 64 * 1024,
            outbound_queue: 512,
            transcript_wait: Duration::from_secs(5),
            log: Arc::new(|msg, fields| crate::logging::info("app.voice_bridge", msg, fields)),
        }
    }
}

/// Everything one call's bridge needs.
#[derive(Clone)]
pub struct CallBridgeSpec {
    /// Telnyx `call_control_id` the carrier's `start` must carry.
    pub call_control_id: String,
    /// The secret the stream URL carries (`?token=`); at least 32 random characters.
    pub token: String,
    pub session: SessionSpec,
    /// Spoken verbatim before the model may speak (the automation disclosure).
    pub opening_line: Option<String>,
    pub xai: XaiConfig,
    pub options: BridgeOptions,
    pub events: Arc<dyn BridgeEvents>,
}

enum Control {
    Say(String),
    Instruct(String),
    Pause,
    Resume,
    Stop,
}

/// The orchestrator's control of one bridge. Calls fail with [`BridgeGone`] after it ended.
#[derive(Clone)]
pub struct BridgeHandle {
    tx: mpsc::Sender<Control>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeGone;

impl BridgeHandle {
    /// Speak `text` verbatim (queued until no response is in flight).
    pub async fn say(&self, text: impl Into<String>) -> Result<(), BridgeGone> {
        self.send(Control::Say(text.into())).await
    }
    /// Give the model a server direction for its next turns. Nothing is spoken and no response is
    /// started: it is a system message item in the conversation, replacing the previous
    /// direction. At most [`MAX_INSTRUCTION_CHARS`] characters, control characters removed. It
    /// waits until the opening line has been heard and while paused (only the latest is kept),
    /// and is dropped when the bridge ends. The text is never logged.
    pub async fn instruct(&self, text: impl Into<String>) -> Result<(), BridgeGone> {
        let text: String = text.into();
        let clean: String = text
            .chars()
            .filter_map(|c| match c {
                '\n' | '\r' | '\t' | '\u{2028}' | '\u{2029}' => Some(' '),
                c if c.is_control() => None,
                // Invisible format characters (zero width, direction overrides, the byte order mark).
                '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}' => None,
                c => Some(c),
            })
            .take(MAX_INSTRUCTION_CHARS)
            .collect();
        if clean.trim().is_empty() {
            return if self.tx.is_closed() {
                Err(BridgeGone)
            } else {
                Ok(())
            };
        }
        self.send(Control::Instruct(clean)).await
    }
    /// Owner handoff: stop relaying in both directions and stop the assistant talking.
    pub async fn pause(&self) -> Result<(), BridgeGone> {
        self.send(Control::Pause).await
    }
    pub async fn resume(&self) -> Result<(), BridgeGone> {
        self.send(Control::Resume).await
    }
    pub async fn stop(&self) -> Result<(), BridgeGone> {
        self.send(Control::Stop).await
    }
    async fn send(&self, c: Control) -> Result<(), BridgeGone> {
        self.tx.send(c).await.map_err(|_| BridgeGone)
    }
}

struct Pending {
    spec: CallBridgeSpec,
    control: mpsc::Receiver<Control>,
}

enum Slot {
    Waiting(Box<Pending>),
    /// Keeps the token, so a caller without it learns nothing about the bridge.
    Connected {
        token: String,
    },
}

/// The bridges this process expects or runs, by bridge id (the stream URL's path segment).
#[derive(Clone, Default)]
pub struct BridgeRegistry {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

/// Why a carrier connection was refused before the upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Unknown bridge id or wrong token (one answer for both).
    Unauthorized,
    /// The bridge already has a carrier connection.
    AlreadyConnected,
}

impl Refusal {
    pub fn status(self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::AlreadyConnected => StatusCode::CONFLICT,
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl BridgeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a call. Returns the bridge id for the stream URL
    /// (`wss://<host>/<route>/{bridge_id}?token=<spec.token>`) and the control handle.
    pub fn expect(&self, spec: CallBridgeSpec) -> (String, BridgeHandle) {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (tx, rx) = mpsc::channel(32);
        let pending = Pending { spec, control: rx };
        if let Ok(mut m) = self.slots.lock() {
            m.insert(id.clone(), Slot::Waiting(Box::new(pending)));
        }
        (id, BridgeHandle { tx })
    }

    /// Forget a bridge (the call ended or was never placed).
    pub fn remove(&self, bridge_id: &str) {
        if let Ok(mut m) = self.slots.lock() {
            m.remove(bridge_id);
        }
    }

    /// A bridge whose carrier connection was admitted and never became a socket: the slot goes,
    /// so the id answers like an unknown one instead of 409 for ever.
    fn release_connected(&self, bridge_id: &str) {
        if let Ok(mut m) = self.slots.lock() {
            if matches!(m.get(bridge_id), Some(Slot::Connected { .. })) {
                m.remove(bridge_id);
            }
        }
    }

    fn admit(&self, bridge_id: &str, token: Option<&str>) -> Result<Box<Pending>, Refusal> {
        let mut m = self.slots.lock().map_err(|_| Refusal::Unauthorized)?;
        let presented = token.unwrap_or("");
        let expected = match m.get(bridge_id) {
            None => return Err(Refusal::Unauthorized),
            Some(Slot::Connected { token }) => token.as_str(),
            Some(Slot::Waiting(p)) => p.spec.token.as_str(),
        };
        // An empty expected token would admit a connection that presents none.
        if expected.is_empty() || !constant_time_eq(expected.as_bytes(), presented.as_bytes()) {
            return Err(Refusal::Unauthorized);
        }
        let token = expected.to_owned();
        match m.insert(bridge_id.to_owned(), Slot::Connected { token }) {
            Some(Slot::Waiting(p)) => Ok(p),
            _ => Err(Refusal::AlreadyConnected),
        }
    }
}

/// The axum WebSocket route's body: `bridge_id` from the path, `token` from the `token` query
/// parameter. Refuses (401/409, no upgrade) or upgrades and runs the bridge to its end.
pub fn accept_carrier(
    registry: &BridgeRegistry,
    bridge_id: &str,
    token: Option<&str>,
    ws: WebSocketUpgrade,
) -> Response {
    match registry.admit(bridge_id, token) {
        Err(r) => {
            let log = BridgeOptions::default().log;
            log(
                "voice bridge connection refused",
                &[("refusal", json!(format!("{r:?}")))],
            );
            r.status().into_response()
        }
        Ok(p) => {
            let max = p.spec.options.max_carrier_frame;
            let wait = p.spec.options.start_timeout;
            // Exactly one of the two takes the pending bridge: the upgraded socket, or the
            // watchdog when the upgrade fails (axum drops the callback) or does not complete in
            // `start_timeout`.
            let cell = Arc::new(Mutex::new(Some(p)));
            let (started_tx, started_rx) = oneshot::channel::<()>();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let (cell, registry, id) = (cell.clone(), registry.clone(), bridge_id.to_owned());
                rt.spawn(async move {
                    if matches!(tokio::time::timeout(wait, started_rx).await, Ok(Ok(()))) {
                        return;
                    }
                    let p = cell.lock().ok().and_then(|mut c| c.take());
                    if let Some(p) = p {
                        upgrade_failed(*p, &registry, &id).await;
                    }
                });
            }
            ws.max_message_size(max)
                .max_frame_size(max)
                .on_upgrade(move |socket| async move {
                    let p = cell.lock().ok().and_then(|mut c| c.take());
                    let _ = started_tx.send(());
                    if let Some(p) = p {
                        run(*p, socket).await;
                    }
                })
        }
    }
}

/// The carrier's request was admitted and no socket came of it: report the end once, let the
/// handle answer [`BridgeGone`] and free the slot.
async fn upgrade_failed(p: Pending, registry: &BridgeRegistry, bridge_id: &str) {
    let Pending { spec, control } = p;
    drop(control);
    registry.release_connected(bridge_id);
    let reason = EndReason::Error {
        kind: "upgrade_failed".into(),
    };
    (spec.options.log)("voice bridge ended", &[("reason", json!(reason.as_str()))]);
    spec.events
        .ended(BridgeEnd {
            reason,
            stats: BridgeStats::default(),
        })
        .await;
}

enum Phase {
    AwaitSession,
    /// The opening line is being spoken; `None` until its `response.created`.
    Opening(Option<String>),
    Live,
}

struct Active {
    id: String,
}

enum CarrierOut {
    Text(String),
    Close,
}

struct State {
    phase: Phase,
    paused: bool,
    turn: u64,
    active: Option<Active>,
    /// A `response.create` was sent and its `response.created` has not arrived.
    requested: bool,
    cancelled: HashSet<String>,
    /// Responses the bridge itself cancelled because the model may not act (paused, or before
    /// the opening line): their tool calls do not reach the orchestrator.
    gated: HashSet<String>,
    /// The latest server direction not yet sent (waits for the live phase and for `resume`).
    instruction: Option<String>,
    /// The item id of the direction now in the conversation.
    instruction_item: Option<String>,
    instruction_seq: u64,
    /// Transcript lines waiting for room in the orchestrator's queue, with when each began to wait.
    backlog: VecDeque<(Instant, TranscriptLine)>,
    transcripts_produced: u64,
    playing: bool,
    mark_seq: u64,
    last_mark: Option<String>,
    out_buf: Vec<u8>,
    pending_calls: HashSet<String>,
    continue_after_done: bool,
    say_queue: VecDeque<String>,
    item_turn: HashMap<String, u64>,
    resp_turn: HashMap<String, u64>,
    last_chunk: Option<u64>,
    stats: BridgeStats,
}

fn num(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

async fn run(p: Pending, socket: WebSocket) {
    let Pending { spec, control } = p;
    let events = spec.events.clone();
    let log = spec.options.log.clone();
    let (mut end, tail) = run_inner(spec, control, socket).await;
    let delivered = tail.await;
    end.stats.transcripts_undelivered = end.stats.transcripts_undelivered.saturating_sub(delivered);
    log(
        "voice bridge ended",
        &[
            ("reason", json!(end.reason.as_str())),
            ("frames_to_provider", json!(end.stats.frames_to_provider)),
            ("frames_to_carrier", json!(end.stats.frames_to_carrier)),
            ("malformed_carrier", json!(end.stats.malformed_carrier)),
            ("malformed_provider", json!(end.stats.malformed_provider)),
            ("interruptions", json!(end.stats.interruptions)),
            ("tool_timeouts", json!(end.stats.tool_timeouts)),
            (
                "transcripts_undelivered",
                json!(end.stats.transcripts_undelivered),
            ),
        ],
    );
    events.ended(end).await;
}

/// Resolves when every task of the bridge is gone, with the number of transcript lines the
/// orchestrator finished taking.
type Done = std::pin::Pin<Box<dyn std::future::Future<Output = u64> + Send>>;

fn finish(reason: EndReason, stats: BridgeStats, c_tx: mpsc::Sender<CarrierOut>) -> BridgeEnd {
    let _ = c_tx.try_send(CarrierOut::Close);
    BridgeEnd { reason, stats }
}

async fn run_inner(
    spec: CallBridgeSpec,
    mut control: mpsc::Receiver<Control>,
    socket: WebSocket,
) -> (BridgeEnd, Done) {
    let opts = spec.options.clone();
    let log = opts.log.clone();
    let (mut c_sink, mut c_stream) = socket.split();

    // Transcripts go to the orchestrator in order on one task.
    let delivered = Arc::new(AtomicU64::new(0));
    let (t_tx, mut t_rx) = mpsc::channel::<TranscriptLine>(TRANSCRIPT_QUEUE);
    let ev = spec.events.clone();
    let taken = delivered.clone();
    let t_task = tokio::spawn(async move {
        while let Some(line) = t_rx.recv().await {
            ev.transcript(line).await;
            taken.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Carrier writer.
    let (c_tx, mut c_rx) = mpsc::channel::<CarrierOut>(opts.outbound_queue);
    let c_task = tokio::spawn(async move {
        while let Some(m) = c_rx.recv().await {
            match m {
                CarrierOut::Text(t) => {
                    if c_sink.send(AxMessage::Text(t.into())).await.is_err() {
                        break;
                    }
                }
                CarrierOut::Close => {
                    let _ = c_sink
                        .send(AxMessage::Close(Some(CloseFrame {
                            code: 1000,
                            reason: "".into(),
                        })))
                        .await;
                    break;
                }
            }
        }
        let _ = c_sink.close().await;
    });

    let mut stats = BridgeStats::default();

    // 1. The carrier's `start`, naming the expected call.
    let start = tokio::time::timeout(opts.start_timeout, async {
        while let Some(msg) = c_stream.next().await {
            let text = match msg {
                Ok(AxMessage::Text(t)) => t.to_string(),
                Ok(AxMessage::Close(_)) | Err(_) => return Err(EndReason::CarrierClosed),
                Ok(_) => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                stats.malformed_carrier += 1;
                continue;
            };
            match v.get("event").and_then(Value::as_str) {
                Some("start") => {
                    let id = v
                        .pointer("/start/call_control_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if constant_time_eq(id.as_bytes(), spec.call_control_id.as_bytes()) {
                        return Ok(());
                    }
                    return Err(EndReason::Error {
                        kind: "call_id_mismatch".into(),
                    });
                }
                Some("stop") => return Err(EndReason::CarrierClosed),
                _ => continue,
            }
        }
        Err(EndReason::CarrierClosed)
    })
    .await;
    let start = start.unwrap_or(Err(EndReason::Error {
        kind: "carrier_start_timeout".into(),
    }));
    if let Err(reason) = start {
        let end = finish(reason, stats, c_tx);
        return (end, Tail::early(c_task, t_task, t_tx, delivered).join());
    }

    // 2. xAI.
    let xai_ws = match xai::connect(&spec.xai).await {
        Ok(ws) => ws,
        Err(e) => {
            let kind = match e {
                xai::XaiConnectError::Timeout => "provider_connect_timeout".to_string(),
                xai::XaiConnectError::Request => "provider_request".to_string(),
                xai::XaiConnectError::Handshake { status: Some(s) } => {
                    format!("provider_handshake_{s}")
                }
                xai::XaiConnectError::Handshake { status: None } => {
                    "provider_handshake".to_string()
                }
            };
            let end = finish(EndReason::Error { kind }, stats, c_tx);
            return (end, Tail::early(c_task, t_task, t_tx, delivered).join());
        }
    };
    let (mut x_sink, mut x_stream) = xai_ws.split();
    let (x_tx, mut x_rx) = mpsc::channel::<Option<String>>(opts.outbound_queue);
    let x_task = tokio::spawn(async move {
        while let Some(m) = x_rx.recv().await {
            match m {
                Some(t) => {
                    if x_sink.send(WsMessage::text(t)).await.is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
        let _ = x_sink.close().await;
    });

    let (tool_tx, mut tool_rx) = mpsc::channel::<(String, Option<String>)>(32);
    // Tool calls in flight: aborted when the bridge ends, so none runs on after `ended`.
    let mut tools: JoinSet<()> = JoinSet::new();
    let mut st = State {
        phase: Phase::AwaitSession,
        paused: false,
        turn: 0,
        active: None,
        requested: false,
        cancelled: HashSet::new(),
        gated: HashSet::new(),
        instruction: None,
        instruction_item: None,
        instruction_seq: 0,
        backlog: VecDeque::new(),
        transcripts_produced: 0,
        playing: false,
        mark_seq: 0,
        last_mark: None,
        out_buf: Vec::new(),
        pending_calls: HashSet::new(),
        continue_after_done: false,
        say_queue: VecDeque::new(),
        item_turn: HashMap::new(),
        resp_turn: HashMap::new(),
        last_chunk: None,
        stats,
    };

    macro_rules! xsend {
        ($v:expr) => {
            if let Err(e) = x_tx.try_send(Some($v.to_string())) {
                break match e {
                    mpsc::error::TrySendError::Full(_) => EndReason::Error {
                        kind: "outbound_queue_full".into(),
                    },
                    mpsc::error::TrySendError::Closed(_) => EndReason::ProviderClosed,
                };
            }
        };
    }
    macro_rules! csend {
        ($v:expr) => {
            if let Err(e) = c_tx.try_send(CarrierOut::Text($v.to_string())) {
                break match e {
                    mpsc::error::TrySendError::Full(_) => EndReason::Error {
                        kind: "outbound_queue_full".into(),
                    },
                    mpsc::error::TrySendError::Closed(_) => EndReason::CarrierClosed,
                };
            }
        };
    }

    // A transcript line for the orchestrator: queued, or waiting in order behind a full queue.
    macro_rules! tline {
        ($line:expr) => {{
            let line: TranscriptLine = $line;
            st.transcripts_produced += 1;
            if !st.backlog.is_empty() {
                st.backlog.push_back((Instant::now(), line));
            } else {
                match t_tx.try_send(line) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(line)) => {
                        st.backlog.push_back((Instant::now(), line));
                    }
                    // The orchestrator's transcript task is gone (its handler panicked): nobody
                    // would see this line or any later one, so the call ends.
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        break EndReason::Error {
                            kind: "transcript_consumer_gone".into(),
                        };
                    }
                }
            }
            if st.backlog.len() > TRANSCRIPT_QUEUE {
                break EndReason::Error {
                    kind: "transcript_backlog".into(),
                };
            }
        }};
    }

    let session_deadline = Instant::now() + opts.start_timeout;
    let mut idle_deadline = Instant::now() + opts.idle_timeout;
    let b64 = base64::engine::general_purpose::STANDARD;

    let reason: EndReason = 'outer: {
        if x_tx
            .try_send(Some(spec.session.session_update().to_string()))
            .is_err()
        {
            break 'outer EndReason::ProviderClosed;
        }
        loop {
            let awaiting_session = matches!(st.phase, Phase::AwaitSession);
            tokio::select! {
                biased;
                c = control.recv() => {
                    match c {
                        None | Some(Control::Stop) => break EndReason::Stopped,
                        Some(Control::Pause) => {
                            st.paused = true;
                            st.out_buf.clear();
                            if st.playing {
                                csend!(json!({"event": "clear"}));
                                st.playing = false;
                            }
                            if let Some(a) = st.active.take() {
                                xsend!(xai::response_cancel(Some(&a.id)));
                                st.gated.insert(a.id.clone());
                                st.cancelled.insert(a.id);
                            }
                        }
                        Some(Control::Instruct(text)) => {
                            idle_deadline = Instant::now() + opts.idle_timeout;
                            st.instruction = Some(text);
                        }
                        Some(Control::Resume) => {
                            st.paused = false;
                            idle_deadline = Instant::now() + opts.idle_timeout;
                        }
                        Some(Control::Say(text)) => {
                            idle_deadline = Instant::now() + opts.idle_timeout;
                            st.say_queue.push_back(text);
                        }
                    }
                }
                _ = tokio::time::sleep_until(st.backlog.front().map_or(idle_deadline, |(since, _)| *since + opts.transcript_wait)), if !st.backlog.is_empty() => {
                    break EndReason::Error { kind: "transcript_backlog".into() };
                }
                permit = t_tx.reserve(), if !st.backlog.is_empty() => {
                    let Ok(permit) = permit else {
                        break EndReason::Error { kind: "transcript_consumer_gone".into() };
                    };
                    if let Some((_, line)) = st.backlog.pop_front() {
                        permit.send(line);
                    }
                }
                a = tool_rx.recv() => {
                    if let Some((call_id, output)) = a {
                        let output = output.unwrap_or_else(|| {
                            st.stats.tool_timeouts += 1;
                            NO_ANSWER_OUTPUT.to_string()
                        });
                        idle_deadline = Instant::now() + opts.idle_timeout;
                        xsend!(xai::function_output(&call_id, &output));
                        st.pending_calls.remove(&call_id);
                        if st.pending_calls.is_empty() {
                            st.continue_after_done = true;
                        }
                    }
                }
                m = x_stream.next() => {
                    let text = match m {
                        None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => break EndReason::ProviderClosed,
                        Some(Ok(WsMessage::Text(t))) => t.to_string(),
                        Some(Ok(_)) => continue,
                    };
                    let Ok(ev) = xai::parse_server_event(&text) else {
                        st.stats.malformed_provider += 1;
                        continue;
                    };
                    match ev {
                        ServerEvent::SessionUpdated => {
                            if awaiting_session {
                                idle_deadline = Instant::now() + opts.idle_timeout;
                                match &spec.opening_line {
                                    Some(line) => {
                                        xsend!(xai::force_message(line, false));
                                        st.phase = Phase::Opening(None);
                                    }
                                    None => st.phase = Phase::Live,
                                }
                            }
                        }
                        ServerEvent::ResponseCreated { id } => {
                            idle_deadline = Instant::now() + opts.idle_timeout;
                            st.requested = false;
                            st.turn += 1;
                            st.resp_turn.insert(id.clone(), st.turn);
                            if let Phase::Opening(op @ None) = &mut st.phase {
                                *op = Some(id.clone());
                            } else if !matches!(st.phase, Phase::Live) || st.paused {
                                // The model may not speak before the opening line or while paused.
                                xsend!(xai::response_cancel(Some(&id)));
                                st.gated.insert(id.clone());
                                st.cancelled.insert(id);
                                continue;
                            }
                            if let Some(old) = st.active.replace(Active { id: id.clone() }) {
                                // Never two responses in flight: the older one goes.
                                xsend!(xai::response_cancel(Some(&old.id)));
                                st.cancelled.insert(old.id);
                            }
                        }
                        ServerEvent::AudioDelta { response_id, audio } => {
                            let rid = response_id.unwrap_or_default();
                            let allowed = match &st.phase {
                                Phase::Opening(Some(op)) => *op == rid,
                                Phase::Live => !st.paused,
                                _ => false,
                            };
                            if !allowed || st.cancelled.contains(&rid) {
                                st.stats.dropped_gated += 1;
                                continue;
                            }
                            st.out_buf.extend_from_slice(&audio);
                            let n = st.out_buf.len() / MIN_CARRIER_CHUNK * MIN_CARRIER_CHUNK;
                            if n > 0 {
                                let chunk: Vec<u8> = st.out_buf.drain(..n).collect();
                                csend!(json!({"event": "media", "media": {"payload": b64.encode(&chunk)}}));
                                st.stats.frames_to_carrier += 1;
                                st.playing = true;
                            }
                        }
                        ServerEvent::ResponseDone { id, status } => {
                            idle_deadline = Instant::now() + opts.idle_timeout;
                            let id = id.unwrap_or_default();
                            if !st.cancelled.contains(&id) && !st.out_buf.is_empty() {
                                let mut chunk = std::mem::take(&mut st.out_buf);
                                if chunk.len() < MIN_CARRIER_CHUNK {
                                    chunk.resize(MIN_CARRIER_CHUNK, MULAW_SILENCE);
                                }
                                csend!(json!({"event": "media", "media": {"payload": b64.encode(&chunk)}}));
                                st.stats.frames_to_carrier += 1;
                                st.playing = true;
                            }
                            st.out_buf.clear();
                            if st.playing && !st.cancelled.contains(&id) {
                                st.mark_seq += 1;
                                let name = format!("turn-{}", st.resp_turn.get(&id).copied().unwrap_or(st.turn));
                                let name = format!("{name}-{}", st.mark_seq);
                                csend!(json!({"event": "mark", "mark": {"name": name}}));
                                st.last_mark = Some(name);
                            }
                            if st.active.as_ref().is_some_and(|a| a.id == id) {
                                st.active = None;
                            }
                            if let Phase::Opening(Some(op)) = &st.phase {
                                if *op == id {
                                    // The disclosure was not heard in full: the model may not
                                    // speak freely after it, so the call ends here.
                                    let cut = st.cancelled.contains(&id)
                                        || matches!(status.as_deref(), Some("cancelled" | "failed" | "incomplete"));
                                    if cut {
                                        break EndReason::Error { kind: "opening_line_not_spoken".into() };
                                    }
                                    st.phase = Phase::Live;
                                }
                            }
                        }
                        ServerEvent::FunctionCall { response_id, call_id, name, arguments } => {
                            if response_id.as_ref().is_some_and(|r| st.gated.contains(r)) {
                                // The response was cancelled by the hold (or came before the
                                // opening line): its tool call is not the orchestrator's to act on.
                                xsend!(xai::function_output(&call_id, CANCELLED_TOOL_OUTPUT));
                                continue;
                            }
                            st.stats.tool_calls += 1;
                            st.continue_after_done = false;
                            if !spec.session.tools.iter().any(|t| t.name == name) {
                                // Not a declared tool: refused plainly, the orchestrator is not asked.
                                xsend!(xai::function_output(&call_id, UNKNOWN_TOOL_OUTPUT));
                                st.continue_after_done = st.pending_calls.is_empty();
                                continue;
                            }
                            st.pending_calls.insert(call_id.clone());
                            let arguments = serde_json::from_str(&arguments)
                                .unwrap_or_else(|_| json!({"_unparsed": true}));
                            let call = ToolCall { name, arguments, turn_id: st.turn };
                            let ev = spec.events.clone();
                            let tx = tool_tx.clone();
                            let bound = opts.tool_timeout;
                            while tools.try_join_next().is_some() {}
                            tools.spawn(async move {
                                let out = tokio::time::timeout(bound, ev.tool_call(call)).await.ok();
                                let _ = tx.send((call_id, out)).await;
                            });
                        }
                        ServerEvent::SpeechStarted { item_id } => {
                            idle_deadline = Instant::now() + opts.idle_timeout;
                            if matches!(st.phase, Phase::Live) && !st.paused {
                                st.turn += 1;
                                if let Some(i) = item_id {
                                    st.item_turn.insert(i, st.turn);
                                }
                                if st.playing || st.active.is_some() {
                                    st.stats.interruptions += 1;
                                    st.out_buf.clear();
                                    csend!(json!({"event": "clear"}));
                                    st.playing = false;
                                    if let Some(a) = st.active.take() {
                                        xsend!(xai::response_cancel(Some(&a.id)));
                                        st.cancelled.insert(a.id);
                                    }
                                }
                            }
                        }
                        ServerEvent::InputTranscript { item_id, transcript } => {
                            let turn = item_id.and_then(|i| st.item_turn.get(&i).copied()).unwrap_or(st.turn);
                            tline!(TranscriptLine { speaker: Speaker::Counterparty, text: transcript, turn_id: turn, at: Utc::now() });
                        }
                        ServerEvent::OutputTranscriptDone { response_id, transcript } => {
                            let rid = response_id.unwrap_or_default();
                            if !st.cancelled.contains(&rid) && !transcript.is_empty() {
                                let turn = st.resp_turn.get(&rid).copied().unwrap_or(st.turn);
                                tline!(TranscriptLine { speaker: Speaker::Assistant, text: transcript, turn_id: turn, at: Utc::now() });
                            }
                        }
                        ServerEvent::Error { kind, code } => {
                            st.stats.provider_errors += 1;
                            log("voice bridge provider error", &[("kind", json!(kind)), ("code", json!(code))]);
                            if kind == "timeout" || kind == "max_duration" {
                                break EndReason::Error { kind: format!("provider_error:{kind}") };
                            }
                        }
                        ServerEvent::SpeechStopped | ServerEvent::Other => {}
                    }
                }
                m = c_stream.next() => {
                    let text = match m {
                        None | Some(Err(_)) | Some(Ok(AxMessage::Close(_))) => break EndReason::CarrierClosed,
                        Some(Ok(AxMessage::Text(t))) => t.to_string(),
                        Some(Ok(_)) => { st.stats.malformed_carrier += 1; continue; }
                    };
                    let Ok(v) = serde_json::from_str::<Value>(&text) else {
                        st.stats.malformed_carrier += 1;
                        continue;
                    };
                    match v.get("event").and_then(Value::as_str) {
                        Some("media") => {
                            let track = v.pointer("/media/track").and_then(Value::as_str).unwrap_or("inbound");
                            if track.starts_with("outbound") {
                                continue;
                            }
                            let Some(audio) = v.pointer("/media/payload").and_then(Value::as_str).and_then(|p| b64.decode(p).ok()) else {
                                st.stats.malformed_carrier += 1;
                                continue;
                            };
                            if let Some(chunk) = num(v.pointer("/media/chunk")) {
                                if st.last_chunk.is_some_and(|l| chunk <= l) {
                                    st.stats.dropped_late_chunks += 1;
                                    continue;
                                }
                                st.last_chunk = Some(chunk);
                            }
                            if matches!(st.phase, Phase::Live) && !st.paused {
                                xsend!(xai::append_audio(&audio));
                                st.stats.frames_to_provider += 1;
                            } else {
                                st.stats.dropped_gated += 1;
                            }
                        }
                        Some("stop") => break EndReason::CarrierClosed,
                        Some("mark") => {
                            let name = v.pointer("/mark/name").and_then(Value::as_str);
                            if name.is_some() && name == st.last_mark.as_deref() {
                                st.playing = false;
                            }
                        }
                        Some("error") => {
                            log("voice bridge carrier error", &[("code", v.pointer("/payload/code").cloned().unwrap_or(Value::Null))]);
                        }
                        Some(_) => {}
                        None => st.stats.malformed_carrier += 1,
                    }
                }
                _ = tokio::time::sleep_until(session_deadline), if awaiting_session => {
                    break EndReason::Error { kind: "provider_session_timeout".into() };
                }
                _ = tokio::time::sleep_until(idle_deadline), if !st.paused => {
                    break EndReason::IdleTimeout;
                }
            }

            // After every event: the latest server direction (a silent system item; it starts
            // no response), then one response at a time, then queued lines.
            if matches!(st.phase, Phase::Live) && !st.paused {
                if let Some(text) = st.instruction.take() {
                    if let Some(old) = st.instruction_item.take() {
                        xsend!(xai::delete_item(&old));
                    }
                    st.instruction_seq += 1;
                    let id = format!("ss_direction_{}", st.instruction_seq);
                    xsend!(xai::system_message(
                        &id,
                        &format!("{INSTRUCTION_PREFIX}{text}")
                    ));
                    st.instruction_item = Some(id);
                }
            }
            let free = st.active.is_none() && !st.requested && matches!(st.phase, Phase::Live);
            // While paused the continuation waits for `resume` (it would only be cancelled).
            if free && !st.paused && st.continue_after_done && st.pending_calls.is_empty() {
                st.continue_after_done = false;
                st.requested = true;
                xsend!(xai::response_create());
            } else if free && !st.paused {
                if let Some(line) = st.say_queue.pop_front() {
                    xsend!(xai::force_message(&line, true));
                    st.requested = true;
                }
            }
        }
    };

    let _ = x_tx.try_send(None);
    drop(x_tx);
    // Every line produced; `run` subtracts the ones the orchestrator finished taking.
    st.stats.transcripts_undelivered = st.transcripts_produced;
    let end = finish(reason, st.stats, c_tx);
    let tail = Tail {
        c: c_task,
        t: t_task,
        x: Some(x_task),
        t_tx,
        backlog: st.backlog.into_iter().map(|(_, line)| line).collect(),
        tools,
        delivered,
    };
    (end, tail.join())
}

/// What is left of a bridge when its loop ends.
struct Tail {
    c: JoinHandle<()>,
    t: JoinHandle<()>,
    x: Option<JoinHandle<()>>,
    t_tx: mpsc::Sender<TranscriptLine>,
    backlog: Vec<TranscriptLine>,
    tools: JoinSet<()>,
    delivered: Arc<AtomicU64>,
}

/// Wait for a task until `deadline`, then abort it: nothing of a bridge outlives its end.
async fn join_or_abort(mut h: JoinHandle<()>, deadline: Instant) {
    if tokio::time::timeout_at(deadline, &mut h).await.is_err() {
        h.abort();
        let _ = tokio::time::timeout(Duration::from_millis(500), h).await;
    }
}

impl Tail {
    fn early(
        c: JoinHandle<()>,
        t: JoinHandle<()>,
        t_tx: mpsc::Sender<TranscriptLine>,
        delivered: Arc<AtomicU64>,
    ) -> Self {
        Self {
            c,
            t,
            x: None,
            t_tx,
            backlog: Vec::new(),
            tools: JoinSet::new(),
            delivered,
        }
    }

    fn join(self) -> Done {
        let Self {
            c,
            t,
            x,
            t_tx,
            backlog,
            mut tools,
            delivered,
        } = self;
        Box::pin(async move {
            let deadline = Instant::now() + JOIN_WAIT;
            // A tool call still running is not the model's to hear about any more.
            tools.abort_all();
            let tools_gone = async {
                let _ = tokio::time::timeout_at(deadline, tools.shutdown()).await;
            };
            // Lines that were waiting get the same bounded chance to be taken, in order.
            let flush = async move {
                for line in backlog {
                    if !matches!(
                        tokio::time::timeout_at(deadline, t_tx.send(line)).await,
                        Ok(Ok(()))
                    ) {
                        break;
                    }
                }
                drop(t_tx);
            };
            let x_gone = async {
                if let Some(x) = x {
                    join_or_abort(x, deadline).await;
                }
            };
            tokio::join!(
                tools_gone,
                flush,
                join_or_abort(c, deadline),
                x_gone,
                join_or_abort(t, deadline)
            );
            delivered.load(Ordering::Relaxed)
        })
    }
}
