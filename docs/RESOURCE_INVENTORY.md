# Resource inventory (what was read, and how)

| Resource | Access | Notes |
|---|---|---|
| Repo Trade42/Second-Site | ✅ git + `gh api` | collaborators API blocked by proxy (403) |
| ChatGPT spec + shared link | ❌ not accessible | requirements taken from the master prompt |
| HackPrinceton hacker packet | ✅ Google Drive connector | public repo created Nov 13–15; 36-hour rule; 2-min demo |
| On-site handbook | ✅ | 8am Sun deadline, one main track, judging 9:30–14:30, career-fair sponsors |
| Main tracks doc | ✅ | Business and Enterprise description |
| Special tracks doc | ✅ | CoStar criteria (toolkit = SDKs, MCP servers, OpenAPI; LA + Houston), Peri eligibility, LangChain, Photon prizes incl. Best Voice Agent |
| Team-agent guide (Notion) | ✅ partial (rendered headless) | matches existing tools/board template |
| Photon hackathon guidelines (Notion) | ⚠️ partial | intro + "what you can build" rendered; Requirements section did not render |
| Reference: Plume/HackMIT PIXX-AR | ✅ | room photo → spatial shopping; context only |
| Reference: Devpost "Homie" | ❌ Cloudflare challenge blocked | not read |
| LA Open Data ysqd-apz7 / 67is-svtd | ✅ live API + metadata | coverage 2020-01-01 → present (368,813 electrical rows); snapshot committed |
| LangChain Deep Agents reference | ✅ | `create_deep_agent` signature verified against installed 0.7.38 |
| Photon 2.0 beta docs (llms.txt, webhook, iMessage, voice) | ✅ | Standard Webhooks; text+images only; owner must message first |
| Telnyx docs (webhooks, OpenAPI dial/conference) | ✅ | Ed25519; `/v2/calls`, `/v2/conferences`, `/actions/join` |
| xAI voice SIP docs | ✅ | `realtime.call.incoming`; `wss://api.x.ai/v1/realtime?call_id=` |
| IFC-Bench (HF API) | ✅ metadata | CC-BY-4.0; dental_clinic has arc/mep/str IFC; files not downloaded |
| CoStar / Matterport / Vexcel sponsor toolkit | ❌ no credentials or docs in env | adapters are fixture-only, status BLOCKED |
| IfcOpenShell, Shapely, OR-Tools, FastAPI, SQLAlchemy | ✅ installed and exercised by tests | IfcOpenShell optional extra (not installed) |

Network note: the default cloud environment blocked these hosts; a personal "Second-Site" environment was
created allowing data.lacity.org, huggingface.co, photon.codes, docs.langchain.com, docs.x.ai, developers.telnyx.com.
