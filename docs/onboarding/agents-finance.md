# Onboarding — Agent Systems & Financial Intelligence

**Branch:** `dev/agents-finance` · **Owner:** see docs/OWNERSHIP_MATRIX.md

## Mission
Make the Expansion Supervisor genuinely agentic (LangChain Deep Agents track) and the financial comparison
trustworthy: requirements → evidence-grounded feasibility → deterministic costs → explainable ranking, recomputed when evidence or layouts change.

## Owned directories
`apps/api/app/agents/`, `app/workflows/`, `app/domains/{requirements,feasibility,finance,ranking,evidence}/`, and the chat/properties/rankings route handlers.

## What already works
- `create_deep_agent` supervisor + 4 subagents + 8 tools; approval guard; deterministic fallback; scripted-model test.
- Facial-studio requirement template; feasibility rules (area, rooms, electrical, sinks, water, HVAC); cost catalog;
  cents-exact CostScenario; ranking with hard constraints, weights, uncertainty penalty, explanations.
- Event chain evidence_updated → re-assessment → scenario → re-rank.

## Required features (priority order)
1. Set `SUPERVISOR_MODEL` (e.g. `anthropic:claude-sonnet-5-5`) + key; make chat plan/delegate with real subagents; stream progress as events.
2. Postgres checkpointer (`AGENT_CHECKPOINTER=postgres`, call `PostgresSaver.setup()` once) and resumable threads per project.
3. Onboarding agent: chat → draft `BusinessExpansionProfile` with `FieldProvenance` (INFERRED) → owner confirms.
4. More requirement templates (salon, med spa, café) and richer feasibility rules using geometry (sink distance from SiteGeometry fixtures).
5. User-adjustable ranking priorities endpoint (`PUT business-profile` already exists) and horizon what-ifs.
6. Recompute on `layout.version_created` / `bom.generated` → `financial.scenario.updated` with `design_version_id`.
7. Evidence subagent: run `validate_property` before answers; surface conflicts (`EvidenceService.conflicts`).

## Shared contracts you consume / produce
Consume: BusinessExpansionProfile, PropertyDigitalProfile, PermitRecord, SourceEvidence, InfrastructureRequirement.
Produce: AgentTaskResult, InfrastructureAssessment, RenovationWorkItem, CostScenario, PropertyRankingResult, ActivityEvent.

## Endpoints / events
Consumes: `/properties/*`, `property.permits.ingested`, `infrastructure.evidence_updated`, `layout.version_created`.
Produces: `/chat`, `/rankings`, `/cost-scenarios`, `/feasibility`; events `property.analysis.completed`, `infrastructure.unknown_detected`, `financial.scenario.updated`, `property.ranking.updated`.

## Dependencies & credentials
Model key (`ANTHROPIC_API_KEY` or other LangChain provider). Nothing else.

## Setup
```bash
git fetch origin && git switch dev/agents-finance   # or: git switch -c dev/agents-finance origin/dev/agents-finance
gh auth login && tools/board whoami
scripts/bootstrap.sh          # deps, .env, Postgres/Redis (Docker) , migrations, contracts
scripts/dev.sh                # API :8000 (docs at /docs), web :3000
cd apps/api && uv run pytest -q
```

## Tests to keep green / extend
`tests/test_finance_ranking.py` (acceptance), `test_events_workflow.py`, `test_supervisor.py`.

## Acceptance
Two properties where the cheaper rent needs much more renovation → the economical one ranks first under cost priority;
unknown infrastructure stays UNKNOWN with verification questions; supervisor answers cite record IDs.

## Do not modify
CoStar/Matterport/permit adapters, geometry solver, `apps/web`, Photon/Telnyx code, canonical contracts (without the contract process).

## Deliverables
Live-model supervisor demo, checkpointed threads, onboarding agent, recompute-on-layout, tests.

## Blockers / submitting
Post on the board tagging the owner/lead. Small PRs `dev/agents-finance` → `main`, `Closes #N` when applicable.
