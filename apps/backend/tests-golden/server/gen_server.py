"""Golden vectors for the Rust server crate, produced by the Python implementation.

Run from the repository root:

    (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_server.py)

Writes, next to this script:
- tokens.json        issue/verify vectors for app.core.security (access tokens and media keys)
- payload_hash.json  app.contracts.approvals.payload_hash over JSON payload texts
- approval.json      an ApprovalRequest dumped by Pydantic (hash filled by model_post_init)

Pure functions only: no database, no network. `time.time` is patched to fixed instants.
"""

from __future__ import annotations

import base64
import json
import sys
import uuid
from datetime import UTC, datetime
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[2] / "api"))

from app.contracts.approvals import (  # noqa: E402
    ActionType,
    ApprovalRequest,
    RiskCategory,
    payload_hash,
)
from app.core import security  # noqa: E402
from app.core.errors import UnauthorizedError  # noqa: E402

SECRET = "golden-test-secret"
USER = uuid.UUID("3f2b8c1e-5d4a-4f6b-9a7c-0e1d2c3b4a59")
NOW = 1763150000.75  # 2025-11-14T19:53:20.75Z
FAR = 4102444800  # 2100-01-01


def at(t: float):
    return mock.patch.object(security.time, "time", return_value=t)


def b64(body: bytes) -> str:
    return base64.urlsafe_b64encode(body).decode().rstrip("=")


def signed(body: str) -> str:
    """A token with a valid signature over an arbitrary body (what only the secret holder can make)."""
    return f"{b64(body.encode())}.{security._sign(SECRET, body)}"


def outcome(fn, *args):
    try:
        return {"ok": str(fn(*args))}
    except UnauthorizedError as exc:
        return {"error": "UnauthorizedError", "message": exc.message}
    except Exception as exc:  # noqa: BLE001  (Python answers these with a 500)
        return {"error": type(exc).__name__, "message": str(exc)}


def token_vectors() -> dict:
    issue = []
    for ttl, purpose in [(7 * 24 * 3600, "access"), (60, "access"), (3600, "media")]:
        with at(NOW):
            tok = security.issue_token(SECRET, USER, ttl_s=ttl, purpose=purpose)
        issue.append({"user_id": str(USER), "purpose": purpose, "exp": int(NOW) + ttl, "token": tok})
    with at(NOW):
        default_tok = security.issue_token(SECRET, USER)
    issue.append({"user_id": str(USER), "purpose": "access", "exp": int(NOW) + 7 * 24 * 3600, "token": default_tok})

    with at(NOW - 10):
        expired = security.issue_token(SECRET, USER, ttl_s=5)
    with at(NOW):
        valid = security.issue_token(SECRET, USER, ttl_s=FAR - int(NOW))
    valid_enc, valid_sig = valid.rsplit(".", 1)
    flipped = valid_sig[:-1] + ("0" if valid_sig[-1] != "0" else "1")
    body_tampered = ("B" if valid_enc[0] != "B" else "C") + valid_enc[1:] + "." + valid_sig

    access_cases = [
        ("valid_far_future", valid, "access", NOW),
        ("valid_default_ttl", default_tok, "access", NOW),
        ("expired", expired, "access", NOW),
        ("exp_equal_now_is_valid", signed(f"access.{USER}.{int(NOW)}"), "access", float(int(NOW))),
        ("exp_just_past", signed(f"access.{USER}.{int(NOW)}"), "access", int(NOW) + 0.5),
        ("wrong_purpose_media_token", security.issue_token(SECRET, USER, ttl_s=10**6, purpose="media"), "access", NOW),
        ("wrong_purpose_requested", valid, "media", NOW),
        ("tampered_signature", f"{valid_enc}.{flipped}", "access", NOW),
        ("tampered_body", body_tampered, "access", NOW),
        ("uppercase_signature", f"{valid_enc}.{valid_sig.upper()}", "access", NOW),
        ("wrong_secret", f"{valid_enc}.{security._sign('other-secret', base64.urlsafe_b64decode(valid_enc + '==').decode())}", "access", NOW),
        ("empty", "", "access", NOW),
        ("no_dot", "abc", "access", NOW),
        ("only_dot", ".", "access", NOW),
        ("junk", "junk", "access", NOW),
        ("two_parts_body", signed(f"access.{USER}"), "access", NOW),
        ("four_parts_body", signed(f"access.{USER}.{FAR}.x"), "access", NOW),
        ("bad_base64_length", "Y." + "0" * 64, "access", NOW),
        ("base64_with_junk_chars", valid_enc[:4] + "!" + valid_enc[4:] + "." + valid_sig, "access", NOW),
        ("base64_with_explicit_padding", b64(f"access.{USER}.{FAR}x".encode()) + "==" + "." + "0" * 64, "access", NOW),
        ("non_utf8_body", b64(b"\xff\xfe.\x80.1") + "." + "0" * 64, "access", NOW),
        ("non_ascii_enc", "é" + valid, "access", NOW),
        ("non_ascii_signature", f"{valid_enc}.{valid_sig[:-1]}é", "access", NOW),
        ("signed_non_int_exp", signed(f"access.{USER}.soon"), "access", NOW),
        ("signed_bad_uuid", signed(f"access.not-a-uuid.{FAR}"), "access", NOW),
        ("signed_exp_with_underscores", signed(f"access.{USER}.4_102_444_800"), "access", NOW),
        ("signed_exp_with_spaces", signed(f"access.{USER}. {FAR} "), "access", NOW),
        ("signed_negative_exp", signed(f"access.{USER}.-5"), "access", NOW),
        ("signed_huge_exp", signed(f"access.{USER}.99999999999999999999999999"), "access", NOW),
        ("signed_uuid_simple_form", signed(f"access.{USER.hex}.{FAR}"), "access", NOW),
        ("signed_uuid_uppercase", signed(f"access.{str(USER).upper()}.{FAR}"), "access", NOW),
        ("signed_uuid_braced", signed(f"access.{{{USER}}}.{FAR}"), "access", NOW),
        ("signed_uuid_urn", signed(f"access.urn:uuid:{USER}.{FAR}"), "access", NOW),
    ]
    verify = []
    for name, tok, purpose, now in access_cases:
        with at(now):
            res = outcome(security.verify_token, SECRET, tok, purpose)
        verify.append({"name": name, "token": tok, "purpose": purpose, "now": now, "python": res})

    media_sign = []
    for key in ["obj/owner-intro.m4a", "a.b.c", "интро/é.m4a", "plain"]:
        with at(NOW):
            tok = security.sign_media_key(SECRET, key)
        media_sign.append({"key": key, "exp": int(NOW) + 900, "token": tok})

    with at(NOW):
        m_valid = security.sign_media_key(SECRET, "obj/owner-intro.m4a", ttl_s=FAR - int(NOW))
    with at(NOW - 1000):
        m_expired = security.sign_media_key(SECRET, "obj/owner-intro.m4a")
    m_enc, m_sig = m_valid.rsplit(".", 1)
    m_flip = m_sig[:-1] + ("0" if m_sig[-1] != "0" else "1")
    media_cases = [
        ("valid", m_valid, NOW),
        ("valid_unicode_key", media_sign[2]["token"], NOW),
        ("expired", m_expired, NOW),
        ("access_token_is_not_media", valid, NOW),
        ("tampered_signature", f"{m_enc}.{m_flip}", NOW),
        ("no_dot", "abc", NOW),
        ("empty", "", NOW),
        ("bad_base64", "Y." + "0" * 64, NOW),
        ("body_without_dot", signed("media"), NOW),
        ("body_with_one_dot", signed("media.x"), NOW),
        ("non_ascii_signature", f"{m_enc}.{m_sig[:-1]}é", NOW),
        ("signed_non_int_exp", signed("media.obj/a.m4a.later"), NOW),
        ("signed_key_with_dots", signed(f"media.a.b.c.{FAR}"), NOW),
    ]
    media_verify = []
    for name, tok, now in media_cases:
        with at(now):
            res = outcome(security.verify_media_key, SECRET, tok)
        media_verify.append({"name": name, "token": tok, "now": now, "python": res})

    return {
        "secret": SECRET,
        "issue": issue,
        "verify": verify,
        "media_sign": media_sign,
        "media_verify": media_verify,
    }


PAYLOADS = [
    ("empty", "{}"),
    ("key_order", '{"b": 1, "a": 2}'),
    ("nested", '{"z": {"y": [3, {"b": true, "a": null}], "x": "s"}, "a": [], "m": {}}'),
    ("unicode", '{"é": "ü", "emoji": "😀", "cjk": "漢字", "ctrl": "\\n\\t\\u0001\\u007f\\u0085", "quote": "\\"\\\\/"}'),
    ("key_sort_codepoints", '{"B": 1, "a": 2, "_": 3, "é": 4, "Z": 5, "aa": 6, "a b": 7, "😀": 8, "ﬀ": 9}'),
    ("ints", '{"zero": 0, "neg": -1, "p53": 9007199254740993, "i64max": 9223372036854775807, "i64min": -9223372036854775808, "u64max": 18446744073709551615}'),
    ("floats", '{"a": 0.0, "b": -0.0, "c": 1.0, "d": 1.5, "e": 0.1, "f": 1e-5, "g": 1e16, "h": 1.7976931348623157e308, "i": 5e-324, "j": 123456.789, "k": 1e22, "l": 100.0, "m": 1E2, "n": 0.0001, "o": 1e15, "p": 2.5e-7}'),
    ("bools_vs_ints", '{"t": true, "f": false, "one": 1, "zero": 0, "n": null}'),
    ("deep_lists", '{"l": [[[[1, [2, [3, {"k": [4]}]]]]]]}'),
    ("call_payload", '{"project_id": "6c1d9b7e-2f3a-4c5d-8e9f-0a1b2c3d4e5f", "recipient": {"role": "broker", "name": "Test Broker", "phone": "+15555550100"}, "property_id": null, "purpose": "Ask about electrical capacity"}'),
    ("purchase", '{"sku": "bed-1", "qty": 3}'),
    ("big_int_beyond_u64", '{"n": 1180591620717411303424}'),
    ("negative_zero_integer_literal", '{"n": -0}'),
]

# serde_json (without arbitrary_precision) cannot hold integers beyond u64, and parses the integer
# literal -0 as the float -0.0. Both are recorded so the limitation stays visible.
RUST_UNSUPPORTED = {"big_int_beyond_u64", "negative_zero_integer_literal"}


def hash_vectors() -> list[dict]:
    out = []
    for name, text in PAYLOADS:
        payload = json.loads(text)
        canonical = json.dumps(payload, sort_keys=True, separators=(",", ":"), default=str)
        out.append(
            {
                "name": name,
                "payload_json": text,
                "canonical": canonical,
                "hash": payload_hash(payload),
                "rust_supported": name not in RUST_UNSUPPORTED,
            }
        )
    return out


def approval_vector() -> dict:
    a = ApprovalRequest(
        approval_id=uuid.UUID("11111111-2222-4333-8444-555555555555"),
        project_id=uuid.UUID("6c1d9b7e-2f3a-4c5d-8e9f-0a1b2c3d4e5f"),
        requested_by_agent="procurement",
        action_type=ActionType.PURCHASE,
        action_payload={"sku": "bed-1", "qty": 3, "note": "für Zimmer 2"},
        risk_category=RiskCategory.CONSEQUENTIAL,
        summary="Buy 3 beds",
        expires_at=datetime(2026, 11, 15, 12, 0, tzinfo=UTC),
        authorized_owner_id=USER,
        idempotency_key="purchase-1",
        created_at=datetime(2026, 11, 14, 12, 0, 0, 123456, tzinfo=UTC),
    )
    return {
        "dump": a.model_dump(mode="json"),
        "without_hash": {k: v for k, v in a.model_dump(mode="json").items() if k != "action_payload_hash"},
    }


def main() -> None:
    (HERE / "tokens.json").write_text(json.dumps(token_vectors(), indent=1, ensure_ascii=False) + "\n")
    (HERE / "payload_hash.json").write_text(json.dumps(hash_vectors(), indent=1, ensure_ascii=False) + "\n")
    (HERE / "approval.json").write_text(json.dumps(approval_vector(), indent=1, ensure_ascii=False) + "\n")
    print("wrote tokens.json, payload_hash.json, approval.json")


if __name__ == "__main__":
    main()
