# API specification

Machine-readable: `packages/contracts/openapi.json` (also `GET /openapi.json`, docs at `/docs`).
Auth: `Authorization: Bearer <token>` (dev: `POST /api/v1/dev/token`). Another owner's project → **404**.
Errors: `ProblemDetail {error_code, message, details, request_id, retryable}`; `x-request-id` echoed.
Long-running work returns **202** + `TaskAccepted {task, status_url}` (inline execution in dev, so the task is usually already terminal).

| Method | Path | Owner | Status |
|---|---|---|---|
| GET | /health, /ready | foundation | working (ready = DB + provider table) |
| POST | /api/v1/dev/token · GET /api/v1/dev/fixtures/project | foundation | dev only |
| POST/GET | /api/v1/projects · /api/v1/projects/{id} | foundation | working |
| PUT | /api/v1/projects/{id}/business-profile | agents-finance | working |
| POST | /api/v1/projects/{id}/chat | agents-finance | deterministic or deep agent |
| POST | /api/v1/projects/{id}/properties/search | agents-finance / property-spatial | working (fixture providers) |
| GET | /properties · /properties/{pid} · /permits · /feasibility | property-spatial / agents-finance | working |
| POST | /properties/{pid}/analyze | agents-finance | working |
| GET | /rankings · /cost-scenarios | agents-finance | working |
| POST | /layouts/generate · GET /layouts · POST /layouts/{lid}/revisions · GET /layouts/{lid}/bom | property-spatial / frontend-product | working (synthetic geometry) |
| POST | /outreach · GET /communications · POST/GET /calls · POST /handoffs/{hid}/response | communications | approval-gated; fixture telephony |
| POST | /api/v1/approvals/{aid}/decision · GET /projects/{id}/approvals | foundation | working |
| POST | /projects/{id}/media | communications / frontend | local object storage |
| GET | /projects/{id}/tasks/{tid} | foundation | working |
| GET | /projects/{id}/events · /events/stream (SSE) | foundation | working |
| POST | /webhooks/photon (Standard Webhooks) · /webhooks/telnyx (Ed25519) | communications | verify + dedupe; routing TODO |

Idempotency: approvals keyed by `idempotency_key`; Photon/Telnyx sends carry `Idempotency-Key`; webhook
events deduped on `(provider, provider_event_id)`.
