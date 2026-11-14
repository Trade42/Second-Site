# Onboarding — Communications, Voice, Handoff & External Execution

**Branch:** `dev/communications` · **Owner:** see docs/OWNERSHIP_MATRIX.md

## Mission
Photon messaging + voice calls that safely gather facts from brokers, hand off to the owner when needed, and feed
findings back as evidence. Targets the Photon track (incl. Best Voice Agent).

## Owned directories
`app/domains/communications/`, `app/services/call_orchestration/`, `app/api/routes/{communications,webhooks}.py`,
`app/integrations/{photon,telnyx,xai_voice,storage}/`, `app/integrations/webhook_signatures.py`, `infra/voice/`, `fixtures/communications/`.

## What already works (communications build)
- Photon owner channel + inbound routing + polling; Telnyx call flow with transcription turns, findings → evidence →
  feasibility, owner handoff via conference; call simulator; Activity page UI. See docs/PROVIDER_STATUS.md
  ("Communications"). To go live: Photon key/project/agent handle (+ webhook or polling) and Telnyx key, connection,
  number, public key, `PUBLIC_BASE_URL` (HTTPS), `ALLOW_LIVE_OUTBOUND_CALLS=true`.

## Foundation (original)
- Outreach creates a PENDING approval; `POST /calls` refuses until APPROVED (single use, exact payload).
- Call state machine + handoff (accept/decline/timeout) with server-side join tokens; owner notification via Photon adapter (fixture outbox).
- Photon Standard Webhooks verification + dedupe; Telnyx Ed25519 verification; escalation keyword policy.
- Verified API notes in module docstrings (Photon beta, Telnyx OpenAPI, xAI SIP).

## Required features (priority order)
1. Photon beta account (photon.codes/beta, free credits): API key, project id, webhook destination → `PHOTON_WEBHOOK_SECRET`. Owner must text the agent first (shared lines can't start conversations).
2. Inbound routing in `webhooks.py`: map sender handle → owner/project; text → chat/design revision; images → `ObjectStorage` + attachment reference (beta: text + images only).
3. Owner approvals over Photon must still go through the authenticated web approval (send a link, not "reply YES").
4. Telnyx: number + connection; map call-control webhooks (answered, hangup, …) to `CallOrchestrator.advance`; conference for owner join.
5. xAI realtime voice over SIP: register number, handle `realtime.call.incoming` (Standard Webhooks), open `wss://api.x.ai/v1/realtime?call_id=`, send `build_session_update(...)`; tool calls → `request_handoff` / findings.
6. Findings → `SourceEvidence(OWNER_REPORTED)` + emit `infrastructure.evidence_updated` (re-ranking is already wired).
7. Play the owner's genuine recorded intro (upload via `/media`); keep the automation disclosure.

## Contracts
Consume: ProjectContext, PropertyDigitalProfile, OutreachRequest, ApprovalRequest, CommunicationSession, CallSession, OwnerHandoffRequest.
Produce: CommunicationSession updates, CallSession, OwnerHandoffRequest, StructuredFinding/SourceEvidence, events `outreach.*`, `call.*`, `infrastructure.evidence_updated`.

## Credentials
`PHOTON_*`, `TELNYX_*`, `XAI_API_KEY`. Live dialing also needs `ALLOW_LIVE_OUTBOUND_CALLS=true` **and** an approved recipient. Never in tests.

## Setup
```bash
git fetch origin && git switch dev/communications   # or: git switch -c dev/communications origin/dev/communications
gh auth login && tools/board whoami
scripts/bootstrap.sh          # deps, .env, Postgres/Redis (Docker) , migrations, contracts
scripts/dev.sh                # API :8000 (docs at /docs), web :3000
cd apps/api && uv run pytest -q
```
Webhooks need public HTTPS (tunnel or `infra/oracle` VM); Photon refuses localhost/redirects.

## Tests
`tests/test_approvals_comms.py` (keep all provider calls mocked).

## Acceptance
A verified or simulated call goes through the state machine, creates an OwnerHandoffRequest, notifies the owner via a supported provider or mock, and handles approve/decline/timeout safely.

## Do not modify
Geometry solver, financial models, CoStar adapter, main web pages (coordinate with frontend-product), canonical contracts (without the contract process).

## Rules
Assistant = "your assistant", never claims to be human, follows disclosure/consent rules; no external action without provider confirmation recorded (`provider_message_ids`, `provider_call_id`).

## Blockers / submitting
Board post; PRs `dev/communications` → `main`.
