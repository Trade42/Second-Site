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
}

/// One dial the fixture provider recorded (Python's `calls` entries).
#[derive(Debug, Clone, PartialEq)]
pub struct FixtureCall {
    pub id: String,
    pub to: String,
    pub idempotency_key: String,
    pub client_state: Option<IndexMap<String, Value>>,
}

/// One command the fixture provider recorded (Python's `commands` entries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelephonyCommand {
    Speak { leg: String, text: String },
    PlaybackStart { leg: String, audio_url: String },
    TranscriptionStart { leg: String },
    Hangup { leg: String },
}

impl TelephonyCommand {
    /// Python's `cmd` value.
    pub fn cmd(&self) -> &'static str {
        match self {
            TelephonyCommand::Speak { .. } => "speak",
            TelephonyCommand::PlaybackStart { .. } => "playback_start",
            TelephonyCommand::TranscriptionStart { .. } => "transcription_start",
            TelephonyCommand::Hangup { .. } => "hangup",
        }
    }

    /// The call leg the command was sent to.
    pub fn leg(&self) -> &str {
        match self {
            TelephonyCommand::Speak { leg, .. }
            | TelephonyCommand::PlaybackStart { leg, .. }
            | TelephonyCommand::TranscriptionStart { leg }
            | TelephonyCommand::Hangup { leg } => leg,
        }
    }

    /// Python's dict: `{"cmd", "leg"}` plus `"text"` or `"audio_url"`.
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
            });
            Ok(DialResult {
                call_control_id: id,
                state: "initiated".into(),
            })
        })
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
}
