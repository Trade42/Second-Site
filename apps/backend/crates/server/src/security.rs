//! Port of `apps/api/app/core/security.py`: HMAC-SHA256 bearer tokens and media capabilities,
//! byte-compatible with Python (a token minted by either backend verifies in the other).
//!
//! Token: `base64url_nopad("{purpose}.{user_id}.{exp}") + "." + hex(hmac_sha256(secret, body))`.
//! Media key: the same with body `"media.{key}.{exp}"`.
//!
//! Decoding reproduces `base64.urlsafe_b64decode` (non-strict `binascii.a2b_base64`) exactly.
//! Unlike Python, every malformed input is a 401: Python raises a 500 for a non-ASCII signature
//! (`hmac.compare_digest` TypeError) and for a validly signed token whose `exp` or user id does not
//! parse.

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

type HmacSha256 = Hmac<Sha256>;

pub const ACCESS_TTL_S: i64 = 7 * 24 * 3600;
pub const MEDIA_TTL_S: i64 = 900;

/// `_sign(secret, body)`: lower-case hex HMAC-SHA256.
pub fn sign(secret: &str, body: &str) -> String {
    match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mut mac) => {
            mac.update(body.as_bytes());
            hex::encode(mac.finalize().into_bytes())
        }
        // HMAC accepts keys of any length; this branch is unreachable in practice.
        Err(_) => String::new(),
    }
}

fn now_f64() -> f64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    d.as_secs_f64()
}

fn now_i64() -> i64 {
    now_f64().floor() as i64
}

fn b64url_nopad(body: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body.as_bytes())
}

/// Constant-time equality of two byte strings (length is not secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `base64.urlsafe_b64decode(enc + "=" * (-len(enc) % 4))`, reproducing CPython's non-strict
/// `binascii.a2b_base64`: characters outside the alphabet are skipped, decoding stops at a
/// completed pad sequence, a dangling quad is an error.
pub fn py_urlsafe_b64decode_padded(enc: &str) -> Result<Vec<u8>, &'static str> {
    if !enc.is_ascii() {
        return Err("string argument should contain only ASCII characters");
    }
    let pad = (4 - enc.len() % 4) % 4;
    let mut input: Vec<u8> = enc
        .bytes()
        .map(|b| match b {
            b'-' => b'+',
            b'_' => b'/',
            other => other,
        })
        .collect();
    input.extend(std::iter::repeat_n(b'=', pad));

    fn value(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut quad_pos = 0u8;
    let mut leftchar = 0u8;
    let mut pads = 0u8;
    for &c in &input {
        if c == b'=' {
            if quad_pos >= 2 {
                pads += 1;
                if quad_pos + pads >= 4 {
                    return Ok(out);
                }
            }
            continue;
        }
        let Some(v) = value(c) else {
            continue;
        };
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                leftchar = v;
            }
            1 => {
                quad_pos = 2;
                out.push((leftchar << 2) | (v >> 4));
                leftchar = v & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push((leftchar << 4) | (v >> 2));
                leftchar = v & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push((leftchar << 6) | v);
                leftchar = 0;
            }
        }
    }
    match quad_pos {
        0 => Ok(out),
        1 => Err("Invalid base64-encoded string"),
        _ => Err("Incorrect padding"),
    }
}

fn decode_body(enc: &str) -> Option<String> {
    let bytes = py_urlsafe_b64decode_padded(enc).ok()?;
    String::from_utf8(bytes).ok()
}

/// Python `int(s)` for the subset a token can carry: surrounding whitespace, optional sign,
/// ASCII digits with single underscores between digits. Returns `None` when Python would raise.
/// Values beyond `i128` saturate (only the sign matters for an expiry check).
fn py_int(s: &str) -> Option<i128> {
    let t = s.trim();
    let (neg, digits) = match t.as_bytes().first()? {
        b'+' => (false, &t[1..]),
        b'-' => (true, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    let mag = clean.parse::<i128>().unwrap_or(i128::MAX);
    Some(if neg { -mag } else { mag })
}

/// `int(exp) < time.time()`.
fn is_expired(exp: i128, now: f64) -> bool {
    (exp as f64) < now
}

/// `issue_token(secret, user_id, ttl_s, purpose)` with an explicit expiry (seconds since epoch).
pub fn issue_token_at(secret: &str, user_id: Uuid, purpose: &str, exp: i64) -> String {
    let body = format!("{purpose}.{user_id}.{exp}");
    format!("{}.{}", b64url_nopad(&body), sign(secret, &body))
}

/// `issue_token(secret, user_id, ttl_s, purpose)`.
pub fn issue_token_with(secret: &str, user_id: Uuid, ttl_s: i64, purpose: &str) -> String {
    issue_token_at(secret, user_id, purpose, now_i64() + ttl_s)
}

/// `issue_token(secret, user_id)`: seven-day access token.
pub fn issue_token(secret: &str, user_id: Uuid) -> String {
    issue_token_with(secret, user_id, ACCESS_TTL_S, "access")
}

/// `verify_token(secret, token, purpose)` at an explicit time.
pub fn verify_token_at(secret: &str, token: &str, purpose: &str, now: f64) -> AppResult<Uuid> {
    let malformed = || AppError::unauthorized("malformed token");
    let (enc, sig) = token.rsplit_once('.').ok_or_else(malformed)?;
    let body = decode_body(enc).ok_or_else(malformed)?;
    let parts: Vec<&str> = body.split('.').collect();
    let [tok_purpose, user, exp] = parts.as_slice() else {
        return Err(malformed());
    };
    if !ct_eq(sign(secret, &body).as_bytes(), sig.as_bytes()) {
        return Err(AppError::unauthorized("invalid token signature"));
    }
    if *tok_purpose != purpose {
        return Err(AppError::unauthorized("token purpose mismatch"));
    }
    let exp = py_int(exp).ok_or_else(malformed)?;
    if is_expired(exp, now) {
        return Err(AppError::unauthorized("token expired"));
    }
    Uuid::parse_str(user).map_err(|_| malformed())
}

/// `verify_token(secret, token, purpose)`.
pub fn verify_token(secret: &str, token: &str, purpose: &str) -> AppResult<Uuid> {
    verify_token_at(secret, token, purpose, now_f64())
}

/// `sign_media_key(secret, key, ttl_s)` with an explicit expiry.
pub fn sign_media_key_at(secret: &str, key: &str, exp: i64) -> String {
    let body = format!("media.{key}.{exp}");
    format!("{}.{}", b64url_nopad(&body), sign(secret, &body))
}

/// `sign_media_key(secret, key, ttl_s=900)`.
pub fn sign_media_key(secret: &str, key: &str, ttl_s: i64) -> String {
    sign_media_key_at(secret, key, now_i64() + ttl_s)
}

/// `verify_media_key(secret, token)` at an explicit time.
pub fn verify_media_key_at(secret: &str, token: &str, now: f64) -> AppResult<String> {
    let invalid = || AppError::unauthorized("invalid media token");
    let expired = || AppError::unauthorized("invalid or expired media token");
    let (enc, sig) = token.rsplit_once('.').ok_or_else(invalid)?;
    let body = decode_body(enc).ok_or_else(invalid)?;
    let (purpose, rest) = body.split_once('.').ok_or_else(invalid)?;
    let (key, exp) = rest.rsplit_once('.').ok_or_else(invalid)?;
    if purpose != "media" || !ct_eq(sig.as_bytes(), sign(secret, &body).as_bytes()) {
        return Err(expired());
    }
    let exp = py_int(exp).ok_or_else(expired)?;
    if is_expired(exp, now) {
        return Err(expired());
    }
    Ok(key.to_owned())
}

/// `verify_media_key(secret, token)`.
pub fn verify_media_key(secret: &str, token: &str) -> AppResult<String> {
    verify_media_key_at(secret, token, now_f64())
}

/// `opaque_reference()`: `"ref_" + secrets.token_urlsafe(16)`.
pub fn opaque_reference() -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!(
        "ref_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let uid = Uuid::new_v4();
        let t = issue_token("s", uid);
        assert_eq!(verify_token("s", &t, "access").unwrap(), uid);
        assert!(verify_token("other", &t, "access").is_err());
        assert!(verify_token("s", &t, "media").is_err());
        let m = sign_media_key("s", "obj/a.b.m4a", 60);
        assert_eq!(verify_media_key("s", &m).unwrap(), "obj/a.b.m4a");
    }

    #[test]
    fn python_base64_quirks() {
        assert_eq!(py_urlsafe_b64decode_padded("YQ").unwrap(), b"a");
        assert!(py_urlsafe_b64decode_padded("Y!Q").is_err());
        assert_eq!(py_urlsafe_b64decode_padded("Y!WJj").unwrap(), b"abc");
        assert_eq!(py_urlsafe_b64decode_padded("YQ==YWJj").unwrap(), b"a");
        assert!(py_urlsafe_b64decode_padded("Y").is_err());
        assert!(py_urlsafe_b64decode_padded("é").is_err());
    }

    #[test]
    fn py_int_subset() {
        assert_eq!(py_int(" +1_000 "), Some(1000));
        assert_eq!(py_int("-5"), Some(-5));
        assert_eq!(py_int("1__0"), None);
        assert_eq!(py_int("1.0"), None);
        assert_eq!(py_int(""), None);
    }

    #[test]
    fn opaque_reference_shape() {
        let r = opaque_reference();
        assert!(r.starts_with("ref_") && r.len() == 26);
    }
}
