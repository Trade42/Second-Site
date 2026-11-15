//! Port of `apps/api/app/integrations/webhook_signatures.py`: webhook signature verification on
//! the raw body bytes.
//!
//! - **Standard Webhooks** (Photon 2.0 beta, xAI voice SIP webhooks): HMAC-SHA256 over
//!   `"{webhook-id}.{webhook-timestamp}.{raw_body}"` with the base64-decoded `whsec_` secret;
//!   header `webhook-signature` holds space-separated `v1,<base64>` entries; 5-minute timestamp
//!   tolerance.
//! - **Telnyx**: Ed25519 signature (`telnyx-signature-ed25519`, base64) over
//!   `"{telnyx-timestamp}|{raw_body}"`, verified with the account public key
//!   (`TELNYX_PUBLIC_KEY`, base64), through the maintained `ed25519-dalek` crate. The message
//!   format follows the Telnyx SDK helper; Python marks it PARTIAL until confirmed against a live
//!   delivery.
//!
//! **Modes.** Verification is identical in live and fixture mode: a request is accepted only with
//! a correct signature made with the configured secret or key, a timestamp within
//! [`TOLERANCE_S`] seconds, and (Telnyx) a `data.id`. With no secret or key configured, both modes
//! refuse. The only difference is the label: the result carries [`WebhookMode`], so an event
//! verified with a fixture secret (`mode = fixture`) can never be mistaken for a verified live
//! event; [`VerifiedWebhook::is_live_verified`] is true only in live mode (Python stores
//! `verified = mode == "live"`).
//!
//! **Headers.** Names are matched case-insensitively. When a name occurs more than once, the first
//! occurrence wins (Starlette's `dict(request.headers)`). Values must be the latin-1 decoding of
//! the raw header bytes, as Starlette gives them to Python ([`header_value_latin1`]).
//!
//! **Errors.** [`WebhookError::InvalidSignature`] is Python's `InvalidSignatureError` (HTTP 400,
//! `INVALID_SIGNATURE`), with Python's message. [`WebhookError::Unhandled`] is an exception Python
//! does not catch, which its API answers with 500 `INTERNAL_ERROR`: a configured secret that is
//! not valid base64, a non-integer or astronomically large Telnyx timestamp, a non-ASCII `v1`
//! signature compared with `hmac.compare_digest`, or a Standard Webhooks timestamp too large to
//! convert to a float.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::py;

/// Timestamp tolerance in seconds (`TOLERANCE_S`).
pub const TOLERANCE_S: i64 = 300;

/// Header names (lower case).
pub const WEBHOOK_ID: &str = "webhook-id";
pub const WEBHOOK_TIMESTAMP: &str = "webhook-timestamp";
pub const WEBHOOK_SIGNATURE: &str = "webhook-signature";
pub const TELNYX_SIGNATURE: &str = "telnyx-signature-ed25519";
pub const TELNYX_TIMESTAMP: &str = "telnyx-timestamp";

/// The provider mode a verification ran in (Python's `mode: "live" | "fixture"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookMode {
    Live,
    Fixture,
}

impl WebhookMode {
    pub fn as_str(self) -> &'static str {
        match self {
            WebhookMode::Live => "live",
            WebhookMode::Fixture => "fixture",
        }
    }
}

/// `VerifiedWebhook`: the signature checked out. `event_id` is the provider's event id
/// (`webhook-id`, or Telnyx's `data.id` as `str()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWebhook {
    pub provider: String,
    pub event_id: String,
    pub mode: WebhookMode,
}

impl VerifiedWebhook {
    /// True only for a live-mode verification (Python stores `verified = mode == "live"`). A
    /// fixture-secret verification is never a verified live event.
    pub fn is_live_verified(&self) -> bool {
        self.mode == WebhookMode::Live
    }
}

/// Why a webhook was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    /// Python `InvalidSignatureError`: HTTP 400, `INVALID_SIGNATURE`.
    #[error("{0}")]
    InvalidSignature(String),
    /// An exception Python does not catch: HTTP 500, `INTERNAL_ERROR`.
    #[error("{0}")]
    Unhandled(String),
}

impl WebhookError {
    /// HTTP status of Python's response.
    pub fn status_code(&self) -> u16 {
        match self {
            WebhookError::InvalidSignature(_) => 400,
            WebhookError::Unhandled(_) => 500,
        }
    }

    /// `error_code` of Python's `ProblemDetail`.
    pub fn error_code(&self) -> &'static str {
        match self {
            WebhookError::InvalidSignature(_) => "INVALID_SIGNATURE",
            WebhookError::Unhandled(_) => "INTERNAL_ERROR",
        }
    }
}

fn invalid(msg: impl Into<String>) -> WebhookError {
    WebhookError::InvalidSignature(msg.into())
}

/// A raw header value as Starlette decodes it (latin-1: each byte is one character).
pub fn header_value_latin1(raw: &[u8]) -> String {
    raw.iter().map(|&b| char::from(b)).collect()
}

/// `_lower(headers).get(name)` with Starlette's semantics: names compared case-insensitively,
/// the first occurrence wins.
fn header<'h, K, V>(headers: &'h [(K, V)], name: &str) -> Option<&'h str>
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    headers
        .iter()
        .find(|(k, _)| k.as_ref().to_lowercase() == name)
        .map(|(_, v)| v.as_ref())
}

/// Python's `now or time.time()`: `None` or `0.0` mean the current time (NaN is truthy, as in
/// Python).
fn now_or_time(now: Option<f64>) -> f64 {
    match now {
        Some(n) if n != 0.0 => n,
        _ => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64()),
    }
}

/// `int(ts)` as Python then uses it: its canonical decimal text (`str(int(ts))`) and its value
/// converted to a float (`Err` with Python's `OverflowError` message when too large).
fn timestamp(ts: &str) -> Result<(String, Result<f64, String>), String> {
    let canonical = py::int_literal(ts)?;
    let value = canonical
        .parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .ok_or_else(|| "int too large to convert to float".to_owned());
    Ok((canonical, value))
}

/// `abs(now - ts) > TOLERANCE_S` with Python's float arithmetic.
fn outside_tolerance(now: f64, ts: f64) -> bool {
    (now - ts).abs() > TOLERANCE_S as f64
}

/// HMAC-SHA256 of `"{msg_id}.{timestamp}."` + body, keyed by the decoded secret, base64.
fn sign_canonical(
    secret: &str,
    msg_id: &str,
    timestamp: &str,
    body: &[u8],
) -> Result<String, String> {
    let key = py::b64decode(secret.strip_prefix("whsec_").unwrap_or(secret))?;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key)
        .map_err(|e| format!("HMAC key rejected: {e}"))?;
    mac.update(format!("{msg_id}.{timestamp}.").as_bytes());
    mac.update(body);
    Ok(format!(
        "v1,{}",
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    ))
}

/// `standard_webhooks_sign(secret, msg_id, timestamp, body)`: `"v1,<base64 HMAC-SHA256>"`.
/// `Err` (Python's `binascii.Error` message) when the secret, after an optional `whsec_` prefix,
/// is not base64.
pub fn standard_webhooks_sign(
    secret: &str,
    msg_id: &str,
    timestamp: i64,
    body: &[u8],
) -> Result<String, String> {
    sign_canonical(secret, msg_id, &timestamp.to_string(), body)
}

/// `verify_standard_webhook(provider, secret, headers, body, mode, now)`, in Python's order:
///
/// 1. no (or an empty) secret: `"<provider> webhook secret not configured"`;
/// 2. a missing or empty `webhook-id`, `webhook-timestamp` or `webhook-signature`:
///    `"missing webhook-id / webhook-timestamp / webhook-signature"`;
/// 3. a timestamp `int()` rejects: `"bad webhook-timestamp"`;
/// 4. more than 300 s from `now` (default: the current time): `"webhook timestamp outside
///    tolerance"`;
/// 5. the expected signature is computed over the canonical integer text of the timestamp
///    (`str(int(ts))`), the id and the raw body; the first whitespace-separated `v1,<sig>` entry
///    equal to it (constant-time) accepts; anything else: `"signature mismatch"`.
pub fn verify_standard_webhook<K, V>(
    provider: &str,
    secret: Option<&str>,
    headers: &[(K, V)],
    body: &[u8],
    mode: WebhookMode,
    now: Option<f64>,
) -> Result<VerifiedWebhook, WebhookError>
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return Err(invalid(format!("{provider} webhook secret not configured")));
    };
    let present = |name: &str| header(headers, name).filter(|v| !v.is_empty());
    let (Some(msg_id), Some(ts), Some(sig_header)) = (
        present(WEBHOOK_ID),
        present(WEBHOOK_TIMESTAMP),
        present(WEBHOOK_SIGNATURE),
    ) else {
        return Err(invalid(
            "missing webhook-id / webhook-timestamp / webhook-signature",
        ));
    };
    let (canonical_ts, ts_value) = timestamp(ts).map_err(|_| invalid("bad webhook-timestamp"))?;
    let now = now_or_time(now);
    let ts_value = ts_value.map_err(WebhookError::Unhandled)?;
    if outside_tolerance(now, ts_value) {
        return Err(invalid("webhook timestamp outside tolerance"));
    }
    let expected =
        sign_canonical(secret, msg_id, &canonical_ts, body).map_err(WebhookError::Unhandled)?;
    let expected = expected.split_once(',').map_or("", |(_, s)| s);
    for entry in py::split_whitespace(sig_header) {
        let (version, sig) = entry.split_once(',').unwrap_or((entry, ""));
        if version == "v1" {
            if !sig.is_ascii() {
                return Err(WebhookError::Unhandled(
                    "comparing strings with non-ASCII characters is not supported".into(),
                ));
            }
            if bool::from(sig.as_bytes().ct_eq(expected.as_bytes())) {
                return Ok(VerifiedWebhook {
                    provider: provider.to_owned(),
                    event_id: msg_id.to_owned(),
                    mode,
                });
            }
        }
    }
    Err(invalid("signature mismatch"))
}

/// `verify_telnyx(public_key_b64, headers, body, mode, now)`, in Python's order:
///
/// 1. no (or an empty) key: `"TELNYX_PUBLIC_KEY not configured"`;
/// 2. a missing or empty `telnyx-signature-ed25519` or `telnyx-timestamp`:
///    `"missing telnyx-signature-ed25519 / telnyx-timestamp"`;
/// 3. a timestamp `int()` rejects: [`WebhookError::Unhandled`] (Python's `ValueError` is not
///    caught there); more than 300 s from `now`: `"webhook timestamp outside tolerance"`;
/// 4. the key (base64, 32 bytes) and the signature (base64, 64 bytes) are decoded leniently, as
///    Python's `base64.b64decode` does, and the Ed25519 signature is checked over
///    `"{telnyx-timestamp}|"` (the header text as sent) followed by the raw body; any failure:
///    `"telnyx signature mismatch"`;
/// 5. the body must be a JSON object whose `data` object has an `id`, else `"telnyx payload
///    missing data.id"`; the event id is `str(data.id)`.
pub fn verify_telnyx<K, V>(
    public_key_b64: Option<&str>,
    headers: &[(K, V)],
    body: &[u8],
    mode: WebhookMode,
    now: Option<f64>,
) -> Result<VerifiedWebhook, WebhookError>
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    let Some(key_b64) = public_key_b64.filter(|s| !s.is_empty()) else {
        return Err(invalid("TELNYX_PUBLIC_KEY not configured"));
    };
    let present = |name: &str| header(headers, name).filter(|v| !v.is_empty());
    let (Some(sig), Some(ts)) = (present(TELNYX_SIGNATURE), present(TELNYX_TIMESTAMP)) else {
        return Err(invalid(
            "missing telnyx-signature-ed25519 / telnyx-timestamp",
        ));
    };
    let now = now_or_time(now);
    let (_, ts_value) = timestamp(ts).map_err(WebhookError::Unhandled)?;
    let ts_value = ts_value.map_err(WebhookError::Unhandled)?;
    if outside_tolerance(now, ts_value) {
        return Err(invalid("webhook timestamp outside tolerance"));
    }
    let mismatch = || invalid("telnyx signature mismatch");
    let key_bytes: [u8; 32] = py::b64decode(key_b64)
        .ok()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(mismatch)?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| mismatch())?;
    let sig_bytes: [u8; 64] = py::b64decode(sig)
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or_else(mismatch)?;
    let signature = Signature::from_bytes(&sig_bytes);
    let mut message = format!("{ts}|").into_bytes();
    message.extend_from_slice(body);
    // RFC 8032 cofactorless verification with a canonical `S`, as OpenSSL (behind Python's
    // `cryptography`) performs it.
    key.verify(&message, &signature).map_err(|_| mismatch())?;
    let missing = || invalid("telnyx payload missing data.id");
    let doc = py::json_loads_bytes(body).map_err(|_| missing())?;
    let id = doc
        .as_object()
        .and_then(|m| m.get("data"))
        .and_then(|d| d.as_object())
        .and_then(|d| d.get("id"))
        .ok_or_else(missing)?;
    Ok(VerifiedWebhook {
        provider: "telnyx".into(),
        event_id: py::value_str(id),
        mode,
    })
}
