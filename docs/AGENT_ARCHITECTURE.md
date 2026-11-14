# Agent architecture

## Supervisor
`apps/api/app/agents/supervisor.py` → `deepagents.create_deep_agent(model, tools, system_prompt, subagents, checkpointer)`
(verified against deepagents 0.7.38). Subagents: `requirements`, `feasibility`, `financial`, `evidence`
(SubAgent dicts with `name/description/system_prompt/tools`). The built-in `task` tool delegates.

- `SUPERVISOR_MODEL` unset → `DeterministicSupervisor` (same tools, fixed plan, `mode: deterministic`).
- Checkpointer: `InMemorySaver` (default) or `PostgresSaver` (`AGENT_CHECKPOINTER=postgres`, needs `setup()`).
  Thread id = `project:<project_id>`.
- `tests/test_supervisor.py` runs the real Deep Agents graph with a scripted chat model and asserts tool execution,
  persistence and that outreach only produced a PENDING approval.

## Tools and authorization (`agents/tools.py`)
| Tool | Category | Notes |
|---|---|---|
| get_project_state, get_ranking, get_feasibility, get_property_evidence, get_cost_scenarios, validate_evidence | READ_ONLY | return record IDs |
| run_property_analysis | INTERNAL_WRITE | deterministic workflow |
| request_outreach_approval | EXTERNAL_COMMUNICATION | `approval_guard`: creates PENDING approval; **cannot send** |
| (none) | CONSEQUENTIAL | no purchase/commitment tool is exposed to the model |

Execution endpoints (`POST /calls`, future send/purchase) call `ApprovalService.require_approved(approval_id, action, payload)`:
APPROVED, unexpired, same action type, same payload hash; then mark EXECUTED (single use).

## The 15 capabilities (`agents/registry.py`)
| # | Capability | Owner | Execution now |
|---|---|---|---|
| 0 | Expansion Supervisor | agents-finance | deep_agent |
| 1 | Business Requirements | agents-finance | subagent |
| 2 | Property Scout | property-spatial | deterministic |
| 3 | Spatial Intelligence | property-spatial | interface |
| 4 | Infrastructure Feasibility | agents-finance | subagent + deterministic engine |
| 5 | Financial Modeling | agents-finance | deterministic |
| 6 | Property Ranking | agents-finance | deterministic |
| 7 | Outreach and Negotiation | communications | interface (approval-gated) |
| 8 | Voice and Handoff | communications | interface (state machine implemented) |
| 9 | Floor Plan Architect | property-spatial | deterministic (CP-SAT) |
| 10 | Interior Design | frontend-product | interface (finish presets) |
| 11 | Procurement | frontend-product | deterministic BOM |
| 12 | Logistics and Opening | frontend-product | interface (placeholder timeline) |
| 13 | Photon Communications | communications | interface |
| 14 | Evidence and Compliance | agents-finance | subagent + deterministic validator |

## Structured delegation
`AgentTaskRequest` / `AgentTaskResult` (contracts/tasks.py) are the agent-to-agent contract; free text is not.

## Voice handoff (communications)
`CallState`: CREATED → DIALING → INTRO → ACTIVE → HANDOFF_PENDING → OWNER_JOINING → HUMAN_JOINED → AI_RESUMED → COMPLETED
(+ FAILED / DECLINED / CANCELLED). Escalation triggers (`domains/communications/escalation.py`): binding
commitments, deposits/payments, guarantor, sensitive personal info, counterparty asks for owner, insufficient
evidence, exceeds authority. Handoff creates a short-lived join token held server-side; events carry only an
opaque reference. No response before `expires_at` → EXPIRED → AI_RESUMED (question deferred).
The assistant is presented as the owner's **assistant**, states it is automated when required, never claims to be human;
the owner's intro is a genuine recording (`intro_recording_reference`), never synthesized.
