# Contracts

**Source of truth:** `apps/api/app/contracts/*.py` (Pydantic v2, `extra="forbid"`).
**Generated:** `packages/contracts/openapi.json`, `packages/contracts/schemas/*.json`, `packages/contracts/src/*.ts`.
**Regenerate:** `scripts/generate-contracts.sh` · **CI check:** `scripts/check-contracts.sh`.

## Conventions
- UUID ids; timezone-aware UTC timestamps (naive rejected); `schema_version` "1.0"; `record_version` for optimistic concurrency (`RecordRepository.update_versioned`).
- Money: integer minor units (cents) + ISO currency. Geometry: meters; polygons validated by Shapely.
- Unknown = `null` or an explicit `UNKNOWN` enum; no guessed defaults.
- Raw provider payloads never appear in consumer contracts (stored in `property_sources.raw_payload`).
- TS: `ProjectContext` etc. are response shapes (all always-present fields required); `XInput` exists when a request shape differs (e.g. `MarketInput`).

## Modules → contracts
| Module | Contracts |
|---|---|
| project.py | ProjectContext, BusinessExpansionProfile (+ SpaceRequirements, InfrastructureNeeds, OperationalRequirements, DesignPreferences, RankingPriorities, FieldProvenance), Market |
| property.py | PropertyIdentity, CommercialListing, InteriorAsset, ExteriorAsset, SourceManifestEntry, PropertyDigitalProfile, PropertySearchRequest |
| evidence.py | SourceEvidence (DOCUMENTED_HISTORICAL requires limitations) |
| permits.py | PermitRecord (permit_status ≠ verification), ExtractedInfrastructureFact, PermitCategory, PermitScope |
| infrastructure.py | InfrastructureRequirement, InfrastructureAssessment (MEETS needs evidence; UNKNOWN needs questions), InfrastructureObservation |
| finance.py | RenovationWorkItem, CostLineItem, CostScenario (no duplicate lines), HardConstraintResult, RankedProperty, PropertyRankingResult |
| geometry.py | SiteGeometry, RoomRequirement, LayoutPreferences, RoomPolygon, FurniturePlacement, FinishSpecification, GeometryValidationReport, DesignVersion, DesignRevisionRequest, LayoutGenerationRequest |
| procurement.py | BillOfMaterials, BomItem (room-linked), SupplierOffer |
| approvals.py | ApprovalRequest (+ state machine, payload hash), ApprovalDecision, ActionType, RiskCategory |
| communications.py | CommunicationSession, CallSession (+ state machine), OwnerHandoffRequest (+ state machine), AuthorityScope, StructuredFinding, OutreachRequest |
| tasks.py | TaskRecord (+ state machine), AgentTaskRequest, AgentTaskResult |
| events.py | ActivityEvent envelope + one payload model per event type |
| api.py | ProblemDetail, CreateProjectRequest, ProjectEnvelope, ChatRequest/Response, TaskAccepted, Health/Ready, ProviderStatusEntry |

## Producer → consumer handoffs
| Producer | Consumer | Contract |
|---|---|---|
| Requirements | Property Scout | BusinessExpansionProfile |
| Property Scout | Identity Resolver | PropertyIdentity |
| Identity Resolver | Permit Intelligence | PropertyIdentity |
| Permit Intelligence | Feasibility | PermitRecord + SourceEvidence |
| Matterport/Vexcel | Feasibility | PropertyDigitalProfile |
| Feasibility | Financial | InfrastructureAssessment + RenovationWorkItem |
| Financial | Ranking | CostScenario |
| Ranking | Frontend | PropertyRankingResult |
| Feasibility | Outreach | `InfrastructureAssessment.verification_questions` |
| Outreach | Voice | OutreachRequest + APPROVED ApprovalRequest |
| Voice | Photon | OwnerHandoffRequest |
| Voice | Feasibility | StructuredFinding → SourceEvidence (OWNER_REPORTED) + `infrastructure.evidence_updated` |
| Supervisor | Floor Plan Architect | SiteGeometry + RoomRequirement[] (LayoutGenerationRequest) |
| Floor Plan Architect | Interior / Procurement | DesignVersion (approved for procurement) |
| Interior | Procurement | FinishSpecification |
| Procurement | Logistics | BillOfMaterials + SupplierOffer |
| Any domain | Evidence validator | SourceEvidence refs + action context |

Rule: INPUT → VALIDATION → PROCESSING → PERSISTED OUTPUT → EVENT → NEXT CONSUMER.
Changing a contract: see `SCHEMA_VERSIONING.md` and `TEAM_WORKFLOW.md`.
