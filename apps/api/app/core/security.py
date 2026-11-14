"""Minimal HMAC-signed bearer tokens for the hackathon build.

Production should replace this with a real IdP (documented in docs/INFRASTRUCTURE_DECISIONS.md).
Tokens: base64url(purpose.user_id.exp).hexsig — no external dependency, constant-time verification.
The purpose is signed, so a token verifies only for the purpose it was minted for.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import secrets
import time
from uuid import UUID

from .errors import UnauthorizedError

# Ordinary bearer tokens (web app, dev token). `api.deps.current_user` accepts only this purpose.
ACCESS_PURPOSE = "access"
# Rust service -> this app, internal routes only (`api/routes/internal.py`); user id = the owner it acts for.
# Never accepted as an access token, and an access token is never accepted where this purpose is required.
SERVICE_PURPOSE = "service"


def _sign(secret: str, body: str) -> str:
    return hmac.new(secret.encode(), body.encode(), hashlib.sha256).hexdigest()


def issue_token(secret: str, user_id: UUID, ttl_s: int = 7 * 24 * 3600, purpose: str = ACCESS_PURPOSE) -> str:
    body = f"{purpose}.{user_id}.{int(time.time()) + ttl_s}"
    enc = base64.urlsafe_b64encode(body.encode()).decode().rstrip("=")
    return f"{enc}.{_sign(secret, body)}"


def verify_token(secret: str, token: str, purpose: str = ACCESS_PURPOSE) -> UUID:
    try:
        enc, sig = token.rsplit(".", 1)
        body = base64.urlsafe_b64decode(enc + "=" * (-len(enc) % 4)).decode()
        tok_purpose, user, exp = body.split(".")
    except Exception as exc:  # noqa: BLE001
        raise UnauthorizedError("malformed token") from exc
    if not hmac.compare_digest(_sign(secret, body), sig):
        raise UnauthorizedError("invalid token signature")
    if tok_purpose != purpose:
        raise UnauthorizedError("token purpose mismatch")
    if int(exp) < time.time():
        raise UnauthorizedError("token expired")
    return UUID(user)


def opaque_reference() -> str:
    """Random reference stored in events instead of real secrets/tokens."""
    return "ref_" + secrets.token_urlsafe(16)


def sign_media_key(secret: str, key: str, ttl_s: int = 900) -> str:
    """Short-lived capability for one stored object (e.g. the owner's recorded intro fetched by the carrier)."""
    body = f"media.{key}.{int(time.time()) + ttl_s}"
    enc = base64.urlsafe_b64encode(body.encode()).decode().rstrip("=")
    return f"{enc}.{_sign(secret, body)}"


def verify_media_key(secret: str, token: str) -> str:
    try:
        enc, sig = token.rsplit(".", 1)
        body = base64.urlsafe_b64decode(enc + "=" * (-len(enc) % 4)).decode()
        purpose, rest = body.split(".", 1)
        key, exp = rest.rsplit(".", 1)
    except ValueError as exc:
        raise UnauthorizedError("invalid media token") from exc
    if purpose != "media" or not hmac.compare_digest(sig, _sign(secret, body)) or int(exp) < time.time():
        raise UnauthorizedError("invalid or expired media token")
    return key
