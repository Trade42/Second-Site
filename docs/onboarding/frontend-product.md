# Onboarding — Frontend, Interior Experience & Procurement

**Branch:** `dev/frontend-product` · **Owner:** see docs/OWNERSHIP_MATRIX.md

## Mission
A polished, honest product: compare properties, inspect evidence and uncertainty, edit layouts, see the BOM, approve actions.
The 2-minute demo lives here.

## Owned directories
`apps/web/` (all), `app/domains/design/interior/`, `app/domains/{procurement,logistics}/`, `app/integrations/catalog/`.

## What already works
Next.js 16 app with six destinations (Overview, Explore Properties, Property Intelligence, Design Studio, Procurement,
Activity & Communications), typed client (`openapi-fetch` + `@second-site/contracts`), shared components
(PropertyCard, CostSummary, EvidenceStatus, ActivityTimeline, TaskStatus, ApprovalDialog, states), read-only Konva floor plan,
"Seed demo project" flow. Screenshots: `docs/demo/`.

## Required features (priority order)
1. Onboarding form → `POST /projects` + `PUT /business-profile` (replace the seed button for real use).
2. Property comparison view (side-by-side costs, line items, assumptions, horizon slider calling the API).
3. Evidence drill-down: permit → evidence → limitations; badges by `EvidenceClassification`; "historical ≠ current" copy.
4. Konva editor: drag/resize rooms, client-side overlap hints, submit `POST /layouts/{id}/revisions`; show `validation_report` issues; version compare.
5. Conversational design changes in chat (via `/chat` once agents-finance supports it).
6. BOM by room, supplier comparison (`SupplierOffer`), approval for purchases (APPROVE_BOM/PURCHASE).
7. Live activity via SSE `/events/stream`; opening timeline (`domains/logistics/timeline.py`).
8. Visual polish to the palette in `globals.css`; adopt shadcn/ui if desired.

## Contracts
Consume: ProjectContext, BusinessExpansionProfile, PropertyRankingResult, PropertyDigitalProfile, CostScenario, DesignVersion, BillOfMaterials, SupplierOffer, ApprovalRequest, ActivityEvent.
Produce: validated requests, DesignRevisionRequest bodies, procurement selections, ApprovalDecision.
Never hand-write API types: run `scripts/generate-contracts.sh` after backend contract changes.

## Setup
```bash
git fetch origin && git switch dev/frontend-product   # or: git switch -c dev/frontend-product origin/dev/frontend-product
gh auth login && tools/board whoami
scripts/bootstrap.sh          # deps, .env, Postgres/Redis (Docker) , migrations, contracts
scripts/dev.sh                # API :8000 (docs at /docs), web :3000
cd apps/api && uv run pytest -q
```
Web only: `pnpm --filter @second-site/web dev` (`NEXT_PUBLIC_API_BASE_URL`, default http://localhost:8000).

## Checks
`pnpm --filter @second-site/web typecheck`, `lint`, `build`.

## Acceptance
Load a project, compare properties, inspect evidence, display layout candidates, present a BOM and pending approvals — all from real endpoints, with loading/error/empty states and no hardcoded demo values.

## Do not modify
Adapters, supervisor, migrations, Photon/Telnyx code, canonical contracts (without the contract process).

## Blockers / submitting
Board post; PRs `dev/frontend-product` → `main`.
