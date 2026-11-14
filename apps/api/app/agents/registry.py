"""The 15 logical agent capabilities, their owners and how they execute.

`execution`: "subagent" = a Deep Agents subagent (LLM), "deterministic" = plain service call exposed as a tool,
"interface" = contract defined, implementation pending in the owner's branch.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Capability:
    number: int
    name: str
    owner: str
    execution: str
    produces: str
    tools: tuple[str, ...] = ()


CAPABILITIES: list[Capability] = [
    Capability(0, "Expansion Supervisor", "agents-finance", "deep_agent", "AgentTaskRequest / plan"),
    Capability(
        1,
        "Business Requirements",
        "agents-finance",
        "subagent",
        "BusinessExpansionProfile",
        ("get_project_state",),
    ),
    Capability(
        2,
        "Property Scout",
        "property-spatial",
        "deterministic",
        "PropertyIdentity",
        ("run_property_analysis",),
    ),
    Capability(3, "Spatial Intelligence", "property-spatial", "interface", "SiteGeometry"),
    Capability(
        4,
        "Infrastructure Feasibility",
        "agents-finance",
        "subagent",
        "InfrastructureAssessment",
        ("get_property_evidence", "get_feasibility"),
    ),
    Capability(
        5, "Financial Modeling", "agents-finance", "deterministic", "CostScenario", ("get_cost_scenarios",)
    ),
    Capability(
        6, "Property Ranking", "agents-finance", "deterministic", "PropertyRankingResult", ("get_ranking",)
    ),
    Capability(
        7,
        "Outreach and Negotiation",
        "communications",
        "interface",
        "CommunicationSession",
        ("request_outreach_approval",),
    ),
    Capability(8, "Voice and Handoff", "communications", "interface", "CallSession / OwnerHandoffRequest"),
    Capability(9, "Floor Plan Architect", "property-spatial", "deterministic", "DesignVersion"),
    Capability(10, "Interior Design", "frontend-product", "interface", "FinishSpecification"),
    Capability(11, "Procurement", "frontend-product", "deterministic", "BillOfMaterials / SupplierOffer"),
    Capability(12, "Logistics and Opening", "frontend-product", "interface", "Timeline"),
    Capability(13, "Photon Communications", "communications", "interface", "CommunicationEvent"),
    Capability(
        14,
        "Evidence and Compliance",
        "agents-finance",
        "subagent",
        "validation findings",
        ("validate_evidence",),
    ),
]
