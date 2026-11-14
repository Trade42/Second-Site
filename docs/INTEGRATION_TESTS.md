# Integration tests

Run: `cd apps/api && uv run pytest -q` (Postgres at `TEST_DATABASE_URL`; DB tests skip with a reason if unreachable).
Live provider tests are opt-in: `RUN_LIVE_TESTS=1 uv run pytest -m live`.

| File | Covers | Needs DB |
|---|---|---|
| test_contracts.py | every contract has a schema; every event type has a payload; required/extra fields; UTC; money ordering + ints; duplicate cost lines; historical evidence limitations; UNKNOWN≠MEETS; polygon validity; photo≠scale-verified; invalid layout cannot be approved; approval/call/task state machines; event payload validation + secret rejection | no |
| test_identity_permits.py | normalization, direction preservation, suite matching, neighbor suite/building rejection, direction/ZIP/APN conflicts, floor-level NEEDS_REVIEW, missing direction, extraction from real permit text, permit-number lookup, historical preservation, missing data, Socrata query shape/pagination (mock transport), error classification, live permit (opt-in) | no |
| test_finance_ranking.py | **acceptance:** cheaper-rent property costs more over 36 mo and ranks lower; flips at 120 mo; determinism; UNKNOWN electrical preserved; capital constraint disqualifies; no double counting (deposit, contingency, monthly, duplicate work items); cent rounding | no |
| test_geometry.py | two valid non-overlapping candidates; infeasible → no rooms; overlap/outside/missing detection; obstacles reduce usable area | no |
| test_ifc.py | MEP extraction (pipe diameter, airflow, rated current) with provenance; IfcOpenShell path (skipped unless installed) | no |
| test_api_smoke.py | **Phase 7.3 smoke** through HTTP: health/ready → project → search task → properties (fixture labels) → real permits → UNKNOWNs → ranking → cost scenarios → full event chain → layouts → room-linked BOM → 4th-room revision (parent link) → infeasible revision keeps last valid; unauthorized access (401/404, ProblemDetail shape); missing profile; deterministic chat grounded | yes |
| test_events_workflow.py | evidence_updated → re-assessment → scenario → ranking v2 with causation chain; superseded history; idempotent re-emit; redelivery guard; conflicting evidence coexists | yes |
| test_approvals_comms.py | approval lifecycle, payload-hash mismatch, repeated decision 409, single-use, declined purchase, expired approval, wrong owner; Photon signature (valid/invalid/stale/tampered) + duplicate dedupe + fixture-not-live flag; Telnyx Ed25519; call handoff accept/decline/timeout; no join token in events; dial failure; disconnect; escalation policy | yes |
| test_supervisor.py | real `create_deep_agent` graph executes run_property_analysis → get_ranking → request_outreach_approval (PENDING only); tool categories | yes |

Also: `scripts/smoke-test.sh` (against a running API), `scripts/check-contracts.sh`, `alembic upgrade head && alembic check` in CI, web typecheck/lint/build, and the Playwright walk-through recorded in `docs/demo/*.png`.
