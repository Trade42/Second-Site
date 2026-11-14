# Provider verification log

| Provider | What was verified | How | Result |
|---|---|---|---|
| LA Open Data | dataset names, columns, coverage, SoQL `$where/$limit/$offset`, 3 teammate permits | live curl 2026-11-14 | ✅ all three permits exist as described; unit lives in `primary_address`; coverage 2020→present |
| LangChain Deep Agents | `create_deep_agent` signature, `SubAgent` keys, checkpointer param | reference docs + `inspect.signature` on installed 0.7.38 + executed graph in tests | ✅ |
| Photon 2.0 beta | API origin, Users/Spaces/Messages model, send body, Idempotency-Key, Standard Webhooks headers + HMAC format, retries, text+images only, shared line can't message first, voice = SIP to external agent | docs pages (beta) | ✅ docs; ❌ no account/credentials |
| Telnyx | `POST /v2/calls` (connection_id,to,from), `POST /v2/conferences` (call_control_id,name), `POST /v2/conferences/{id}/actions/join`, Ed25519 headers | OpenAPI reference pages | ✅ endpoints; ⚠️ signed-message format `"{timestamp}\|{body}"` follows the SDK helper — confirm on first live webhook |
| xAI voice | SIP number registration, `realtime.call.incoming` Standard Webhooks, `wss://api.x.ai/v1/realtime?call_id=`, `session.update`/`response.create` | docs.x.ai | ✅ docs; ❌ no key |
| CoStar / Matterport / Vexcel | toolkit exposes SDKs, MCP servers, OpenAPI for LA + Houston (track description) | special-tracks doc | ❌ no toolkit docs/keys in this environment — **no methods implemented** |
| IFC-Bench | dataset files, license CC-BY-4.0, dental_clinic arc/mep/str | HF API | ✅ metadata; files not downloaded |
