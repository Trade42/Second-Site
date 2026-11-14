# Subsystem ownership, boundaries, contracts, consumers

| Component | Owner branch | Boundary (code) | Produces | Consumed by |
|---|---|---|---|---|
| Contracts | foundation (all, via board) | `apps/api/app/contracts`, `packages/contracts` | every schema | everyone |
| Supervisor + tools | agents-finance | `app/agents` | AgentTaskResult, chat | web chat |
| Requirements | agents-finance | `domains/requirements` | InfrastructureRequirement | feasibility, ranking |
| Feasibility | agents-finance | `domains/feasibility` | InfrastructureAssessment, RenovationWorkItem | finance, outreach (questions) |
| Finance | agents-finance | `domains/finance` | CostScenario | ranking, web |
| Ranking | agents-finance | `domains/ranking` | PropertyRankingResult | web |
| Evidence validator | agents-finance | `domains/evidence` | findings | supervisor |
| Workflow orchestration | agents-finance | `app/workflows` | events | worker, web |
| Property scout / identity | property-spatial | `domains/property`, `integrations/{costar,matterport,vexcel}` | PropertyIdentity, PropertyDigitalProfile | permits, feasibility |
| Permit intelligence | property-spatial | `domains/permits`, `integrations/la_permits` | PermitRecord, SourceEvidence | feasibility |
| IFC benchmark | property-spatial | `integrations/ifc` | IfcElement → observations (isolated) | tests |
| Spatial + solver | property-spatial | `domains/spatial`, `domains/design/geometry`, `domains/design/service.py` | SiteGeometry, DesignVersion, GeometryValidationReport | web, procurement |
| Web app | frontend-product | `apps/web` | validated requests, decisions | owner |
| Interior / procurement / logistics | frontend-product | `domains/design/interior`, `domains/procurement`, `domains/logistics`, `integrations/catalog` | finishes, BillOfMaterials, SupplierOffer, timeline | web |
| Communications | communications | `domains/communications`, `services/call_orchestration`, `api/routes/{communications,webhooks}.py`, `integrations/{photon,telnyx,xai_voice}`, `infra/voice` | CommunicationSession, CallSession, OwnerHandoffRequest, findings | feasibility (evidence), web |
| Shared services, DB, migrations, CI | foundation | `app/services`, `app/db`, `alembic`, `app/core`, `.github`, `infra/compose.yaml` | — | everyone; changes via board |
