.PHONY: bootstrap dev test contracts check-contracts smoke migrate up down board
bootstrap:        ; scripts/bootstrap.sh
dev:              ; scripts/dev.sh
test:             ; scripts/test.sh
contracts:        ; scripts/generate-contracts.sh
check-contracts:  ; scripts/check-contracts.sh
smoke:            ; scripts/smoke-test.sh
migrate:          ; cd apps/api && uv run alembic upgrade head
up:               ; docker compose -f infra/compose.yaml up -d --build
down:             ; docker compose -f infra/compose.yaml down
board:            ; tools/board unread
