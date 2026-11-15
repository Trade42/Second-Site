//! Port of the pure parts of `apps/api/app/integrations/telnyx/__init__.py` (Telnyx Call Control):
//! the `client_state` codec, [`DialResult`], the [`TelephonyProvider`] trait and its in-memory
//! [`FixtureTelephonyProvider`]. The live HTTP provider belongs to the server.
//!
//! Live dialing additionally requires `ALLOW_LIVE_OUTBOUND_CALLS=true` and an APPROVED approval
//! for the exact call payload, enforced by the caller at the action boundary. A dial result is
//! only the carrier's acceptance (`state = "initiated"`): the answer arrives later by webhook, so
//! a `DialResult` is never evidence that a call took place. The fixture provider never places a
//! call; it records what it was asked to do.

use std::sync::Mutex;

use base64::Engine;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use ss_core::IntegrationStatus;

use crate::contracts::ProviderStatusEntry;
use crate::integrations::base::{status_entry, ProviderError, ProviderErrorKind};
use crate::py;

/// Telnyx API base URL (`API`).
pub const API: &str = "https://api.telnyx.com/v2";
/// Voice used for `speak` (`VOICE`).
pub const VOICE: &str = "female";

/// `encode_client_state(state)`: base64 (standard alphabet, padded) of
/// `json.dumps(state, separators=(",", ":"))`: keys in insertion order (an [`IndexMap`], as
/// Python's dict), non-ASCII escaped as `\uXXXX`, floats written as Python writes them.
pub fn encode_client_state(state: &IndexMap<String, Value>) -> String {
    let mut json = String::from("{");
    for (i, (k, v)) in state.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        py::json_push_str(&mut json, k);
        json.push(':');
        py::json_push_compact(&mut json, v);
    }
    json.push('}');
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

/// The `client_state` the call flow sends: `{"call_id": <call id>, "role": <role>}`.
pub fn client_state(call_id: &str, role: &str) -> IndexMap<String, Value> {
    let mut m = IndexMap::new();
    m.insert("call_id".to_owned(), Value::from(call_id));
    m.insert("role".to_owned(), Value::from(role));
    m
}

/// `decode_client_state(value)`: the JSON object inside a base64 `client_state` (top-level keys
/// in document order; a repeated key keeps its first position and its last value, as Python's
/// dict), or an empty map for `None`, `""`, input that is not base64 (decoded leniently, as
/// Python's `base64.b64decode` does: characters outside the alphabet are skipped), bytes that
/// are not UTF-8 JSON, or JSON that is not an object. Never an error, as in Python.
///
/// Narrower than Python on exotic inputs, each of which gives an empty map here: JSON with
/// `NaN`/`Infinity` literals or lone surrogate escapes, UTF-16/32 encoded JSON, and nesting deeper
/// than 128 levels. Integers beyond `u64` become floats, as everywhere `serde_json` parses.
pub fn decode_client_state(value: Option<&str>) -> IndexMap<String, Value> {
    let Some(v) = value.filter(|v| !v.is_empty()) else {
        return IndexMap::new();
    };
    let Ok(bytes) = py::b64decode(v) else {
        return IndexMap::new();
    };
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    serde_json::from_slice::<IndexMap<String, Value>>(bytes).unwrap_or_default()
}

/// [`decode_client_state`] for the raw JSON value of a webhook's `payload.client_state`, as
/// Python receives it from `payload.get("client_state")`: absent or falsy (`null`, `false`, `0`,
/// `""`, `[]`, `{}`) gives an empty map; a string is decoded; any other value is `Err`, because
/// Python's `base64.b64decode` raises `TypeError` there, which no handler catches (500).
pub fn decode_client_state_json(value: Option<&Value>) -> Result<IndexMap<String, Value>, String> {
    match value {
        None => Ok(IndexMap::new()),
        Some(v) if !py::truthy(v) => Ok(IndexMap::new()),
        Some(Value::String(s)) => Ok(decode_client_state(Some(s))),
        Some(other) => Err(format!(
            "argument should be a bytes-like object or ASCII string, not '{}'",
            match other {
                Value::Bool(_) => "bool",
                Value::Number(n) if n.is_f64() => "float",
                Value::Number(_) => "int",
                Value::Array(_) => "list",
                _ => "dict",
            }
        )),
    }
}

/// `DialResult`: the carrier accepted the dial request. `state` is `"initiated"`; whether the call
/// was answered arrives later by webhook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialResult {
    pub call_control_id: String,
    pub state: String,
}

/// `stream_track`: which audio of the call Telnyx streams to `stream_url`
/// (<https://developers.telnyx.com/api-reference/call-commands/streaming-start>; Telnyx's default
/// is `inbound_track`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamTrack {
    #[default]
    Inbound,
    Outbound,
    Both,
}

impl StreamTrack {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamTrack::Inbound => "inbound_track",
            StreamTrack::Outbound => "outbound_track",
            StreamTrack::Both => "both_tracks",
        }
    }
}

/// A codec value of `stream_codec` or `stream_bidirectional_codec` (Telnyx's `StreamCodec` /
/// `StreamBidirectionalCodec` enums, case sensitive). Telnyx's `stream_codec` also accepts
/// `default`: that is `None` in [`StreamOptions::stream_codec`] (the field is then not sent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamCodec {
    Pcmu,
    Pcma,
    G722,
    Opus,
    AmrWb,
    L16,
}

impl StreamCodec {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamCodec::Pcmu => "PCMU",
            StreamCodec::Pcma => "PCMA",
            StreamCodec::G722 => "G722",
            StreamCodec::Opus => "OPUS",
            StreamCodec::AmrWb => "AMR-WB",
            StreamCodec::L16 => "L16",
        }
    }
}

/// `stream_bidirectional_target_legs` (Telnyx's default is `opposite`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTargetLegs {
    Both,
    Own,
    Opposite,
}

impl StreamTargetLegs {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamTargetLegs::Both => "both",
            StreamTargetLegs::Own => "self",
            StreamTargetLegs::Opposite => "opposite",
        }
    }
}

/// Bidirectional streaming: `stream_bidirectional_mode = "rtp"`, so that the base64 RTP payloads
/// our service sends back over the WebSocket (`{"event":"media","media":{"payload":..}}`) are
/// played into the call. Fields left `None` are not sent and take Telnyx's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bidirectional {
    /// `stream_bidirectional_codec` (Telnyx's default `PCMU`, 8 kHz).
    pub codec: StreamCodec,
    pub target_legs: Option<StreamTargetLegs>,
    /// `stream_bidirectional_sampling_rate`: 8000, 16000, 22050, 24000 or 48000.
    pub sampling_rate: Option<u32>,
}

/// The media-stream parameters of `streaming_start` and of a dial that streams from the start
/// (the same field names on both; see the Telnyx pages cited at [`TelephonyProvider::start_streaming`]).
/// Optional fields left `None` are not sent at all.
#[derive(Clone, PartialEq, Eq)]
pub struct StreamOptions {
    /// `stream_url`: the `wss://` address Telnyx connects to.
    pub stream_url: String,
    /// `stream_track`.
    pub track: StreamTrack,
    /// `stream_codec` of the audio Telnyx sends us (`None`: the call's codec).
    pub stream_codec: Option<StreamCodec>,
    /// `None`: one-way stream (Telnyx's default `mp3` bidirectional mode is not requested).
    pub bidirectional: Option<Bidirectional>,
    /// `stream_auth_token`, sent by Telnyx when it opens the WebSocket (at most 4000
    /// characters). Never printed by `Debug` and never in the fixture's JSON record.
    pub auth_token: Option<String>,
}

impl std::fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamOptions")
            .field("stream_url", &self.stream_url)
            .field("track", &self.track)
            .field("stream_codec", &self.stream_codec)
            .field("bidirectional", &self.bidirectional)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl StreamOptions {
    /// What a voice agent on the call needs: the callee's audio (`inbound_track`) as PCMU, and
    /// bidirectional RTP in PCMU so that what we send back is played into the call.
    pub fn voice_agent(stream_url: &str) -> Self {
        Self {
            stream_url: stream_url.to_owned(),
            track: StreamTrack::Inbound,
            stream_codec: Some(StreamCodec::Pcmu),
            bidirectional: Some(Bidirectional {
                codec: StreamCodec::Pcmu,
                target_legs: None,
                sampling_rate: None,
            }),
            auth_token: None,
        }
    }

    /// The request fields, Telnyx's names, only those set.
    pub fn to_fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("stream_url".into(), Value::from(self.stream_url.as_str()));
        m.insert("stream_track".into(), Value::from(self.track.as_str()));
        if let Some(c) = self.stream_codec {
            m.insert("stream_codec".into(), Value::from(c.as_str()));
        }
        if let Some(b) = &self.bidirectional {
            m.insert("stream_bidirectional_mode".into(), Value::from("rtp"));
            m.insert(
                "stream_bidirectional_codec".into(),
                Value::from(b.codec.as_str()),
            );
            if let Some(t) = b.target_legs {
                m.insert(
                    "stream_bidirectional_target_legs".into(),
                    Value::from(t.as_str()),
                );
            }
            if let Some(r) = b.sampling_rate {
                m.insert("stream_bidirectional_sampling_rate".into(), Value::from(r));
            }
        }
        if let Some(t) = &self.auth_token {
            m.insert("stream_auth_token".into(), Value::from(t.as_str()));
        }
        m
    }
}

/// The carrier events of a media stream (`data.event_type`), named here so that the call
/// orchestrator can match on them. Until something is wired to them the orchestrator ignores
/// them as it ignores every event type it does not handle.
/// <https://developers.telnyx.com/docs/voice/programmable-voice/media-streaming>,
/// <https://developers.telnyx.com/api-reference/callbacks/call-streaming-started>.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEvent {
    Started,
    Stopped,
    Failed,
}

impl StreamEvent {
    pub const STARTED: &'static str = "streaming.started";
    pub const STOPPED: &'static str = "streaming.stopped";
    pub const FAILED: &'static str = "streaming.failed";

    pub fn parse(event_type: &str) -> Option<Self> {
        match event_type {
            Self::STARTED => Some(StreamEvent::Started),
            Self::STOPPED => Some(StreamEvent::Stopped),
            Self::FAILED => Some(StreamEvent::Failed),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StreamEvent::Started => Self::STARTED,
            StreamEvent::Stopped => Self::STOPPED,
            StreamEvent::Failed => Self::FAILED,
        }
    }
}

fn unsupported(what: &str) -> TelephonyError {
    TelephonyError::Provider(ProviderError::new(
        "telnyx",
        ProviderErrorKind::NotConfigured,
        format!("{what} is not supported by this telephony provider"),
    ))
}

/// Why a telephony command failed: a provider error (Python `ProviderError`), or live outbound
/// calls being disabled (Python `ForbiddenError`, HTTP 403 `FORBIDDEN`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TelephonyError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("{0}")]
    Forbidden(String),
}

/// `TelephonyProvider` (Python `Protocol`). Implementations are shared by request handlers, so
/// methods take `&self` and recording state sits behind interior mutability.
pub trait TelephonyProvider: Send + Sync {
    /// `"live"` or `"fixture"`.
    fn mode(&self) -> &str;
    /// Ask the carrier to dial `to` from `from`. Idempotent on `idempotency_key`.
    fn dial(
        &self,
        to: &str,
        from: &str,
        webhook_url: &str,
        idempotency_key: &str,
        client_state: Option<&IndexMap<String, Value>>,
    ) -> Result<DialResult, TelephonyError>;
    fn speak(&self, call_control_id: &str, text: &str) -> Result<(), TelephonyError>;
    fn play(&self, call_control_id: &str, audio_url: &str) -> Result<(), TelephonyError>;
    fn start_transcription(&self, call_control_id: &str) -> Result<(), TelephonyError>;
    fn hangup(&self, call_control_id: &str) -> Result<(), TelephonyError>;
    /// Create a conference holding `call_control_id`; returns the conference id.
    fn create_conference(
        &self,
        call_control_id: &str,
        name: &str,
    ) -> Result<String, TelephonyError>;
    fn join_conference(
        &self,
        conference_id: &str,
        call_control_id: &str,
    ) -> Result<(), TelephonyError>;
    fn status(&self) -> ProviderStatusEntry;

    // The media-stream commands. The defaults refuse (`NOT_CONFIGURED`) without doing anything,
    // so that a provider written before them (a test double) still compiles; the live and the
    // fixture provider implement every one.

    /// [`TelephonyProvider::dial`] that also asks for the media stream from the first moment
    /// (`stream_url` and the other stream fields on `POST /calls`).
    /// <https://developers.telnyx.com/api-reference/call-commands/dial>
    fn dial_streaming(
        &self,
        _to: &str,
        _from: &str,
        _webhook_url: &str,
        _idempotency_key: &str,
        _client_state: Option<&IndexMap<String, Value>>,
        _stream: &StreamOptions,
    ) -> Result<DialResult, TelephonyError> {
        Err(unsupported("dial_streaming"))
    }
    /// `POST /calls/{id}/actions/streaming_start`.
    /// <https://developers.telnyx.com/api-reference/call-commands/streaming-start>
    fn start_streaming(
        &self,
        _call_control_id: &str,
        _stream: &StreamOptions,
    ) -> Result<(), TelephonyError> {
        Err(unsupported("streaming_start"))
    }
    /// `POST /calls/{id}/actions/streaming_stop`; `stream_id` `None` stops every stream of the
    /// call. <https://developers.telnyx.com/api-reference/call-commands/streaming-stop>
    fn stop_streaming(
        &self,
        _call_control_id: &str,
        _stream_id: Option<&str>,
    ) -> Result<(), TelephonyError> {
        Err(unsupported("streaming_stop"))
    }
    /// `POST /calls/{id}/actions/transcription_stop`.
    /// <https://developers.telnyx.com/api-reference/call-commands/transcription-stop>
    fn stop_transcription(&self, _call_control_id: &str) -> Result<(), TelephonyError> {
        Err(unsupported("transcription_stop"))
    }
    /// `POST /calls/{id}/actions/playback_stop` with `stop = "all"` (the current audio and the
    /// queue). <https://developers.telnyx.com/api-reference/call-commands/stop-audio-playback>
    fn stop_playback(&self, _call_control_id: &str) -> Result<(), TelephonyError> {
        Err(unsupported("playback_stop"))
    }
}

/// One dial the fixture provider recorded (Python's `calls` entries).
#[derive(Debug, Clone, PartialEq)]
pub struct FixtureCall {
    pub id: String,
    pub to: String,
    pub idempotency_key: String,
    pub client_state: Option<IndexMap<String, Value>>,
    /// The stream asked for on the dial (`dial_streaming`); `None` for a plain `dial`.
    pub stream: Option<StreamOptions>,
}

/// One command the fixture provider recorded (Python's `commands` entries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelephonyCommand {
    Speak {
        leg: String,
        text: String,
    },
    PlaybackStart {
        leg: String,
        audio_url: String,
    },
    TranscriptionStart {
        leg: String,
    },
    Hangup {
        leg: String,
    },
    StreamingStart {
        leg: String,
        stream: StreamOptions,
    },
    StreamingStop {
        leg: String,
        stream_id: Option<String>,
    },
    TranscriptionStop {
        leg: String,
    },
    PlaybackStop {
        leg: String,
    },
}

impl TelephonyCommand {
    /// Python's `cmd` value.
    pub fn cmd(&self) -> &'static str {
        match self {
            TelephonyCommand::Speak { .. } => "speak",
            TelephonyCommand::PlaybackStart { .. } => "playback_start",
            TelephonyCommand::TranscriptionStart { .. } => "transcription_start",
            TelephonyCommand::Hangup { .. } => "hangup",
            TelephonyCommand::StreamingStart { .. } => "streaming_start",
            TelephonyCommand::StreamingStop { .. } => "streaming_stop",
            TelephonyCommand::TranscriptionStop { .. } => "transcription_stop",
            TelephonyCommand::PlaybackStop { .. } => "playback_stop",
        }
    }

    /// The call leg the command was sent to.
    pub fn leg(&self) -> &str {
        match self {
            TelephonyCommand::Speak { leg, .. }
            | TelephonyCommand::PlaybackStart { leg, .. }
            | TelephonyCommand::TranscriptionStart { leg }
            | TelephonyCommand::Hangup { leg }
            | TelephonyCommand::StreamingStart { leg, .. }
            | TelephonyCommand::StreamingStop { leg, .. }
            | TelephonyCommand::TranscriptionStop { leg }
            | TelephonyCommand::PlaybackStop { leg } => leg,
        }
    }

    /// Python's dict: `{"cmd", "leg"}` plus `"text"` or `"audio_url"`; for the stream commands
    /// (Rust only) `"stream_url"` and `"stream_track"`, or `"stream_id"` when one was given. The
    /// stream's auth token is never in it.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("cmd".into(), Value::from(self.cmd()));
        m.insert("leg".into(), Value::from(self.leg()));
        match self {
            TelephonyCommand::Speak { text, .. } => {
                m.insert("text".into(), Value::from(text.as_str()));
            }
            TelephonyCommand::PlaybackStart { audio_url, .. } => {
                m.insert("audio_url".into(), Value::from(audio_url.as_str()));
            }
            TelephonyCommand::StreamingStart { stream, .. } => {
                m.insert("stream_url".into(), Value::from(stream.stream_url.as_str()));
                m.insert("stream_track".into(), Value::from(stream.track.as_str()));
            }
            TelephonyCommand::StreamingStop {
                stream_id: Some(id),
                ..
            } => {
                m.insert("stream_id".into(), Value::from(id.as_str()));
            }
            _ => {}
        }
        Value::Object(m)
    }
}

/// What the fixture provider has recorded so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FixtureTelephonyState {
    pub calls: Vec<FixtureCall>,
    /// Conference id -> call legs, in join order.
    pub conferences: IndexMap<String, Vec<String>>,
    pub commands: Vec<TelephonyCommand>,
    /// The next `dial` fails once with `UNAVAILABLE` ("simulated dial failure").
    pub fail_next_dial: bool,
}

/// `FixtureTelephonyProvider`: records every command; the call simulator plays the carrier's role
/// by emitting webhook-shaped events. Ids are `fixture-call-<uuid4>` and `fixture-conf-<uuid4>`.
/// Status `MOCK`, "Simulated calls; no PSTN traffic.": it never claims a call was placed.
#[derive(Debug, Default)]
pub struct FixtureTelephonyProvider {
    state: Mutex<FixtureTelephonyState>,
}

impl FixtureTelephonyProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn with<R>(&self, f: impl FnOnce(&mut FixtureTelephonyState) -> R) -> R {
        // A poisoned lock only means another thread panicked mid-update; the recorded data is
        // still the best record we have.
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    /// A copy of everything recorded.
    pub fn snapshot(&self) -> FixtureTelephonyState {
        self.with(|s| s.clone())
    }

    /// Python's `fail_next_dial = value`.
    pub fn set_fail_next_dial(&self, value: bool) {
        self.with(|s| s.fail_next_dial = value);
    }

    /// Python's `calls`.
    pub fn calls(&self) -> Vec<FixtureCall> {
        self.with(|s| s.calls.clone())
    }

    /// Python's `commands`.
    pub fn commands(&self) -> Vec<TelephonyCommand> {
        self.with(|s| s.commands.clone())
    }

    /// Python's `conferences`.
    pub fn conferences(&self) -> IndexMap<String, Vec<String>> {
        self.with(|s| s.conferences.clone())
    }
}

impl FixtureTelephonyProvider {
    fn dial_inner(
        &self,
        to: &str,
        idempotency_key: &str,
        client_state: Option<&IndexMap<String, Value>>,
        stream: Option<&StreamOptions>,
    ) -> Result<DialResult, TelephonyError> {
        self.with(|s| {
            if s.fail_next_dial {
                s.fail_next_dial = false;
                return Err(TelephonyError::Provider(ProviderError::new(
                    "telnyx",
                    ProviderErrorKind::Unavailable,
                    "simulated dial failure",
                )));
            }
            if let Some(c) = s
                .calls
                .iter()
                .find(|c| c.idempotency_key == idempotency_key)
            {
                return Ok(DialResult {
                    call_control_id: c.id.clone(),
                    state: "initiated".into(),
                });
            }
            let id = format!("fixture-call-{}", uuid::Uuid::new_v4());
            s.calls.push(FixtureCall {
                id: id.clone(),
                to: to.to_owned(),
                idempotency_key: idempotency_key.to_owned(),
                client_state: client_state.cloned(),
                stream: stream.cloned(),
            });
            Ok(DialResult {
                call_control_id: id,
                state: "initiated".into(),
            })
        })
    }

    fn record(&self, cmd: TelephonyCommand) -> Result<(), TelephonyError> {
        self.with(|s| s.commands.push(cmd));
        Ok(())
    }
}

impl TelephonyProvider for FixtureTelephonyProvider {
    fn mode(&self) -> &str {
        "fixture"
    }

    fn dial(
        &self,
        to: &str,
        _from: &str,
        _webhook_url: &str,
        idempotency_key: &str,
        client_state: Option<&IndexMap<String, Value>>,
    ) -> Result<DialResult, TelephonyError> {
        self.dial_inner(to, idempotency_key, client_state, None)
    }

    fn speak(&self, call_control_id: &str, text: &str) -> Result<(), TelephonyError> {
        self.with(|s| {
            s.commands.push(TelephonyCommand::Speak {
                leg: call_control_id.to_owned(),
                text: text.to_owned(),
            })
        });
        Ok(())
    }

    fn play(&self, call_control_id: &str, audio_url: &str) -> Result<(), TelephonyError> {
        self.with(|s| {
            s.commands.push(TelephonyCommand::PlaybackStart {
                leg: call_control_id.to_owned(),
                audio_url: audio_url.to_owned(),
            })
        });
        Ok(())
    }

    fn start_transcription(&self, call_control_id: &str) -> Result<(), TelephonyError> {
        self.with(|s| {
            s.commands.push(TelephonyCommand::TranscriptionStart {
                leg: call_control_id.to_owned(),
            })
        });
        Ok(())
    }

    fn hangup(&self, call_control_id: &str) -> Result<(), TelephonyError> {
        self.with(|s| {
            s.commands.push(TelephonyCommand::Hangup {
                leg: call_control_id.to_owned(),
            })
        });
        Ok(())
    }

    fn create_conference(
        &self,
        call_control_id: &str,
        _name: &str,
    ) -> Result<String, TelephonyError> {
        let conf = format!("fixture-conf-{}", uuid::Uuid::new_v4());
        self.with(|s| {
            s.conferences
                .insert(conf.clone(), vec![call_control_id.to_owned()])
        });
        Ok(conf)
    }

    fn join_conference(
        &self,
        conference_id: &str,
        call_control_id: &str,
    ) -> Result<(), TelephonyError> {
        self.with(|s| {
            s.conferences
                .entry(conference_id.to_owned())
                .or_default()
                .push(call_control_id.to_owned())
        });
        Ok(())
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            "telnyx",
            self.mode(),
            IntegrationStatus::Mock,
            false,
            "Simulated calls; no PSTN traffic.",
        )
    }

    fn dial_streaming(
        &self,
        to: &str,
        _from: &str,
        _webhook_url: &str,
        idempotency_key: &str,
        client_state: Option<&IndexMap<String, Value>>,
        stream: &StreamOptions,
    ) -> Result<DialResult, TelephonyError> {
        self.dial_inner(to, idempotency_key, client_state, Some(stream))
    }

    fn start_streaming(
        &self,
        call_control_id: &str,
        stream: &StreamOptions,
    ) -> Result<(), TelephonyError> {
        self.record(TelephonyCommand::StreamingStart {
            leg: call_control_id.to_owned(),
            stream: stream.clone(),
        })
    }

    fn stop_streaming(
        &self,
        call_control_id: &str,
        stream_id: Option<&str>,
    ) -> Result<(), TelephonyError> {
        self.record(TelephonyCommand::StreamingStop {
            leg: call_control_id.to_owned(),
            stream_id: stream_id.map(str::to_owned),
        })
    }

    fn stop_transcription(&self, call_control_id: &str) -> Result<(), TelephonyError> {
        self.record(TelephonyCommand::TranscriptionStop {
            leg: call_control_id.to_owned(),
        })
    }

    fn stop_playback(&self, call_control_id: &str) -> Result<(), TelephonyError> {
        self.record(TelephonyCommand::PlaybackStop {
            leg: call_control_id.to_owned(),
        })
    }
}
