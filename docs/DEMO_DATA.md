# Demo data

| Fixture | Origin | Contents |
|---|---|---|
| `fixtures/permits/la_permits_snapshot.json` | **FIXTURE_REAL** — verbatim LADBS records retrieved 2026-11-14 from data.lacity.org (`ysqd-apz7`, `67is-svtd`) | 57 electrical + 60 mechanical permits for 5380 E Alhambra Ave and 6922 W Hollywood Blvd (all suites) |
| `fixtures/properties/la_candidates.json` | **FIXTURE_SYNTHETIC** — invented listing terms on real addresses | rent, area, NNN, deposit, build-out. **Not CoStar records.** |
| `fixtures/geometry/hollywood_302_synthetic.json` | FIXTURE_SYNTHETIC | 12.0×11.6 m rectangle, corner column, entrance on y=0, sink at (11.5, 10.8). Not measured. |
| `fixtures/projects/facial_studio.json` | FIXTURE_SYNTHETIC (spec demo inputs) | 3 treatment + reception + storage, $180k capital, $7,500 rent limit, 36-mo horizon |
| `fixtures/communications/photon_inbound_text.json` | FIXTURE_SYNTHETIC | message shaped like Photon beta message objects |
| `fixtures/ifc/mini_mep.ifc` | FIXTURE_SYNTHETIC (hand-written IFC4) | pipe, sink, duct, panel with property sets. **Not IFC-Bench.** |

## Verified permit facts used by the demo
| Permit | Address (as recorded) | Status | Fact |
|---|---|---|---|
| 19041-10000-39945 | 5380 E ALHAMBRA AVE 90032, APN 5220031007 | Permit Finaled 2022-07-20 | change of use warehouse → office/R&D; new 800A 120/240V 3P 4W service, transformer; 9,840 sq ft |
| 19042-10000-24243 | same | Permit Finaled 2022-07-07 | new 2-inch water service with backflow preventer |
| 20042-90000-08431 | 6922 W HOLLYWOOD BLVD 302 90028, APN 5548006011 | Permit Finaled 2020-06-19 | rough and finish for 1 break room sink |
| 20042-90000-08432 | 6922 W HOLLYWOOD BLVD **304** | Permit Finaled | identical sink permit for the neighboring suite — **must not** attach to 302 |

## Demo numbers (deterministic, illustrative ranges)
36-month total: Hollywood #302 ≈ $356,097 vs Alhambra ≈ $377,384 — Alhambra has lower rent but ~$100k more expected
build-out (open shell partitions, new sink runs, new HVAC distribution). At 120 months Alhambra is cheaper.
With the $180k capital limit Alhambra is disqualified on upfront cash. All unit costs are planning ranges in
`domains/finance/cost_catalog.py`, not quotes.
