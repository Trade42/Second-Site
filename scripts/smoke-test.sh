#!/usr/bin/env bash
# Integrated smoke test against a RUNNING API (default http://localhost:8000):
# project -> confirmed profile -> LA candidates -> permits -> feasibility -> costs -> ranking -> events.
set -euo pipefail
API="${API_BASE_URL:-http://localhost:8000}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
py() { python3 -c "import json,sys; d=json.load(sys.stdin); $1"; }
step() { printf '%-48s' "$1"; }

step "health";   curl -sf "$API/health" | py 'print(d["status"])'
step "ready";    curl -sf "$API/ready" | py 'print(d["status"], d["checks"])'
TOKEN=$(curl -sf -X POST "$API/api/v1/dev/token" | py 'print(d["access_token"])')
H=(-H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json")
BODY=$(python3 -c "import json; f=json.load(open('$ROOT/fixtures/projects/facial_studio.json')); print(json.dumps({k:f[k] for k in ('project_name','market','business_profile')}))")
step "create project (3-room facial studio)"; PID=$(curl -sf "${H[@]}" -X POST "$API/api/v1/projects" -d "$BODY" | py 'print(d["project"]["project_id"])'); echo "$PID"
step "property search + analysis task"; curl -sf "${H[@]}" -X POST "$API/api/v1/projects/$PID/properties/search" -d '{"city":"Los Angeles","state":"CA"}' | py 'print(d["task"]["status"]); assert d["task"]["status"]=="COMPLETED"'
step "properties loaded";  curl -sf "${H[@]}" "$API/api/v1/projects/$PID/properties" | py 'print(len(d)); assert len(d)>=2'
step "permit evidence attached"; curl -sf "${H[@]}" "$API/api/v1/projects/$PID/properties" | py 'n=sum(len(p["permit_records"]) for p in d); print(n); assert n>0'
step "infrastructure unknowns represented"; curl -sf "${H[@]}" "$API/api/v1/projects/$PID/properties" | py 'n=sum(a["status"]=="UNKNOWN" for p in d for a in p["infrastructure_assessments"]); print(n); assert n>0'
step "cost scenarios"; curl -sf "${H[@]}" "$API/api/v1/projects/$PID/cost-scenarios" | py 'print([s["long_term_cost"] for s in d])'
step "ranking persisted"; curl -sf "${H[@]}" "$API/api/v1/projects/$PID/rankings" | py 'r=d[-1]; print("v",r["ranking_version"], [x["rank"] for x in r["ranked_properties"]])'
step "activity events"; curl -sf "${H[@]}" "$API/api/v1/projects/$PID/events" | py 't={e["event_type"] for e in d["events"]}; print(len(d["events"])); assert "property.ranking.updated" in t'
echo "SMOKE TEST PASSED (project $PID)"
