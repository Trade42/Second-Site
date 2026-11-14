# Data flow

```mermaid
sequenceDiagram
  participant U as Owner (web)
  participant API as FastAPI
  participant T as TaskService
  participant S as Scout (CoStar adapter)
  participant P as PermitIntelligence (LA Socrata)
  participant F as Feasibility
  participant $ as Finance
  participant K as Ranking
  participant E as EventService (activity_events)
  U->>API: POST /projects (profile CONFIRMED)
  API->>E: project.profile.confirmed
  U->>API: POST /properties/search
  API->>T: TaskRecord(analyze_property) -> 202
  T->>S: search(PropertySearchRequest)
  S-->>E: property.identity.resolved ×N, property.search.completed
  loop each property
    T->>P: ingest(PropertyIdentity)
    P-->>E: property.permits.ingested (PermitRecord + SourceEvidence)
    T->>F: evaluate(profile, requirements, digital profile, evidence)
    F-->>E: property.analysis.completed, infrastructure.unknown_detected
    T->>$: scenario(work items, listing)
    $-->>E: financial.scenario.updated
  end
  T->>K: rank(profile, profiles, scenarios, assessments)
  K-->>E: property.ranking.updated
  T-->>E: task.completed
  U->>API: GET /rankings, /events (or SSE /events/stream)
```

## Event-driven recomputation (tested)
`infrastructure.evidence_updated` → feasibility re-evaluates that property (old rows `superseded`) →
`property.analysis.completed` → `financial.scenario.updated` → ranking subscriber →
`property.ranking.updated` (new version). Each hop carries `causation_id` of the previous event.

## Layout revision path (tested)
`DesignRevisionRequest` (e.g. 4 treatment rooms) → CP-SAT solver → Shapely validation →
new `DesignVersion` with `parent_version_id` → `layout.version_created` → BOM on request.
Infeasible → `layout.validation_failed`, revision `INFEASIBLE`, last valid version untouched.
Next steps (owners): construction implications → `bom.recalculation_required` → updated `CostScenario`.
