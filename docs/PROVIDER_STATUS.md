# Provider status (truthful, as of the foundation commit)

Legend: LIVE · MOCK (fixture) · PARTIAL (live code written, not exercised end-to-end) · BLOCKED · NOT_STARTED

| Provider | Adapter | Credentials in env | Live tested | Fixture | Status | Remaining |
|---|---|---|---|---|---|---|
| CoStar | `integrations/costar` protocol + fixture; live raises NOT_CONFIGURED | No | No | Synthetic listings for 2 real LA addresses | **BLOCKED** | Inspect sponsor SDK/MCP/OpenAPI, implement `LiveCoStarProvider`, reconcile CoStar IDs |
| Matterport | protocol + fixture geometry | No | No | Synthetic 12×11.6 m rectangle | **BLOCKED** | Model API access, real floor geometry → `SiteGeometry` |
| Vexcel | protocol + empty fixture | No | No | placeholder asset | **BLOCKED** | imagery/roof analysis access |
| LA permits (Socrata) | live + fixture | Not required (public) | **Yes** — manual curl of all 3 permits + address queries; opt-in pytest `RUN_LIVE_TESTS=1` | Real snapshot (117 records) | **LIVE-capable** (default fixture) | Pre-2020 history source; parcel-level joins |
| IFC-Bench | STEP reader + optional IfcOpenShell | n/a | No (files not downloaded) | tiny synthetic IFC4 | **PARTIAL** | download dental_clinic/mep.ifc, run with `--extra ifc` |
| LangChain Deep Agents | `create_deep_agent` supervisor + 4 subagents | No model key | Executed with a scripted chat model (real graph, real tools) | deterministic supervisor | **PARTIAL** | set `SUPERVISOR_MODEL` + key; Postgres checkpointer wiring |
| Photon 2.0 beta | live send + Standard Webhooks verify + fixture outbox | No | Signature scheme tested with generated secrets | yes | **PARTIAL** | project/API key, webhook destination, inbound routing |
| Telnyx | live dial/conference + Ed25519 verify + fixture | No | Signature tested with generated keypair | yes | **PARTIAL** | credentials, call-control event → state mapping |
| xAI voice | session.update builder | No | No | n/a | **NOT_STARTED** | realtime WSS bridge, SIP routing |
| Oracle deploy | runbook + Caddyfile + compose | No | No | n/a | **NOT_STARTED** | VM, DNS, TLS |
| Product sourcing | catalog protocol + illustrative prices | No | No | yes | **MOCK** | real catalog provider |

`GET /ready` reports the same table at runtime.
