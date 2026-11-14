# Architecture

```mermaid
flowchart TB
  subgraph Web["apps/web — Next.js (frontend-product)"]
    UI[Overview · Explore · Intelligence · Design · Procurement · Activity]
    TC[typed client: openapi-fetch + @second-site/contracts]
  end
  subgraph API["apps/api — FastAPI"]
    R[routes: projects, properties, layouts, outreach, approvals, events, webhooks]
    SUP[Expansion Supervisor\ndeepagents.create_deep_agent]
    WF[workflows/property_analysis]
    subgraph Domains
      PROP[property · permits · spatial · design/geometry\nproperty-spatial]
      FIN[requirements · feasibility · finance · ranking · evidence\nagents-finance]
      PROC[design/interior · procurement · logistics\nfrontend-product]
      COMM[communications · call_orchestration · webhooks\ncommunications]
    end
    SVC[shared services: events · tasks · approvals · authorization · evidence]
    INT[integrations: costar · matterport · vexcel · la_permits · ifc · photon · telnyx · xai_voice · catalog · storage]
  end
  PG[(PostgreSQL\nbusiness state + LangGraph checkpoints)]
  RD[(Redis\nevent queue)]
  WK[worker]
  UI --> TC --> R
  R --> SUP --> WF
  R --> WF --> Domains
  SUP -. tools .-> Domains
  Domains --> SVC --> PG
  Domains --> INT
  SVC -- redis mode --> RD --> WK --> Domains
  PG -- /events, /events/stream --> UI
```

Three kinds of state, kept apart:
1. **Business state** — Postgres tables (`projects`, `properties`, `permit_records`, `property_observations`, …). Authoritative.
2. **Agent execution state** — LangGraph checkpoints (InMemorySaver / PostgresSaver). Never read as facts.
3. **Conversation state** — chat/Photon/call transcripts referenced from `communications`. Never authoritative.

Every component's owner, contract and consumer: see `SUBSYSTEM_OWNERSHIP.md` and `CONTRACTS.md`.
