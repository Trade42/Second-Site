# Decisions log

| ID | Decision | Why |
|---|---|---|
| D-01 | One FastAPI backend with domain modules; no microservices | 4 people, 36 hours |
| D-02 | Pydantic v2 = source of truth → OpenAPI/JSON Schema → generated TS (`openapi-typescript`) | single contract, CI drift check |
| D-03 | Rows store the validated contract in a JSONB `data` column + indexed columns | 31 tables without hand-mapping every field; teammates evolve contracts without migrations for every optional field |
| D-04 | Money = integer cents; Decimal half-up rounding; deposits excluded from long-term cost | spec rule; no float totals |
| D-05 | LA first; Houston only in the property adapter | permit datasets for LA |
| D-06 | LA permits: live adapter + **real** snapshot fixture (FIXTURE_REAL) | offline dev/tests without inventing records |
| D-07 | Permit matching requires house number + street name + type; direction/ZIP/APN conflicts reject; different unit rejects; floor-level records → NEEDS_REVIEW | never attach a neighbor's permit |
| D-08 | Historical electrical service → electrical requirement stays **UNKNOWN** with the 800A as observed context + an allowance cost range | permit ≠ spare capacity |
| D-09 | Listing terms for the two real addresses are **synthetic** (`provider: fixture`) until CoStar access | never pass fixtures off as CoStar |
| D-10 | Deterministic services for arithmetic, matching, ranking, geometry; LLM only orchestrates/explains | spec; testability |
| D-11 | Supervisor runs a deterministic plan when `SUPERVISOR_MODEL` is unset (`mode: deterministic`) | demo works without a key; honest labeling |
| D-12 | External actions only via `approval_guard` → PENDING approval; execution endpoints re-check `require_approved` | enforcement at the boundary, not in prompts |
| D-13 | Event bus = persisted `activity_events` + in-process subscribers (inline) or Redis list + worker; idempotency key unique; redelivery guard via causation | durable, loop-safe, simple |
| D-14 | Layout solver: OR-Tools CP-SAT over corridor strips with deterministic work budget; Shapely validation | real constraint solver, reproducible |
| D-15 | Photon for owner messaging; Telnyx owns PSTN + conference; xAI realtime over SIP for the assistant leg | Photon beta has no 3-way conferencing; verified docs |
| D-16 | Four long-lived branches `dev/*` from one foundation commit; small PRs into `main` | parallel work with continuous integration |
| D-17 | Postgres required for DB tests (skip with reason otherwise); pure-domain tests need nothing | real JSONB/identity semantics |
| D-18 | Dev auth = HMAC bearer tokens via `POST /api/v1/dev/token` (disabled in production) | hackathon; swap for IdP later |

## Contradictions resolved
- Spec route shapes (short/global) vs prompt (project-scoped) → project-scoped routes (D-prompt).
- Earlier "Houston first" → LA first (authoritative update).
- Teammate wrote "6922 Hollywood Blvd, Suite 302"; LADBS records say **6922 W HOLLYWOOD BLVD 302**. Identity uses the W; matching treats a missing direction as non-evidence.
- Teammate said the 6922 sink permit "documents a break-room sink" → verified: `20042-90000-08431`, "Rough and finish for 1 break room sink", finaled 2020-06-19, unit 302. An identical permit exists for suite 304 (`…-08432`) and is correctly **excluded**.
- 5380 E Alhambra 800A permit → verified (`19041-10000-39945`, finaled 2022-07-20) but it is part of a **change of use (warehouse → office/R&D, 9,840 sq ft)** for the building; unit allocation unknown.
