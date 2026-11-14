# Merge strategy

```
main ──●(foundation)────●──────●──────●───── demo tag
        ├─ dev/agents-finance ──┘      │
        ├─ dev/property-spatial ───────┘
        ├─ dev/frontend-product ──┘ (small PRs, any order)
        └─ dev/communications ─────────────┘
```
- All four `dev/*` branches start at the same foundation commit.
- PRs into `main` are small and frequent; CI (backend, contracts+web, compose, board) must pass.
- `main` is merged back into each `dev/*` after every merge that touches shared code (`git merge origin/main`).
- Never force-push `main` or `dev/*`. Never rewrite someone else's branch.
- Conflicts in shared files (`app/contracts`, `packages/contracts`, `alembic/versions`, `app/main.py`): regenerate contracts instead of hand-merging generated files; for migrations, rebase your revision on the latest head (`alembic heads` must show one head).
- Integration windows (suggested, ET): Sat 2pm, Sat 8pm, Sun 1am, Sun 5am — everyone merges what works; run `scripts/smoke-test.sh` on `main` afterwards.
- Code freeze **Sun 6:30am ET**; submission by **8:00am ET Sun Nov 15**. Repository must be **public** before submitting.
