# Events

Envelope (`ActivityEvent`): `schema_version, event_id, event_type, project_id, correlation_id, causation_id,
idempotency_key (unique), actor_type, actor_id, occurred_at, payload`. Payload is validated against the
per-type model in `contracts/events.py::EVENT_PAYLOADS`; keys like `token`, `join_token`, `secret` are rejected.

| Family | Types | Payload | Emitted by (today) |
|---|---|---|---|
| project | project.profile.confirmed | ProfileConfirmedPayload | projects route |
| property | property.search.completed · identity.resolved · permits.ingested · spatial_analysis.completed · analysis.completed | see contracts | workflow (spatial_analysis: property-spatial TODO) |
| infrastructure | unknown_detected · evidence_updated | UnknownDetectedPayload · EvidenceUpdatedPayload | feasibility · communications (findings) |
| financial / ranking | financial.scenario.updated · property.ranking.updated | ScenarioUpdatedPayload · RankingUpdatedPayload | workflow |
| outreach | approval_requested · approved · completed | OutreachPayload | outreach route (approved/completed: communications TODO) |
| call | started · owner_handoff_requested · owner_joined · completed · failed | CallPayload / HandoffRequestedPayload | CallOrchestrator |
| layout | version_created · validation_failed · approved | LayoutPayload | DesignService (approved: frontend-product TODO) |
| bom | generated · recalculation_required · approved | BomPayload | BOM route (others TODO) |
| procurement / logistics | procurement.offer_updated · logistics.timeline_updated | OfferPayload · TimelinePayload | TODO frontend-product |
| approval | requested · decided | ApprovalPayload | ApprovalService |
| task | started · completed · failed | TaskPayload | TaskService |

## Subscribers (cross-domain consumers)
| Event | Handler | Result |
|---|---|---|
| infrastructure.evidence_updated | `feasibility.reevaluate` | property.analysis.completed → financial.scenario.updated |
| financial.scenario.updated | `ranking.rerank` | property.ranking.updated (new version) |

Register with `@subscribe(EventType.X, "unique.handler.name")` (`app/services/events.py`). Handlers must be
idempotent: check `already_handled(db, event, ResultType)` and emit results with `causation=event`.
Max causation depth 12. Reading: `GET /api/v1/projects/{id}/events?after=<seq>` or SSE `/events/stream`.
