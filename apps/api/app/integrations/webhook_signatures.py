"""Webhook signature verification.

- Standard Webhooks (used by Photon 2.0 beta and xAI voice SIP webhooks): HMAC-SHA256 over
  "{webhook-id}.{webhook-timestamp}.{raw_body}" with the base64-decoded `whsec_` secret; header
  `webhook-signature` holds space-separated "v1,<base64>" entries; 5-minute timestamp tolerance.
- Telnyx: Ed25519 signature (`telnyx-signature-ed25519`, base64) over "{telnyx-timestamp}|{raw_body}"
  verified with the account public key (`TELNYX_PUBLIC_KEY`, base64). Message format follows the
  Telnyx SDK helper; confirm against a live delivery before demo (PARTIAL).

Results carry `mode` so a fixture-secret verification can never be mistaken for live verification.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import time
from dataclasses import dataclass

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

from app.core.errors import InvalidSignatureError

TOLERANCE_S = 300


@dataclass(frozen=True)
class VerifiedWebhook:
    provider: str
    event_id: str
    mode: str  # "live" | "fixture"


def _lower(headers) -> dict[str, str]:
    return {k.lower(): v for k, v in dict(headers).items()}


def standard_webhooks_sign(secret: str, msg_id: str, timestamp: int, body: bytes) -> str:
    key = base64.b64decode(secret.removeprefix("whsec_"))
    signed = f"{msg_id}.{timestamp}.".encode() + body
    return "v1," + base64.b64encode(hmac.new(key, signed, hashlib.sha256).digest()).decode()


def verify_standard_webhook(
    provider: str, secret: str | None, headers, body: bytes, mode: str, now: float | None = None
) -> VerifiedWebhook:
    if not secret:
        raise InvalidSignatureError(f"{provider} webhook secret not configured")
    h = _lower(headers)
    msg_id, ts, sig_header = h.get("webhook-id"), h.get("webhook-timestamp"), h.get("webhook-signature")
    if not (msg_id and ts and sig_header):
        raise InvalidSignatureError("missing webhook-id / webhook-timestamp / webhook-signature")
    try:
        ts_i = int(ts)
    except ValueError as exc:
        raise InvalidSignatureError("bad webhook-timestamp") from exc
    if abs((now or time.time()) - ts_i) > TOLERANCE_S:
        raise InvalidSignatureError("webhook timestamp outside tolerance")
    expected = standard_webhooks_sign(secret, msg_id, ts_i, body).split(",", 1)[1]
    for entry in sig_header.split():
        version, _, sig = entry.partition(",")
        if version == "v1" and hmac.compare_digest(sig, expected):
            return VerifiedWebhook(provider, msg_id, mode)
    raise InvalidSignatureError("signature mismatch")


def verify_telnyx(
    public_key_b64: str | None, headers, body: bytes, mode: str, now: float | None = None
) -> VerifiedWebhook:
    if not public_key_b64:
        raise InvalidSignatureError("TELNYX_PUBLIC_KEY not configured")
    h = _lower(headers)
    sig, ts = h.get("telnyx-signature-ed25519"), h.get("telnyx-timestamp")
    if not (sig and ts):
        raise InvalidSignatureError("missing telnyx-signature-ed25519 / telnyx-timestamp")
    if abs((now or time.time()) - int(ts)) > TOLERANCE_S:
        raise InvalidSignatureError("webhook timestamp outside tolerance")
    try:
        key = Ed25519PublicKey.from_public_bytes(base64.b64decode(public_key_b64))
        key.verify(base64.b64decode(sig), f"{ts}|".encode() + body)
    except (InvalidSignature, ValueError) as exc:
        raise InvalidSignatureError("telnyx signature mismatch") from exc
    import json

    try:
        event_id = json.loads(body)["data"]["id"]
    except Exception as exc:  # noqa: BLE001
        raise InvalidSignatureError("telnyx payload missing data.id") from exc
    return VerifiedWebhook("telnyx", str(event_id), mode)
