# Provider status (truthful; updated when the sponsor data went live)

Legend: LIVE · MOCK (fixture) · PARTIAL (live code written, not exercised end-to-end) · BLOCKED · NOT_STARTED

| Provider | Adapter | Credentials in env | Live tested | Fixture | Status | Remaining |
|---|---|---|---|---|---|---|
| CoStar | `KitCoStarProvider` reads the sponsor **data kit** (JSONL snapshot; the toolkit is not an API) | Kit extracted locally (`.data/costar-kit`, gitignored) | **Yes** — LA lease spaces end to end; opt-in pytest | Synthetic listings (CI) + synthetic mini-kit in tests | **LIVE (snapshot data)** — labeled `FIXTURE_REAL` / source `SNAPSHOT` | Houston UI; sale listings; tenant/comps datasets |
| Matterport | `LiveMatterportProvider`: Model API GraphQL (names, measured area, wall graph, openings) | Yes (`.env`) | **Yes** — schema introspected; Suite 110B (630 W 6th St) → validated layouts | Synthetic 12×11.6 m rectangle | **LIVE** | Only 3 LA listed suites have a scan named for the suite; Showcase SDK not used |
| Vexcel | `LiveVexcelProvider`: API 2.0 `ortho/extract`, `ortho/collections`, `property/extract` | Yes (`.env`) | **Yes** — imagery + roof elements at candidate sites | none | **LIVE** | Oblique views; elements feeding feasibility rules |
| LA permits (Socrata) | live + fixture | Not required (public) | **Yes** — manual curl of all 3 permits + address queries; opt-in pytest `RUN_LIVE_TESTS=1` | Real snapshot (117 records) | **LIVE-capable** (default fixture) | Pre-2020 history source; parcel-level joins |
| IFC-Bench | STEP reader + optional IfcOpenShell | n/a | No (files not downloaded) | tiny synthetic IFC4 | **PARTIAL** | download dental_clinic/mep.ifc, run with `--extra ifc` |
| LangChain Deep Agents | `create_deep_agent` supervisor + 4 subagents | No model key | Executed with a scripted chat model (real graph, real tools) | deterministic supervisor | **PARTIAL** | set `SUPERVISOR_MODEL` + key; Postgres checkpointer wiring |
| Photon 2.0 beta | live send + Standard Webhooks verify + fixture outbox | No | Signature scheme tested with generated secrets | yes | **PARTIAL** | project/API key, webhook destination, inbound routing |
| Telnyx | live dial/conference + Ed25519 verify + fixture | No | Signature tested with generated keypair | yes | **PARTIAL** | credentials, call-control event → state mapping |
| xAI voice | session.update builder | No | No | n/a | **NOT_STARTED** | realtime WSS bridge, SIP routing |
| Oracle deploy | runbook + Caddyfile + compose | No | No | n/a | **NOT_STARTED** | VM, DNS, TLS |
| Product sourcing | catalog protocol + illustrative prices | No | No | yes | **MOCK** | real catalog provider |

`GET /ready` reports the same table at runtime.

## Sponsor data: how it is used and labeled (added with the live integration)

- **CoStar kit** (`apps/api/app/integrations/costar`): active LA/Houston lease spaces. Rent = rate × SF per the
  kit docs (`rateBasis` 1 monthly / 2 yearly); "call for rate" spaces are excluded from cost ranking. Operating
  costs: quoted CAM is used; FullService = included; net leases with no quote get a **flagged estimate**
  (`nnn_is_estimate=true`) = median CAM quoted by ≥10 net-lease spaces in the same state. Build-out is inferred
  from listing notes only when explicit (noted in the disclaimer). Photos are CoStar CDN links (not stored).
- **Matterport** (`integrations/matterport`): models come from the kit's `commercial-properties.csv`. A scan counts
  as the listed space **only if its title names the suite** (`suite_matches`); other scans in the building are
  context and never drive layouts. Geometry: largest floor → polygonized wall graph → `SiteGeometry`
  (`MATTERPORT_MEASURED`, meters, rotated to an axis-aligned local frame). The solver lays out the largest
  inscribed rectangle and maps rooms back; assumptions (unprogrammed area, removable partitions) are in the report.
- **Vexcel** (`integrations/vexcel`): orthomosaic proxied by `/api/v1/media/vexcel/ortho/{property_id}` (key stays
  server-side; only stored properties). Elements → `VISUALLY_OBSERVED` observations with limitations
  (building-level, detector scores, distance to nearest structure).
- CoStar CDN photos returned HTTP 403 from the development sandbox; the UI hides images that fail to load.
- Live smoke tests: `RUN_LIVE_TESTS=1 uv run pytest -m live` (needs `.env` keys and the extracted kit).
