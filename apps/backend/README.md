# Second Site backend (Rust)

A Rust port of the Python backend in `apps/api`, **in progress**. The Python code is the
specification: the port keeps its HTTP surface, JSON field names, enum strings, validation rules
and PostgreSQL schema. Photon and the LangChain supervisor stay in Python.

## Crates

| Crate | Path | Status |
|---|---|---|
| `ss-core` | `crates/core` | Ported: ids, UTC timestamps, money, shared enums, `Point2D`, `SourceReference`, the state-transition helper, the project and business-profile contracts (`contracts/{common,state,project}.py`), and a shared `ValidationError`. |
| `ss-domain` | `crates/domain` | Ported so far: evidence, permit, property, infrastructure and finance contracts; address normalization and identity matching; permit fact extraction and permit matching (`fetch_and_match`); the fixture CoStar provider, the CoStar data-kit provider (files only) and the fixture LA permit provider; Property Scout profile assembly and `attach_permits`; business requirement templates. Not yet: feasibility, finance calculator, ranking, evidence validator, Matterport/Vexcel enrichment, live HTTP providers. |
| `ss-server` | `crates/server` | HTTP API, storage and workflows: built on a separate track. |

Dependency direction: `ss-server` -> `ss-domain` -> `ss-core`. `ss-core` and `ss-domain` have no
HTTP and no database.

## Build and test

With Rust stable, inside `apps/backend`:

```
cargo build -p ss-domain
cargo test -p ss-core
cargo test -p ss-domain
cargo clippy -p ss-core -p ss-domain --all-targets -- -D warnings
cargo fmt -p ss-core -p ss-domain -- --check
```

Tests read `fixtures/` and `packages/contracts/schemas/` from the repository at run time.

## Golden vectors

`tests-golden/` holds JSON vectors produced by running the Python implementation, and the scripts
that produced them. Regenerate after changing the Python specification:

```
(cd apps/api && .venv/bin/python ../backend/tests-golden/gen_core.py)
(cd apps/api && .venv/bin/python ../backend/tests-golden/gen_domain.py)
```

`tests-golden/costar_kit_synthetic*/` are **synthetic** directories shaped like the CoStar data kit
(copied from the Python test suite). They are not CoStar data.
