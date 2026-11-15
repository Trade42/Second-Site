//! xAI realtime voice (Grok Voice) over a WebSocket: the client half of the voice bridge
//! (`services::voice_bridge`).
//!
//! Field names are taken from xAI's documentation, saved by the voice-1 investigation:
//! <https://docs.x.ai/developers/rest-api-reference/inference/voice.md> (section "Realtime" and
//! its schema file `voice-realtime.ws.json`) and
//! <https://docs.x.ai/developers/model-capabilities/audio/speech-to-speech.md> (session
//! parameters, audio formats, function calling, `force_message`).
//!
//! The server key goes only into the `Authorization: Bearer` header of the WebSocket handshake.
//! It is never part of the URL, a log line or an error text ([`XaiConfig`]'s `Debug` redacts
//! it).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Production endpoint (`wss://api.x.ai/v1/realtime`, model in the query).
pub const DEFAULT_REALTIME_URL: &str = "wss://api.x.ai/v1/realtime?model=grok-voice-latest";

/// The largest frame accepted from xAI (an audio delta is a few kilobytes).
pub const MAX_PROVIDER_FRAME: usize = 1 << 20;

/// Where and how to connect. Tests point `url` at a local mock (`ws://127.0.0.1:...`).
#[derive(Clone)]
pub struct XaiConfig {
    pub url: String,
    api_key: Arc<str>,
    pub connect_timeout: Duration,
}

impl XaiConfig {
    pub fn new(url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let key: String = api_key.into();
        Self {
            url: url.into(),
            api_key: Arc::from(key),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

impl std::fmt::Debug for XaiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XaiConfig")
            .field("url", &self.url)
            .field("api_key", &"<redacted>")
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

/// Why the connection could not be opened. The text never carries the key or the URL's query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XaiConnectError {
    Timeout,
    /// Bad URL or key characters (not a valid header value).
    Request,
    /// TCP, TLS or handshake refusal; `status` is the HTTP status when the server answered.
    Handshake {
        status: Option<u16>,
    },
}

impl std::fmt::Display for XaiConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "xai realtime connect timed out"),
            Self::Request => write!(f, "xai realtime request could not be built"),
            Self::Handshake { status: Some(s) } => {
                write!(f, "xai realtime handshake refused (HTTP {s})")
            }
            Self::Handshake { status: None } => write!(f, "xai realtime handshake failed"),
        }
    }
}

pub type XaiSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Open the realtime socket with the server key in the `Authorization` header.
pub async fn connect(cfg: &XaiConfig) -> Result<XaiSocket, XaiConnectError> {
    let mut req = cfg
        .url
        .as_str()
        .into_client_request()
        .map_err(|_| XaiConnectError::Request)?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", cfg.api_key))
        .map_err(|_| XaiConnectError::Request)?;
    req.headers_mut().insert("authorization", auth);
    let ws_cfg = WebSocketConfig::default()
        .max_message_size(Some(MAX_PROVIDER_FRAME))
        .max_frame_size(Some(MAX_PROVIDER_FRAME));
    // `wss:` uses tokio-tungstenite's rustls connector with the webpki roots; the crate's
    // rustls has only the ring provider compiled in, so that is the one it picks.
    let fut = tokio_tungstenite::connect_async_with_config(req, Some(ws_cfg), true);
    match tokio::time::timeout(cfg.connect_timeout, fut).await {
        Err(_) => Err(XaiConnectError::Timeout),
        Ok(Err(e)) => Err(XaiConnectError::Handshake {
            status: match e {
                tokio_tungstenite::tungstenite::Error::Http(resp) => Some(resp.status().as_u16()),
                _ => None,
            },
        }),
        Ok(Ok((ws, _))) => Ok(ws),
    }
}

/// One function tool the model may call (`session.tools[]`, `type: "function"`). Server-side
/// tools (`web_search`, `x_search`, `file_search`, `mcp`) are deliberately not representable.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionTool {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
}

/// What the bridge configures with `session.update`. Audio is always `audio/pcmu` (8 kHz G.711
/// mu-law, the carrier's codec) in both directions and turn detection is always `server_vad`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSpec {
    pub instructions: String,
    pub voice: String,
    pub tools: Vec<FunctionTool>,
    /// `audio.input.transcription.model` (e.g. `grok-transcribe`); `None` leaves it unset.
    pub input_transcription_model: Option<String>,
}

impl SessionSpec {
    /// The `session.update` client event, exactly as sent.
    pub fn session_update(&self) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        let mut input = json!({"format": {"type": "audio/pcmu"}});
        if let Some(m) = &self.input_transcription_model {
            input["transcription"] = json!({ "model": m });
        }
        json!({
            "type": "session.update",
            "session": {
                "instructions": self.instructions,
                "voice": self.voice,
                "turn_detection": {"type": "server_vad"},
                "audio": {
                    "input": input,
                    "output": {"format": {"type": "audio/pcmu"}},
                },
                "tools": tools,
            }
        })
    }
}

/// Client events (bridge to xAI).
pub fn append_audio(audio: &[u8]) -> Value {
    json!({
        "type": "input_audio_buffer.append",
        "audio": base64::engine::general_purpose::STANDARD.encode(audio),
    })
}

/// A line spoken verbatim by TTS, without the model; it is its own turn (no `response.create`).
pub fn force_message(text: &str, interruptible: bool) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "force_message",
            "role": "assistant",
            "interruptible": interruptible,
            "content": [{"type": "output_text", "text": text}],
        }
    })
}

pub fn function_output(call_id: &str, output: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {"type": "function_call_output", "call_id": call_id, "output": output},
    })
}

/// A server direction for the model's next turns, as a system message item: no audio, nothing
/// spoken, and no response is started by it. Schema file `voice-realtime.ws.json`, client event
/// `conversation.item.create`, item "Message": `type: "message"`, `role: "system"` ("for
/// system-level context"), optional client-generated `id`, `content[].type: "input_text"`.
pub fn system_message(id: &str, text: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "message",
            "role": "system",
            "id": id,
            "content": [{"type": "input_text", "text": text}],
        }
    })
}

/// Remove a conversation item (`conversation.item.delete {item_id}`, same schema file).
pub fn delete_item(item_id: &str) -> Value {
    json!({"type": "conversation.item.delete", "item_id": item_id})
}

pub fn response_create() -> Value {
    json!({"type": "response.create"})
}

pub fn response_cancel(response_id: Option<&str>) -> Value {
    match response_id {
        Some(id) => json!({"type": "response.cancel", "response_id": id}),
        None => json!({"type": "response.cancel"}),
    }
}

/// The server events the bridge acts on; everything else is [`ServerEvent::Other`].
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    SessionUpdated,
    SpeechStarted {
        item_id: Option<String>,
    },
    SpeechStopped,
    InputTranscript {
        item_id: Option<String>,
        transcript: String,
    },
    ResponseCreated {
        id: String,
    },
    AudioDelta {
        response_id: Option<String>,
        audio: Vec<u8>,
    },
    OutputTranscriptDone {
        response_id: Option<String>,
        transcript: String,
    },
    FunctionCall {
        response_id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
    },
    ResponseDone {
        id: Option<String>,
        status: Option<String>,
    },
    /// `error.type` / `error.code` only: the message may echo client input and is dropped.
    Error {
        kind: String,
        code: Option<String>,
    },
    Other,
}

/// Why a frame could not be read as a server event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned)
}

/// Parse one text frame from xAI.
pub fn parse_server_event(text: &str) -> Result<ServerEvent, Malformed> {
    let v: Value = serde_json::from_str(text).map_err(|_| Malformed)?;
    let ty = v.get("type").and_then(Value::as_str).ok_or(Malformed)?;
    Ok(match ty {
        "session.updated" => ServerEvent::SessionUpdated,
        "input_audio_buffer.speech_started" => ServerEvent::SpeechStarted {
            item_id: s(&v, "item_id"),
        },
        "input_audio_buffer.speech_stopped" => ServerEvent::SpeechStopped,
        "conversation.item.input_audio_transcription.completed" => ServerEvent::InputTranscript {
            item_id: s(&v, "item_id"),
            transcript: s(&v, "transcript").ok_or(Malformed)?,
        },
        "response.created" => ServerEvent::ResponseCreated {
            id: v
                .get("response")
                .and_then(|r| s(r, "id"))
                .ok_or(Malformed)?,
        },
        // `response.audio.delta` is the older name the docs say is also used.
        "response.output_audio.delta" | "response.audio.delta" => {
            let b64 = v.get("delta").and_then(Value::as_str).ok_or(Malformed)?;
            ServerEvent::AudioDelta {
                response_id: s(&v, "response_id"),
                audio: base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|_| Malformed)?,
            }
        }
        "response.output_audio_transcript.done" => ServerEvent::OutputTranscriptDone {
            response_id: s(&v, "response_id"),
            transcript: s(&v, "transcript").unwrap_or_default(),
        },
        "response.function_call_arguments.done" => ServerEvent::FunctionCall {
            response_id: s(&v, "response_id"),
            call_id: s(&v, "call_id").ok_or(Malformed)?,
            name: s(&v, "name").ok_or(Malformed)?,
            arguments: s(&v, "arguments").unwrap_or_else(|| "{}".into()),
        },
        "response.done" => {
            let r = v.get("response");
            ServerEvent::ResponseDone {
                id: r.and_then(|r| s(r, "id")),
                status: r.and_then(|r| s(r, "status")),
            }
        }
        "error" => {
            let e = v.get("error");
            ServerEvent::Error {
                kind: e
                    .and_then(|e| s(e, "type"))
                    .unwrap_or_else(|| "unknown".into()),
                code: e.and_then(|e| s(e, "code")),
            }
        }
        _ => ServerEvent::Other,
    })
}
