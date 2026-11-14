# Oracle Cloud (or any VM) deployment runbook

Persistent FastAPI + worker on one VM behind a TLS reverse proxy. **This runbook creates nothing
automatically and assumes no paid or free-tier resource exists.** It is portable to any Linux VM
(Fly, Render VM, EC2, a lab box).

## 1. Provision (manual, with the team's approval)
- Ubuntu 22.04+ VM (OCI Ampere A1 or x86), 2 vCPU / 4–8 GB RAM, public IP.
- Open ingress 80/443 only (security list / NSG). Keep 5432/6379 private.
- A DNS name pointing at the VM (needed for HTTPS and for Photon/Telnyx webhooks — Photon refuses
  non-HTTPS and private destinations).

## 2. Install
```bash
sudo apt-get update && sudo apt-get install -y docker.io docker-compose-plugin caddy git
git clone https://github.com/Trade42/Second-Site && cd Second-Site
cp .env.example .env   # fill real values on the server only; never commit
```
Set at least: `APP_ENV=production`, `ALLOW_DEV_TOKENS=false`, a strong `AUTH_SECRET`,
`PUBLIC_BASE_URL=https://<host>`, `CORS_ORIGINS=https://<web-host>`, `TASK_EXECUTION_MODE=redis`.

## 3. Run
```bash
docker compose -f infra/compose.yaml up -d --build postgres redis api worker
```

## 4. Reverse proxy (HTTPS + WebSocket/SSE)
`infra/oracle/Caddyfile` terminates TLS (automatic certificates), proxies `/` to the API, and
disables buffering so SSE (`/events/stream`) and future voice WebSockets work.
```bash
sudo cp infra/oracle/Caddyfile /etc/caddy/Caddyfile && sudo sed -i "s/api.example.com/<host>/" /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

## 5. Verify
`curl https://<host>/health` and `curl https://<host>/ready` (provider table shows which integrations are LIVE vs MOCK).
Then register webhook URLs: Photon -> `https://<host>/webhooks/photon`, Telnyx -> `https://<host>/webhooks/telnyx`.
