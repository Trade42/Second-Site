# Ownership matrix

GitHub logins seen on the repo: `mihirmalaviya`, `tiwari-ira` (Ira), `jasleenkgarcha`, `azizu06` (board template author).
**Who takes which workstream has not been decided — humans fill the Owner column** (also in `AGENTS.md` §4).

| Workstream | Branch | Owner (GitHub login) | Folders |
|---|---|---|---|
| 1 Agent systems & financial intelligence | `dev/agents-finance` | _UNASSIGNED_ | `apps/api/app/agents/`, `app/workflows/`, `app/domains/{requirements,feasibility,finance,ranking,evidence}/` |
| 2 Property, permits & spatial | `dev/property-spatial` | _UNASSIGNED_ | `app/domains/{property,permits,spatial}/`, `app/domains/design/geometry/`, `app/domains/design/service.py`, `app/integrations/{costar,matterport,vexcel,la_permits,ifc}/`, `fixtures/{properties,permits,geometry,ifc}/` |
| 3 Frontend, interior & procurement | `dev/frontend-product` | _UNASSIGNED_ | `apps/web/`, `app/domains/design/interior/`, `app/domains/{procurement,logistics}/`, `app/integrations/catalog/` |
| 4 Communications, voice & execution | `dev/communications` | _UNASSIGNED_ | `app/domains/communications/`, `app/services/call_orchestration/`, `app/api/routes/{communications,webhooks}.py`, `app/integrations/{photon,telnyx,xai_voice,webhook_signatures.py,storage}/`, `infra/voice/`, `fixtures/communications/` |
| Shared foundation (change via board + PR to main) | `main` | team lead | `app/contracts/`, `packages/contracts/`, `app/services/`, `app/db/`, `alembic/`, `app/core/`, `app/api/{deps,errors}.py`, `app/main.py`, `app/repositories/`, `scripts/`, `infra/compose.yaml`, `.github/`, `tools/board`, `.claude/`, `AGENTS.md` |

Route files under `app/api/routes/` follow the domain owner (properties.py → agents-finance + property-spatial jointly; design.py → property-spatial; projects.py chat → agents-finance).
