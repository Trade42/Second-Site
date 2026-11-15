//! The live half of `apps/api/app/integrations/telnyx/__init__.py` (Telnyx Call Control v2) and
//! the voice provider selection of `core/container.py`. The trait, the fixture provider and the
//! `client_state` codec are `ss_domain::integrations::telnyx`.
//!
//! Selection (Python's): [`LiveTelnyxProvider`] only when `VOICE_PROVIDER_MODE=live` **and**
//! `TELNYX_API_KEY` is set; otherwise [`FixtureTelephonyProvider`], which never places a call.
//! Live dialing also needs `ALLOW_LIVE_OUTBOUND_CALLS=true` (else `dial` is refused with Python's
//! `ForbiddenError`, HTTP 403) and an APPROVED approval for the exact payload, which the call
//! orchestrator enforces before it dials.
//!
//! Requests are Python's: `POST {API}{path}` with `Authorization: Bearer <key>`, a JSON body, and
//! `Idempotency-Key` on `dial` only; one attempt (Python's `RetryPolicy(attempts=1)`), 20 s
//! timeout; the answer's `data` object. A dial answer only means the carrier accepted the request
//! (`state = "initiated"`); whether the call was answered arrives later by webhook.
//!
//! Differences from Python: a 2xx answer without `data.call_control_id` (dial) or `data.id`
//! (conference) is a `BAD_RESPONSE` provider error here (Python: an uncaught `KeyError`, 500);
//! path segments taken from the carrier (`call_control_id`, conference id) are percent-encoded
//! outside `[A-Za-z0-9-._~:]` so that they cannot change the request path.
//! The media-stream commands (`dial_streaming`, `streaming_start`/`_stop`, `transcription_stop`,
//! `playback_stop`) are Rust only (Python has none); they follow the same request pattern, without
//! `command_id` or `client_state`, as every other command here.
//! Whom a live dial rings (owner, 11:45Z, "Can they just call me only?"): with
//! `LIVE_CALLS_ONLY_TO` set, that number instead of the one asked for, always; without it nobody
//! (403 before any request), unless `LIVE_CALLS_TO_CONTACTS=true`. See
//! [`Settings::live_call_target`].
//! Tested only against a local mock server: no real call has been placed by this code.

use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_core::IntegrationStatus;
use ss_domain::contracts::ProviderStatusEntry;
use ss_domain::integrations::base::{status_entry, ProviderError, ProviderErrorKind, RetryPolicy};
use ss_domain::integrations::telnyx::{
    encode_client_state, DialResult, FixtureTelephonyProvider, StreamOptions, TelephonyError,
    TelephonyProvider, API, VOICE,
};

use super::http::HttpClient;
use super::Reported;
use crate::app::{App, ProviderStatus};
use crate::config::{mask_number, LiveCallTarget, ProviderMode, Settings};

const PROVIDER: &str = "telnyx";

fn policy() -> RetryPolicy {
    RetryPolicy {
        attempts: 1,
        backoff_s: 0.5,
        timeout_s: 20.0,
    }
}

/// Percent-encode a path segment outside `[A-Za-z0-9-._~:]`.
pub fn path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `LiveTelnyxProvider`.
pub struct LiveTelnyxProvider {
    api_key: String,
    connection_id: String,
    allow_outbound: bool,
    configured: bool,
    /// [`Settings::voice_live_missing`] at construction: while not empty, no dial is sent.
    preflight_missing: Vec<&'static str>,
    /// [`Settings::live_call_target`]: whom a dial may ring. `None` only on a client built by
    /// hand (tests); [`Voice::from_settings`] always sets it.
    target: Option<LiveCallTarget>,
    base_url: String,
    client: HttpClient,
}

/// The refusal while live calls are restricted to the owner's test number and none is set.
pub const ONLY_TO_REFUSAL: &str =
    "live call not dialed, nothing was sent to the carrier: live calls are restricted to the \
     owner's test number, and LIVE_CALLS_ONLY_TO is not set to a phone number (+ and 8 to 15 \
     digits). Set it to the owner's own number. No listing contact can be dialed without \
     LIVE_CALLS_TO_CONTACTS=true.";

impl std::fmt::Debug for LiveTelnyxProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("LiveTelnyxProvider")
            .field("base_url", &self.base_url)
            .field("allow_outbound", &self.allow_outbound)
            .field("configured", &self.configured)
            .finish_non_exhaustive()
    }
}

impl LiveTelnyxProvider {
    pub fn new(api_key: &str, connection_id: &str, allow_outbound: bool) -> Self {
        Self::with_base_url(api_key, connection_id, allow_outbound, API)
    }

    /// The same client against another base URL (tests use a local mock server).
    pub fn with_base_url(
        api_key: &str,
        connection_id: &str,
        allow_outbound: bool,
        base_url: &str,
    ) -> Self {
        Self {
            api_key: api_key.to_owned(),
            connection_id: connection_id.to_owned(),
            allow_outbound,
            configured: !api_key.is_empty() && !connection_id.is_empty(),
            preflight_missing: Vec::new(),
            target: None,
            base_url: base_url.trim_end_matches('/').to_owned(),
            client: HttpClient::new(PROVIDER, policy(), Self::auth_headers(api_key)),
        }
    }

    /// The settings (names only) without which this client refuses to dial: see
    /// [`Settings::voice_live_missing`]. [`Voice::from_settings`] always sets it.
    pub fn with_preflight(mut self, missing: Vec<&'static str>) -> Self {
        self.preflight_missing = missing;
        self
    }

    /// Whom this client may ring: with [`LiveCallTarget::TestNumber`] every dial goes to that
    /// number instead of the one asked for; with [`LiveCallTarget::Nobody`] every dial is refused.
    pub fn with_call_target(mut self, target: LiveCallTarget) -> Self {
        self.target = Some(target);
        self
    }

    /// The text of the refusal for these missing settings (names only, never a value).
    pub fn preflight_refusal(missing: &[&str]) -> String {
        format!(
            "live call not dialed, nothing was sent to the carrier: these settings are missing \
             or unusable: {} (the server could not receive or verify the call's events; \
             PUBLIC_BASE_URL must be https://<public host>, no path, no trailing slash)",
            missing.join(", ")
        )
    }

    fn auth_headers(api_key: &str) -> Vec<(String, String)> {
        vec![("Authorization".to_owned(), format!("Bearer {api_key}"))]
    }

    /// `_post(path, body, idem)`: the answer's `data` object (`{}` when absent or empty).
    fn post(
        &self,
        path: &str,
        body: &Value,
        idem: Option<&str>,
    ) -> Result<serde_json::Map<String, Value>, ProviderError> {
        let url = format!("{}{path}", self.base_url);
        let with_key;
        let client = match idem {
            Some(k) => {
                let mut headers = Self::auth_headers(&self.api_key);
                headers.push(("Idempotency-Key".to_owned(), k.to_owned()));
                with_key = HttpClient::new(PROVIDER, policy(), headers);
                &with_key
            }
            None => &self.client,
        };
        let resp = client.with_retries(|| client.post_json_once(&url, body))?;
        let doc = resp.json(PROVIDER)?;
        Ok(match doc.get("data") {
            Some(Value::Object(m)) => m.clone(),
            _ => serde_json::Map::new(),
        })
    }

    /// `dial` and `dial_streaming`: the same body, plus the stream fields when asked for.
    fn dial_request(
        &self,
        to: &str,
        from: &str,
        webhook_url: &str,
        idempotency_key: &str,
        client_state: Option<&IndexMap<String, Value>>,
        stream: Option<&StreamOptions>,
    ) -> Result<DialResult, TelephonyError> {
        if !self.allow_outbound {
            return Err(TelephonyError::Forbidden(
                "live outbound calls disabled (ALLOW_LIVE_OUTBOUND_CALLS=false)".into(),
            ));
        }
        // Before any request: a phone must not ring while the server can hear nothing about the
        // call (no public address for the webhooks, no key to verify them, no caller number).
        if !self.preflight_missing.is_empty() {
            return Err(TelephonyError::Forbidden(Self::preflight_refusal(
                &self.preflight_missing,
            )));
        }
        // Whom it may ring: the owner's test number instead of whatever was asked for, the
        // contact only with the second switch, otherwise nobody. The number asked for is never
        // sent while a test number is set.
        let test_number = match &self.target {
            Some(LiveCallTarget::TestNumber(n)) => Some(n.as_str()),
            Some(LiveCallTarget::Nobody) => {
                return Err(TelephonyError::Forbidden(ONLY_TO_REFUSAL.into()));
            }
            Some(LiveCallTarget::Contacts) | None => None,
        };
        let to = test_number.unwrap_or(to);
        let mut body = json!({
            "connection_id": self.connection_id,
            "to": to,
            "from": from,
            "webhook_url": webhook_url,
            "answering_machine_detection": "detect",
            "timeout_secs": 40,
        });
        if let Some(cs) = client_state.filter(|c| !c.is_empty()) {
            body["client_state"] = Value::String(encode_client_state(cs));
        }
        if let (Some(stream), Some(m)) = (stream, body.as_object_mut()) {
            m.extend(stream.to_fields());
        }
        // A carrier's error text may repeat the number dialed: the test number is private.
        let data = self
            .post("/calls", &body, Some(idempotency_key))
            .map_err(|mut e| {
                if let Some(n) = test_number {
                    let masked = mask_number(n);
                    e.message = e
                        .message
                        .replace(n, &masked)
                        .replace(n.trim_start_matches('+'), &masked);
                }
                e
            })?;
        Ok(DialResult {
            call_control_id: Self::field(&data, "call_control_id")?,
            state: "initiated".into(),
        })
    }

    /// A call command without a result (`POST /calls/{id}/actions/{action}`).
    fn action(
        &self,
        call_control_id: &str,
        action: &str,
        body: &Value,
    ) -> Result<(), TelephonyError> {
        self.post(
            &format!("/calls/{}/actions/{action}", path_segment(call_control_id)),
            body,
            None,
        )?;
        Ok(())
    }

    fn field(data: &serde_json::Map<String, Value>, key: &str) -> Result<String, ProviderError> {
        match data.get(key) {
            Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
            _ => Err(ProviderError::new(
                PROVIDER,
                ProviderErrorKind::BadResponse,
                format!("response without data.{key}"),
            )),
        }
    }
}

impl TelephonyProvider for LiveTelnyxProvider {
    fn mode(&self) -> &str {
        "live"
    }

    fn dial(
        &self,
        to: &str,
        from: &str,
        webhook_url: &str,
        idempotency_key: &str,
        client_state: Option<&IndexMap<String, Value>>,
    ) -> Result<DialResult, TelephonyError> {
        self.dial_request(to, from, webhook_url, idempotency_key, client_state, None)
    }

    fn speak(&self, call_control_id: &str, text: &str) -> Result<(), TelephonyError> {
        self.post(
            &format!("/calls/{}/actions/speak", path_segment(call_control_id)),
            &json!({"payload": text, "voice": VOICE, "language": "en-US"}),
            None,
        )?;
        Ok(())
    }

    fn play(&self, call_control_id: &str, audio_url: &str) -> Result<(), TelephonyError> {
        self.post(
            &format!(
                "/calls/{}/actions/playback_start",
                path_segment(call_control_id)
            ),
            &json!({"audio_url": audio_url}),
            None,
        )?;
        Ok(())
    }

    fn start_transcription(&self, call_control_id: &str) -> Result<(), TelephonyError> {
        self.post(
            &format!(
                "/calls/{}/actions/transcription_start",
                path_segment(call_control_id)
            ),
            &json!({"transcription_engine": "Telnyx"}),
            None,
        )?;
        Ok(())
    }

    fn hangup(&self, call_control_id: &str) -> Result<(), TelephonyError> {
        self.post(
            &format!("/calls/{}/actions/hangup", path_segment(call_control_id)),
            &json!({}),
            None,
        )?;
        Ok(())
    }

    fn create_conference(
        &self,
        call_control_id: &str,
        name: &str,
    ) -> Result<String, TelephonyError> {
        let data = self.post(
            "/conferences",
            &json!({"call_control_id": call_control_id, "name": name}),
            None,
        )?;
        Ok(Self::field(&data, "id")?)
    }

    fn join_conference(
        &self,
        conference_id: &str,
        call_control_id: &str,
    ) -> Result<(), TelephonyError> {
        self.post(
            &format!("/conferences/{}/actions/join", path_segment(conference_id)),
            &json!({"call_control_id": call_control_id}),
            None,
        )?;
        Ok(())
    }

    fn dial_streaming(
        &self,
        to: &str,
        from: &str,
        webhook_url: &str,
        idempotency_key: &str,
        client_state: Option<&IndexMap<String, Value>>,
        stream: &StreamOptions,
    ) -> Result<DialResult, TelephonyError> {
        self.dial_request(
            to,
            from,
            webhook_url,
            idempotency_key,
            client_state,
            Some(stream),
        )
    }

    fn start_streaming(
        &self,
        call_control_id: &str,
        stream: &StreamOptions,
    ) -> Result<(), TelephonyError> {
        self.action(
            call_control_id,
            "streaming_start",
            &Value::Object(stream.to_fields()),
        )
    }

    fn stop_streaming(
        &self,
        call_control_id: &str,
        stream_id: Option<&str>,
    ) -> Result<(), TelephonyError> {
        let body = match stream_id {
            Some(id) => json!({"stream_id": id}),
            None => json!({}),
        };
        self.action(call_control_id, "streaming_stop", &body)
    }

    fn stop_transcription(&self, call_control_id: &str) -> Result<(), TelephonyError> {
        self.action(call_control_id, "transcription_stop", &json!({}))
    }

    fn stop_playback(&self, call_control_id: &str) -> Result<(), TelephonyError> {
        self.action(call_control_id, "playback_stop", &json!({"stop": "all"}))
    }

    fn status(&self) -> ProviderStatusEntry {
        status_entry(
            PROVIDER,
            "live",
            IntegrationStatus::Partial,
            self.configured,
            &format!(
                "Call flow (dial, intro, transcription turns, handoff conference) per API reference; \
                 never dialed from CI{}.",
                if self.allow_outbound {
                    ""
                } else {
                    "; live outbound disabled"
                }
            ),
        )
    }
}

/// The selected telephony provider (`c.telephony`). `fixture` is the same provider as
/// `telephony` when the fixture is selected, so tests can read what it recorded.
#[derive(Clone)]
pub struct Voice {
    pub telephony: Arc<dyn TelephonyProvider>,
    pub fixture: Option<Arc<FixtureTelephonyProvider>>,
}

impl Voice {
    /// `build_container`'s choice.
    pub fn from_settings(s: &Settings) -> Self {
        Self::from_settings_with_base_url(s, API)
    }

    /// [`Voice::from_settings`] against another carrier address (tests use a local mock server).
    /// The live client carries the settings' preflight, so every live dial passes it.
    pub fn from_settings_with_base_url(s: &Settings, base_url: &str) -> Self {
        match (&s.telnyx_api_key, s.voice_provider_mode) {
            (Some(key), ProviderMode::Live) if !key.is_empty() => Self::live(Arc::new(
                LiveTelnyxProvider::with_base_url(
                    key.expose(),
                    s.telnyx_connection_id.as_deref().unwrap_or(""),
                    s.allow_live_outbound_calls,
                    base_url,
                )
                .with_preflight(s.voice_live_missing())
                .with_call_target(s.live_call_target()),
            )),
            _ => Self::fixture(),
        }
    }

    pub fn fixture() -> Self {
        let f = Arc::new(FixtureTelephonyProvider::new());
        Self {
            telephony: f.clone(),
            fixture: Some(f),
        }
    }

    pub fn live(provider: Arc<dyn TelephonyProvider>) -> Self {
        Self {
            telephony: provider,
            fixture: None,
        }
    }

    pub fn mode(&self) -> String {
        self.telephony.mode().to_owned()
    }
}

/// `xai_voice.status(bool(c.settings.xai_api_key))`.
struct XaiVoiceStatus;

impl ProviderStatus for XaiVoiceStatus {
    fn name(&self) -> &str {
        "xai_voice"
    }
    fn status(&self, settings: &Settings) -> ProviderStatusEntry {
        ss_domain::integrations::xai_voice::status(settings.has("XAI_API_KEY"))
    }
}

/// Put the selected voice provider on the application state and report `telnyx` and
/// `xai_voice` in `/ready` as Python does (replacing the `NOT_STARTED` placeholders in place).
pub fn register(app: &App, voice: Voice) {
    let telephony = voice.telephony.clone();
    app.providers.register(Arc::new(Reported {
        name: "telnyx",
        status: Box::new(move || telephony.status()),
    }));
    app.providers.register(Arc::new(XaiVoiceStatus));
    app.extensions.insert(Arc::new(voice));
}

impl App {
    /// The selected voice provider. Tests replace it with
    /// `app.extensions.insert(Arc::new(Voice::...))`.
    pub fn voice(&self) -> Arc<Voice> {
        match self.extensions.get::<Voice>() {
            Some(v) => v,
            None => {
                let v = Arc::new(Voice::from_settings(&self.settings));
                self.extensions.insert(v.clone());
                v
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_cannot_change_the_path() {
        assert_eq!(path_segment("v3:abc-_.~"), "v3:abc-_.~");
        assert_eq!(path_segment("a/../b?x=1"), "a%2F..%2Fb%3Fx%3D1");
    }
}
