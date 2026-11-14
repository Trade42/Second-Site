# Requirements (organized from the master prompt + hackathon docs)

The full 37-section product spec lives in the owner's ChatGPT conversation and was **not accessible**
from this environment; the master initialization prompt embeds the authoritative requirements and was used.

## Product
Second Site — "Your next location, from search to opening." AI expansion platform for a business opening
location #2: discover properties → infrastructure feasibility → total-cost comparison → outreach → layouts →
procurement → opening. All workflows share one project record set.

## MVP scope (this hackathon)
- Market: **Los Angeles** (Houston supported by the property adapter only). No hospitality/short-term rentals.
- Demo business: facial studio, 3 treatment + reception + storage, $180k opening budget, minimize plumbing changes.
- Candidate properties: 5380 E Alhambra Ave (90032) and 6922 W Hollywood Blvd Ste 302 (90028).

## Hard rules (encoded in code, see DECISIONS.md)
| Rule | Where enforced |
|---|---|
| Historical permit ≠ current/spare capacity | `SourceEvidence` requires limitations for DOCUMENTED_HISTORICAL; feasibility never yields MEETS for capacity from permits |
| Evidence classes kept separate (8 statuses) | `EvidenceClassification` enum; no single confidence score |
| UNKNOWN ≠ MEETS | `InfrastructureAssessment` validator; ranking uncertainty penalty |
| No synthetic data presented as CoStar | `CommercialListing.provider='fixture'`, `data_origin=FIXTURE_SYNTHETIC`; validator flags violations |
| No approximate layout presented as approved | `DesignVersion` cannot be APPROVED when invalid; disclaimer on every report; photo geometry cannot be scale-verified |
| No claimed action without provider confirmation | `SendResult.state` is `accepted`/`recorded`, never `delivered`; dial returns `initiated` |
| Approvals bind exact payload, enforced in code | `ApprovalService.require_approved` (payload hash, single use, expiry) |
| Money in integer cents | all money fields `int`; Decimal half-up rounding |
| Geometry in meters | `SiteGeometry.units` pattern `^m$` |
| Secrets never committed / never in events | `.gitignore`; `ActivityEvent` rejects token/secret payload keys |

## Sponsor-track targets and what each needs
| Track | Judged on | Foundation support |
|---|---|---|
| CoStar "Inside and Out" | ≥2 of CoStar/Matterport/Vexcel combined (30%), agentic depth (30%), usefulness (25%), demo (15%) | Typed adapters + digital profile merging listing, interior, exterior, permits. **Live toolkit not yet accessed.** |
| LangChain Deep Agents | best use of deepagents | `create_deep_agent` supervisor with subagents + tools, checkpointing |
| Photon 2.0 | messaging agent (also "Best Voice Agent") | Photon beta adapter, Standard Webhooks verification, owner notifications |
| Peri | real business consulted + evidence | `docs/PERI_CONSULTATION.md` (to be filled by humans) |
| Amazon Practical AI | agentic, useful, polished | whole product |
| Main: Business & Enterprise | creativity, utility, charity, avidity | — |
