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
| Photon 2.0 beta | Owner channel (link on first owner text), inbound routing (JOIN/LATER/STATUS/STOP + chat), send replies with verified ids, webhook + polling (list messages), Standard Webhooks verify | No | Request shapes tested against docs with a mock transport; full flow tested with the fixture inbox/outbox | yes (+ "simulate a text" in the UI) | **PARTIAL — code complete, awaiting credentials** | Photon project key + project id, shared-line assignment for the owner's handle, webhook destination (or use polling) |
| Telnyx | Call flow: dial (client_state, AMD) → intro/disclosure → transcription turns → findings → handoff (owner dial + conference) → hangup/finalize; Ed25519 verify | No | Command shapes tested with a mock transport; event handling tested end to end via the call simulator | Call simulator (labeled SIMULATED) | **PARTIAL — code complete, awaiting credentials** | API key, Call Control connection + number, public webhook URL, `ALLOW_LIVE_OUTBOUND_CALLS=true` |
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

## Communications (Photon + Telnyx): how it works and what is verified

- **Owner channel (Photon):** owner saves the handle they text from (Activity page) → texts the agent's number once
  (shared lines cannot start conversations) → channel LINKED with the Space id and agent User id taken from that
  inbound message. Replies use `spaceId=space.id`, `senderId=receivedBy.id`, `parentMessageId=id` (per the Beta
  quickstart). Unknown senders are never answered. Approvals are never granted by text; texts carry app links.
- **Inbound:** webhook (`/webhooks/photon`, Standard Webhooks, dedupe on event id and message id) or polling
  (`POST /api/v1/owner/channel/poll`, `GET /v1/projects/{id}/messages?direction=received`). JOIN/LATER answer a
  pending handoff; STATUS summarizes; anything else goes to the supervisor chat and the answer is texted back.
- **Broker call (Telnyx):** only after an APPROVED `PLACE_CALL`. Questions come from the property's open
  feasibility assessments. Opening: the owner's genuine recorded intro (signed short-lived URL) or a spoken intro;
  the automation disclosure is always stated. Escalation phrases (deposit, guarantor, sign, ...) → the assistant
  says it cannot agree to anything → owner handoff (Photon text + in-app card). JOIN → Telnyx dials the owner's own
  number and conferences the broker leg; no answer/LATER → the question is deferred and the call continues.
- **Findings:** each answer → `StructuredFinding`; clear values → `SourceEvidence(OWNER_REPORTED, provider=broker_call)`
  with "not independently verified" limitations → `infrastructure.evidence_updated` → feasibility (reads
  `tenant_subpanel_amps`, `in_unit_plumbing`, `hvac_zonable`) → cost scenario → re-ranking.
- **Without credentials:** the voice provider is the fixture, so every call is created `simulated=true`; the
  simulator feeds Telnyx-shaped events (`call.answered`, `call.speak.ended`, `call.transcription`, `call.hangup`, owner
  leg + `conference.participant.joined`) through the same handler as real webhooks. Simulated evidence is
  `FIXTURE_SYNTHETIC` and every limitation starts with "SIMULATED CALL".
- **Not done:** no real Photon or Telnyx traffic has been exchanged (no credentials). xAI realtime voice is not
  wired; the call uses Telnyx speak + transcription turns instead. Photon image attachments are recorded by
  reference only (download endpoint not verified).
