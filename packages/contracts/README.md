# @second-site/contracts (GENERATED)

Do not edit files here by hand. The source of truth is the Pydantic v2 models in
`apps/api/app/contracts/`. Regenerate everything with:

```bash
scripts/generate-contracts.sh
```

| File | Produced by |
|---|---|
| `openapi.json` | `python -m app.contracts.export` (FastAPI OpenAPI + every canonical contract) |
| `schemas/*.json` | same — one JSON Schema per contract, `index.json` lists them |
| `src/generated/openapi.ts` | `openapi-typescript openapi.json` |
| `src/index.ts` | `scripts/write-index.mjs` — `export type ProjectContext = components["schemas"]["ProjectContext"]` etc. |

CI fails if regeneration produces a diff (`scripts/check-contracts.sh`).
