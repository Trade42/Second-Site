# Onboarding — Property Intelligence, Permits & Spatial Engineering

**Branch:** `dev/property-spatial` · **Owner:** see docs/OWNERSHIP_MATRIX.md

## Mission
Turn real sponsor data + LA permits into a source-linked `PropertyDigitalProfile`, and turn measured geometry into valid layouts. This is where the CoStar track's 30% "use of data" is won.

## Owned directories
`app/domains/{property,permits,spatial}/`, `app/domains/design/geometry/`, `app/domains/design/service.py`,
`app/integrations/{costar,matterport,vexcel,la_permits,ifc}/`, `fixtures/{properties,permits,geometry,ifc}/`.

## What already works
- Address normalization + identity matching (suite/floor/building scopes, conflict rejection).
- LA permits: live Socrata adapter (verified) + real snapshot; regex fact extraction; evidence with limitations.
- CP-SAT layout solver (spine/front topologies) + Shapely validation; two candidates; infeasible handling; revisions.
- IFC reader (STEP fallback + optional IfcOpenShell).

## Required features (priority order)
1. **CoStar toolkit**: get keys at the sponsor table/Discord; read the SDK/MCP/OpenAPI docs; implement `LiveCoStarProvider.search` (LA first) returning `RawListing`; store CoStar IDs in `provider_property_ids`. Do not invent methods.
2. Matterport: map a real model's floor plan into `SiteGeometry` (`geometry_source=MATTERPORT_MEASURED`, `scale_verified` only if measured); confirm the Hollywood #302 model the teammate found.
3. Vexcel: exterior assets/roof/parking for the same addresses → `ExteriorAsset` + observations.
4. Permit history before 2020 (find the LADBS legacy dataset), parcel (APN) joins, more extraction patterns.
5. Emit `property.spatial_analysis.completed`; set `geometry_reference`.
6. Solver: non-rectangular floors, entrances on any wall, plumbing proximity from real fixtures, furniture placement.
7. IFC-Bench: download `projects/dental_clinic/mep.ifc` (CC-BY-4.0), `uv sync --extra ifc`, add a test; keep it isolated from real properties.

## Contracts
Consume: BusinessExpansionProfile, PropertySearchRequest, PropertyIdentity, SourceEvidence, SiteGeometry, RoomRequirement, DesignRevisionRequest.
Produce: PropertyDigitalProfile, PermitRecord, InfrastructureObservation, SiteGeometry, DesignVersion, GeometryValidationReport.

## Endpoints / events
Produces data behind `/properties*`, `/permits`, `/layouts*`; events `property.identity.resolved`, `property.permits.ingested`, `property.spatial_analysis.completed`, `layout.version_created`, `layout.validation_failed`.

## Credentials
CoStar/Matterport/Vexcel sponsor keys (env names in .env.example are placeholders until docs confirm). LA Open Data needs none (`PERMIT_PROVIDER_MODE=live`).

## Setup
```bash
git fetch origin && git switch dev/property-spatial   # or: git switch -c dev/property-spatial origin/dev/property-spatial
gh auth login && tools/board whoami
scripts/bootstrap.sh          # deps, .env, Postgres/Redis (Docker) , migrations, contracts
scripts/dev.sh                # API :8000 (docs at /docs), web :3000
cd apps/api && uv run pytest -q
```

## Tests
`tests/test_identity_permits.py`, `test_geometry.py`, `test_ifc.py`; live: `RUN_LIVE_TESTS=1 uv run pytest -m live`.

## Acceptance
A selected LA property links to real source records with provenance; documented facts show limitations; valid room
configurations come from verified geometry; missing linkage/geometry returns an explicit error/UNKNOWN, never invented output.

## Do not modify
Supervisor internals, financial rules, `apps/web`, voice code, canonical contracts (without the contract process).

## Blockers / submitting
Board post tagging the lead; PRs `dev/property-spatial` → `main`.
