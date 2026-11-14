#!/usr/bin/env bash
# Smoke test of the demo path against a RUNNING Rust service: the Rust counterpart of
# scripts/smoke-test.sh, extended into the design studio. Seeds the way the web app does:
# health and ready -> dev token -> fixture project (profile included) -> property search and
# analysis -> properties, permit evidence, UNKNOWN assessments -> cost scenarios -> ranking ->
# events -> layouts for the best-ranked property with measured geometry -> task result ->
# layout list -> geometry -> one revision -> bill of materials -> events.
#
# Usage: apps/backend/scripts/smoke.sh [BASE_URL]
#   BASE_URL defaults to $API_BASE_URL, else http://localhost:8000. Needs curl and python3.
#   SMOKE_TIMEOUT: seconds per request (default 120; the search and the layout solver run inline).
#
# One line per step; the first failed assertion prints FAIL and exits 1. Assertions follow the
# truth rules: fixture data must be labelled as fixture data, UNKNOWN stays UNKNOWN in the ranking,
# money is integer minor units, a layout is PROPOSED and never APPROVED, and bill prices are
# illustrative fixture prices, not supplier quotes. Every run creates a new project.
set -euo pipefail

API=${1:-${API_BASE_URL:-http://localhost:8000}}
API=${API%/}
TIMEOUT=${SMOKE_TIMEOUT:-120}
command -v curl >/dev/null || { echo "smoke: curl is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "smoke: python3 is required" >&2; exit 2; }
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

n=0
name=
step() { n=$((n + 1)); name=$1; }
pass() { printf '%2d  %-38s ok    %s\n' "$n" "$name" "$*"; }
fail() {
  printf '%2d  %-38s FAIL  %s\n' "$n" "$name" "$*"
  printf 'SMOKE TEST FAILED at step %d (%s) against %s\n' "$n" "$name" "$API" >&2
  exit 1
}

# Python helpers: J(name) loads a saved response, need(cond, msg) fails the step with msg.
PRELUDE='import json, os, sys
TMP, ARGS = sys.argv[1], sys.argv[2:]
def J(name):
    with open(os.path.join(TMP, name + ".json"), encoding="utf-8") as f:
        return json.load(f)
def need(cond, msg):
    if not cond:
        print(msg)
        sys.exit(3)
def is_money(v):
    return isinstance(v, int) and not isinstance(v, bool)
'
py() { python3 -I -c "$PRELUDE$1" "$TMP" "${@:2}"; }
# check CODE [ARGS...]: run CODE; its output is the step's detail (or the failure message).
check() {
  local out
  out=$(py "$@" 2>&1) || fail "$out"
  pass "$out"
}

# http SAVE METHOD PATH STATUS [BODY_FILE]: the response body is saved as $TMP/SAVE.json.
http() {
  local save=$1 method=$2 path=$3 want=$4 body=${5-} code rc detail
  local args=(-sS -X "$method" -o "$TMP/$save.json" -w '%{http_code}' --max-time "$TIMEOUT"
    -H 'Accept: application/json')
  if [[ -f $TMP/auth.h ]]; then args+=(-H "@$TMP/auth.h"); fi
  if [[ -n $body ]]; then args+=(-H 'Content-Type: application/json' --data-binary "@$body"); fi
  if ! code=$(curl "${args[@]}" "$API$path" 2>"$TMP/curl.err"); then
    rc=$?
    fail "$method $path: no HTTP answer from $API (curl exit $rc: $(tr '\n' ' ' <"$TMP/curl.err"))"
  fi
  if [[ $code != "$want" ]]; then
    detail=$(py 'd = J(ARGS[0]); print("%s: %s" % (d.get("error_code"), d.get("message")) if isinstance(d, dict) else str(d)[:300])' "$save" 2>/dev/null) \
      || detail=$(head -c 300 "$TMP/$save.json")
    fail "$method $path: HTTP $code, expected $want ($detail)"
  fi
}

echo "Smoke test of the Rust service at $API"

step "health"
http health GET /health 200
check 'd = J("health")
need(d.get("status") == "ok", "status is %r, expected ok" % d.get("status"))
print("status ok, version %s" % d.get("version"))'

step "ready (honest about the store)"
http ready GET /ready 200
check 'd = J("ready")
db = d.get("checks", {}).get("database")
need(db, "/ready has no checks.database")
if db.startswith("memory"):
    need(d.get("status") == "degraded", "the memory store must be reported degraded, got %r" % d.get("status"))
else:
    need(db == "ok", "database check is %r" % db)
    need(d.get("status") == "ready", "status %r although the database check is ok" % d.get("status"))
providers = d.get("providers") or []
need(providers, "/ready lists no providers")
print("%s; database: %s; %d providers reported" % (d["status"], db, len(providers)))'

step "dev token"
http token POST /api/v1/dev/token 200
TOKEN=$(py 'd = J("token")
need(d.get("token_type") == "bearer" and d.get("access_token"), "no bearer token in %r" % d)
print(d["access_token"])' 2>&1) || fail "$TOKEN"
(umask 077 && printf 'Authorization: Bearer %s\n' "$TOKEN" >"$TMP/auth.h")
pass "bearer token for a dev user"

step "fixture project (labelled)"
http fixture GET /api/v1/dev/fixtures/project 200
check 'd = J("fixture")
origin = (d.get("_meta") or {}).get("data_origin")
need(origin == "FIXTURE_SYNTHETIC", "the seed project is not labelled as fixture data (_meta.data_origin=%r)" % origin)
need(d.get("business_profile"), "the fixture has no business_profile")
body = {k: d[k] for k in ("project_name", "market", "business_profile")}
with open(os.path.join(TMP, "create_body.json"), "w", encoding="utf-8") as f:
    json.dump(body, f)
search = {"city": d["market"]["city"], "state": d["market"]["state"]}
with open(os.path.join(TMP, "search_body.json"), "w", encoding="utf-8") as f:
    json.dump(search, f)
print("%r, %s" % (d["project_name"], origin))'

step "create project with its profile"
http project POST /api/v1/projects 201 "$TMP/create_body.json"
PID=$(py 'd = J("project")
pid = d["project"]["project_id"]
bp = d.get("business_profile")
need(bp, "the created project has no business profile")
need(bp.get("confirmation_status") == "CONFIRMED", "profile is %r; the search needs the confirmed fixture profile" % bp.get("confirmation_status"))
refs = [r.get("source") or "" for r in bp.get("source_references") or []]
need(any(s.startswith("fixture:") for s in refs), "the profile does not name its fixture source: %r" % refs)
print(pid)' 2>&1) || fail "$PID"
P=/api/v1/projects/$PID
pass "project $PID; profile CONFIRMED in the fixture (source fixture:facial_studio.json), no separate confirmation needed"

step "property search and analysis"
http search POST "$P/properties/search" 202 "$TMP/search_body.json"
STATUS_URL=$(py 'd = J("search")
t = d["task"]
need(t["status"] == "COMPLETED", "search task is %s (%s: %s)" % (t["status"], t.get("error_code"), t.get("error_message")))
need(t["output_reference"].get("property_ids"), "the search found no property")
print(d["status_url"])' 2>&1) || fail "$STATUS_URL"
check 'o = J("search")["task"]["output_reference"]
print("task COMPLETED inline: %d properties, ranking v%s" % (len(o["property_ids"]), o.get("ranking_version")))'

step "search task status"
http search_task GET "$STATUS_URL" 200
check 't, s = J("search_task"), J("search")["task"]
need(t["task_id"] == s["task_id"] and t["status"] == "COMPLETED", "task %s is %s" % (t["task_id"], t["status"]))
print("%s %s (%s)" % (t["task_type"], t["status"], t["assigned_subsystem"]))'

step "properties (fixture data labelled)"
http props GET "$P/properties" 200
check 'props = J("props")
need(len(props) >= 2, "expected at least 2 properties, got %d" % len(props))
origins = []
for p in props:
    street = p["property_identity"]["street_address"]
    listing = p.get("commercial_listing") or {}
    need(listing.get("data_origin"), "%s: listing has no data_origin" % street)
    if listing.get("provider") == "fixture":
        need(listing["data_origin"].startswith("FIXTURE"), "%s: fixture listing labelled %r" % (street, listing["data_origin"]))
    for a in p.get("interior_assets") or []:
        if a.get("provider") == "fixture":
            need(str(a.get("data_origin")).startswith("FIXTURE"), "%s: fixture scan labelled %r" % (street, a.get("data_origin")))
    origins.append(listing["data_origin"])
print("%d properties; listings: %s" % (len(props), ", ".join(sorted(set(origins)))))'

step "permit evidence attached"
check 'props = J("props")
permits = [(p, r) for p in props for r in p.get("permit_records") or []]
need(permits, "no permit record is attached to any property")
for p, r in permits:
    need(r.get("property_id") == p["property_id"], "permit %s is attached to another property" % r.get("permit_number"))
    need(r.get("data_origin") and r.get("scope"), "permit %s has no data_origin or scope" % r.get("permit_number"))
origins = sorted({r["data_origin"] for _, r in permits})
scopes = sorted({r["scope"] for _, r in permits})
print("%d permit records (%s; scopes %s)" % (len(permits), ", ".join(origins), ", ".join(scopes)))'

step "infrastructure UNKNOWNs represented"
check 'props = J("props")
unknown = sum(a["status"] == "UNKNOWN" for p in props for a in p.get("infrastructure_assessments") or [])
need(unknown > 0, "no UNKNOWN infrastructure assessment; the fixture data has unknowns")
statuses = sorted({a["status"] for p in props for a in p.get("infrastructure_assessments") or []})
print("%d UNKNOWN assessments (statuses seen: %s)" % (unknown, ", ".join(statuses)))'

step "cost scenarios (integer money)"
http costs GET "$P/cost-scenarios" 200
check 'costs = J("costs")
need(costs, "no cost scenario")
fields = ("opening_cost_low", "opening_cost_expected", "opening_cost_high", "upfront_cash_expected", "monthly_operating_cost", "long_term_cost")
for c in costs:
    for k in fields:
        need(is_money(c.get(k)), "scenario %s: %s=%r is not integer minor units" % (c["scenario_id"], k, c.get(k)))
    for li in c.get("line_items") or []:
        for k in ("low", "expected", "high"):
            need(is_money(li.get(k)), "line item %s: %s=%r is not integer minor units" % (li.get("line_item_id"), k, li.get(k)))
print("%d scenarios; long-term cost (cents): %s" % (len(costs), ", ".join(str(c["long_term_cost"]) for c in costs)))'

step "ranking persisted (UNKNOWN counted)"
http rankings GET "$P/rankings" 200
check 'rankings, props = J("rankings"), {p["property_id"]: p for p in J("props")}
need(rankings, "no ranking")
r = max(rankings, key=lambda x: x["ranking_version"])
ranked = sorted(r["ranked_properties"], key=lambda x: x["rank"])
need(ranked, "the ranking is empty")
for e in ranked:
    p = props.get(e["property_id"])
    need(p, "ranked property %s is not in the project" % e["property_id"])
    unknown = sum(a["status"] == "UNKNOWN" for a in p.get("infrastructure_assessments") or [])
    need(e["unknown_requirement_count"] == unknown, "%s: ranking counts %d unknowns, the property has %d UNKNOWN assessments" % (p["property_identity"]["street_address"], e["unknown_requirement_count"], unknown))
print("v%d: %s" % (r["ranking_version"], "; ".join("#%d %s%s" % (e["rank"], props[e["property_id"]]["property_identity"]["street_address"], " (disqualified)" if e["disqualified"] else "") for e in ranked)))'

step "activity events"
http events GET "$P/events?limit=500" 200
check 'types = {e["event_type"] for e in J("events")["events"]}
for t in ("property.search.completed", "property.ranking.updated", "task.completed"):
    need(t in types, "no %s event" % t)
print("%d events, %d types" % (len(J("events")["events"]), len(types)))'

step "best-ranked property with measured geometry"
PROP=$(py 'rankings, props = J("rankings"), {p["property_id"]: p for p in J("props")}
r = max(rankings, key=lambda x: x["ranking_version"])
for e in sorted(r["ranked_properties"], key=lambda x: x["rank"]):
    p = props[e["property_id"]]
    scans = [a for a in p.get("interior_assets") or [] if a.get("has_measured_geometry")]
    if scans:
        print(p["property_id"], e["rank"], scans[0]["asset_id"], p["property_identity"]["street_address"])
        break
else:
    need(False, "no ranked property has an interior scan with measured geometry")' 2>&1) || fail "$PROP"
read -r PROP_ID PROP_RANK SCAN_ID PROP_STREET <<<"$PROP"
pass "rank $PROP_RANK: $PROP_STREET (scan $SCAN_ID)"

# The room program of apps/api/tests/test_api_smoke.py and the parity vectors.
cat >"$TMP/generate_body.json" <<EOF
{"property_id": "$PROP_ID", "room_requirements": [
  {"room_type": "treatment", "count": 3, "min_area_sqm": 10, "min_width_m": 2.8, "needs_plumbing": true},
  {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
  {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5}]}
EOF

step "layout generation"
http generate POST "$P/layouts/generate" 202 "$TMP/generate_body.json"
LAYOUT_URL=$(py 'd = J("generate")
t = d["task"]
need(t["task_type"] == "generate_layout", "task type %r" % t["task_type"])
need(t["status"] == "COMPLETED", "layout task is %s (%s: %s)" % (t["status"], t.get("error_code"), t.get("error_message")))
print(d["status_url"])' 2>&1) || fail "$LAYOUT_URL"
check 't = J("generate")["task"]
print("task %s COMPLETED (%s)" % (t["task_id"], t["assigned_subsystem"]))'

step "layout task result"
http generate_task GET "$LAYOUT_URL" 200
V1=$(py 't = J("generate_task")
need(t["status"] == "COMPLETED", "task is %s" % t["status"])
ids = t["output_reference"].get("design_version_ids") or []
need(ids, "the task produced no design version")
print(ids[0])' 2>&1) || fail "$V1"
check 'print("%d design versions" % len(J("generate_task")["output_reference"]["design_version_ids"]))'

step "layout list (PROPOSED, never APPROVED)"
http layouts GET "$P/layouts" 200
GEOMETRY_ID=$(py 'layouts = J("layouts")
made = J("generate_task")["output_reference"]["design_version_ids"]
by_id = {l["design_version_id"]: l for l in layouts}
for i in made:
    need(i in by_id, "design version %s is not listed" % i)
for l in layouts:
    need(l["approval_status"] != "APPROVED", "layout v%d is APPROVED; nothing may approve a layout here" % l["version_number"])
for i in made:
    l = by_id[i]
    need(l["approval_status"] == "PROPOSED", "new layout v%d is %s, expected PROPOSED" % (l["version_number"], l["approval_status"]))
    rep = l["validation_report"]
    need(rep["valid"], "new layout v%d is invalid: %r" % (l["version_number"], rep["issues"]))
    need("Not a code-compliance review" in rep["disclaimer"], "layout v%d lost the disclaimer" % l["version_number"])
    need(l["property_id"] == ARGS[0], "layout v%d is for another property" % l["version_number"])
print(by_id[made[0]]["geometry_id"])' "$PROP_ID" 2>&1) || fail "$GEOMETRY_ID"
check 'made = J("generate_task")["output_reference"]["design_version_ids"]
vs = sorted(l["version_number"] for l in J("layouts") if l["design_version_id"] in made)
print("versions %s PROPOSED and valid; disclaimer kept" % ", ".join("v%d" % v for v in vs))'

step "measured geometry (labelled)"
http geometry GET "$P/layouts/geometry/$GEOMETRY_ID" 200
check 'g = J("geometry")
need(g.get("source_id") == ARGS[0], "geometry comes from %r, not the scan %s" % (g.get("source_id"), ARGS[0]))
need(g.get("data_origin"), "geometry has no data_origin")
if g.get("geometry_source") == "FIXTURE":
    need(g["data_origin"].startswith("FIXTURE"), "fixture geometry labelled %r" % g["data_origin"])
print("%s, %s, scale_verified=%s" % (g.get("geometry_source"), g["data_origin"], g.get("scale_verified")))' "$SCAN_ID"

cat >"$TMP/revise_body.json" <<'EOF'
{"requested_changes": ["add a fourth treatment room"], "room_requirement_overrides": [
  {"room_type": "treatment", "count": 4, "min_area_sqm": 10, "min_width_m": 2.8, "needs_plumbing": true},
  {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
  {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5}]}
EOF

step "one revision"
http revise POST "$P/layouts/$V1/revisions" 202 "$TMP/revise_body.json"
V_NEW=$(py 't = J("revise")["task"]
need(t["status"] == "COMPLETED", "revision task is %s (%s: %s)" % (t["status"], t.get("error_code"), t.get("error_message")))
o = t["output_reference"]
need(o.get("revision_status") == "APPLIED", "revision_status is %r" % o.get("revision_status"))
need(o.get("design_version_ids"), "the revision produced no version")
print(o["design_version_ids"][0])' 2>&1) || fail "$V_NEW"
http layouts_after GET "$P/layouts" 200
check 'by_id = {l["design_version_id"]: l for l in J("layouts_after")}
l = by_id.get(ARGS[1])
need(l, "revised version %s is not listed" % ARGS[1])
need(l["parent_version_id"] == ARGS[0], "revised version has parent %r, expected %s" % (l["parent_version_id"], ARGS[0]))
need(l["approval_status"] == "PROPOSED", "revised version is %s, expected PROPOSED" % l["approval_status"])
need(not any(x["approval_status"] == "APPROVED" for x in by_id.values()), "a layout is APPROVED")
need(l["validation_report"]["valid"], "the revised layout is invalid")
treat = sum(r.get("room_type") == "treatment" for r in l["room_polygons"])
need(treat == 4, "revised layout has %d treatment rooms, expected 4" % treat)
print("v%d from v%d APPLIED: %d rooms, PROPOSED" % (l["version_number"], by_id[ARGS[0]]["version_number"], len(l["room_polygons"])))' "$V1" "$V_NEW"

step "bill of materials (not supplier quotes)"
http bom GET "$P/layouts/$V1/bom" 200
check 'b = J("bom")
items = b.get("items") or []
need(items, "the bill has no items")
need(b["design_version_id"] == ARGS[0], "bill is for %s" % b["design_version_id"])
need(b.get("status") != "APPROVED" and b.get("approved_total") is None, "the bill claims approval (%s, approved_total=%r)" % (b.get("status"), b.get("approved_total")))
need(is_money(b.get("estimated_total")) and b["estimated_total"] > 0, "estimated_total=%r is not positive integer minor units" % b.get("estimated_total"))
for it in items:
    need(is_money(it.get("unit_price_estimate")), "%s: unit price %r is not integer minor units" % (it["item_type"], it.get("unit_price_estimate")))
    need(str(it.get("price_origin")).startswith("FIXTURE"), "%s: price_origin %r is not a fixture label" % (it["item_type"], it.get("price_origin")))
    need("not a supplier quote" in (it.get("price_note") or "").lower(), "%s: price note does not say it is not a supplier quote: %r" % (it["item_type"], it.get("price_note")))
print("%d items, estimated %d cents, status %s; fixture prices, not supplier quotes" % (len(items), b["estimated_total"], b["status"]))' "$V1"

step "events after the design studio"
http events_after GET "$P/events?limit=500" 200
check 'evs = J("events_after")["events"]
types = [e["event_type"] for e in evs]
for t in ("layout.version_created", "bom.generated"):
    need(t in types, "no %s event" % t)
need("layout.approved" not in types, "a layout.approved event exists")
print("%d events (%d layout.version_created, %d bom.generated)" % (len(evs), types.count("layout.version_created"), types.count("bom.generated")))'

echo "SMOKE TEST PASSED (project $PID)"
