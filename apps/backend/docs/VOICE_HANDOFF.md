# Voice calls: handoff note (2026-11-15)

From this work's build and review reports; code read for names only. **Nothing here has placed a call.** **mock** = tested against local fakes and fixtures; **read** = read, not run. The only real provider traffic in the reports: 6 short xAI sessions from Python scripts (synthetic text; plus token mints and reads) and 4 Telnyx account GETs.

## 1. What this is

- Our server dials through Telnyx and bridges the call's media stream to xAI realtime. No hosted xAI agent id.
- Our backend builds each call's instructions: client, property facts, missing information, approved questions, rules.
- The server decides: rules read every line the other side says; commitments and sensitive requests hold the call and ask the client; the model's approval tools always get "not approved".
- A call needs an APPROVED `PLACE_CALL` approval, checked in code (exact payload, unexpired, single use).
- Off by default: every switch in section 3 is off, empty, `fixture` or local in `.env.example`.

**Ten minutes (mock):** a fixture carrier and a fixture bridge, no network.

```
cd apps/backend
cargo test -p ss-server --test voice_call --test voice_purposes
VOICE_WIRE_PRINT=1 cargo test -p ss-server --test voice_call \
  the_server_decides_on_a_simulated_bridged_call::memory -- --nocapture
```

The second prints the `session.update` and the opening line. Both were run for this note and by its reviewer (in memory: 24 and 12 passed; `voice_purposes` 19 with section 7's `instruct` commit; PostgreSQL variants need `TEST_DATABASE_URL`). Over HTTP with the fixture provider (`POST …/calls/{call_id}/simulate`) the bridge is off: the older scripted call runs, and the client is told the tour and rent limits were not used.

**A call with a tour and rent limits** (synthetic), `POST /api/v1/projects/{project_id}/outreach`:

```json
{"property_id":"<uuid>","channel":"VOICE","purpose":"Tour and rent",
 "recipient":{"role":"broker","name":"Dana Broker","phone":"+15555550100"},
 "questions":["Is there a loading dock?"],
 "purposes":{"tour":{"windows":[{"start":"2026-11-18T15:00:00-08:00","end":"2026-11-18T17:00:00-08:00"}],"duration_minutes":30},
  "negotiation":{"target_monthly_rent_cents":400000,"max_monthly_rent_cents":451700,"max_counters":1}}}
```

Then `POST /api/v1/approvals/{approval_id}/decision` with `{"decision":"APPROVED","action_payload_hash":"<returned>"}`, then `POST /api/v1/projects/{project_id}/calls` with `{"communication_id":"<returned>"}`. Fixture mode (the default) dials nothing; **live mode dials `recipient.phone`**. Above 1 counter the maximum can be heard (section 6).

## 2. The map

`S` = `apps/backend/crates/server/src`, `D` = `apps/backend/crates/domain/src`. Counts: the reports' last, in memory, all **mock**. A = ADOPT, AF = ADOPT_WITH_FIXES_APPLIED, AO = ADOPT_AFTER_OWED_FIXES.

| File | Does | Tests | Review |
|---|---|---|---|
| `S/services/voice_bridge.rs`, `S/integrations/xai_realtime.rs` | Telnyx stream ↔ xAI: opening line first, one response at a time, interruption, tools, `instruct` | `voice_bridge` 28 | rounds 1 and 2 AF |
| `D/integrations/telnyx.rs`, `S/integrations/telnyx.rs` | stream, transcription and playback commands | `telnyx` 27 | A |
| `S/services/intro_recording.rs`, `S/http/routes/recordings.rs` | the client's recorded introduction | `intro_recording` 7 | AO |
| `S/http/mod.rs` `loggable_path`, `routes/media.rs` | media link redacted in the log; `nosniff` | `media_log` 5 | A |
| `D/communications/response_agent.rs` | rules, no language model: findings, holds, redaction | `response_agent` 27 | round 3 A |
| `D/communications/call_purposes.rs` | tour times, rent limits, their two tools | `call_purposes` 31 | round 2 A |
| `D/communications/call_summary.rs` | summary and the client's text | `call_summary` 13 | AF |
| `S/services/call_orchestration.rs`, `voice_session_config.rs`, `S/http/routes/voice_stream.rs` | the call's states, the instructions, the stream route | `voice_call` 24, `voice_purposes` 12 | wiring AF, glue AF |
| `S/http/mod.rs` `public_path_guard` | what a tunnelled request may reach | `public_guard` 18 | A |
| `apps/api/app/agents/voice_session.py` | browser "talk to the assistant" | pytest `test_voice_session.py` 21 | AF |

No report shows a whole `ss-server` run after the glue (before it, guard writer: 560 in memory), a clean whole PostgreSQL run, or the e2e script.

**The call's life** (functions in `call_orchestration.rs` unless named):

1. Request: `routes/communications.rs::outreach` stores a PENDING approval; questions and purposes stay server side (`stash_request`), bound to it by `request_digest`. Approval is given in the app, never by text.
2. Start: `start` → `require_approved`, `intro_recording::active_for_call`, dial, `begin_purposes` (`call_purposes::approve`).
3. Answer: `answered_bridged`. With an ACTIVE recording only `play` runs; `bridged_event` waits for `call.playback.ended` of our URL with `status: completed`, else `fail_politely` (apology, hangup). No recording: straight on. A machine: hung up without a word.
4. Bridge: `begin_bridge` builds `voice_session_config`, registers the bridge with a random token, runs `response_agent::start` and `start_streaming`; Telnyx connects to `GET /api/v1/voice/stream/{bridge_id}?token=…` (`voice_bridge::accept_carrier`). The opening line ("Hello, this is an AI assistant, an automated system, calling on behalf of {client} about {property}.") is spoken verbatim first.
5. Each line they say: `bridge_input` → `on_bridge_utterance` → `response_agent::step`; findings → `record_finding`; exact lines → `apply_say`. Only redacted text is stored (misses: section 6).
6. Tools: `on_bridge_tool`. `request_owner_approval` and `escalate_to_owner` get `not_approved_output` / `owner_asked_output` and raise `request_handoff`. `check_tour_time` and `evaluate_rent` → `on_purpose_tool` → `call_purposes::dispatch`; `notify_outcomes` tells the client.
7. Hold: `request_handoff` pauses the bridge; the client answers (`POST …/handoffs/{id}/response`) → `respond` (conference, dial the client) → `owner_leg_event`: `owner_joined_line` only on HUMAN_JOINED; when the client hangs up, AI_RESUMED and `agent_resume`.
8. End: `call.hangup` → `advance` → `VoiceCalls::close` → `finalize` → `voice_summary` (`call_summary`, `call_purposes::owner_lines`) → `notify_owner`.

## 3. The switches

| Setting | Alone it does |
|---|---|
| `VOICE_PROVIDER_MODE=live`, `TELNYX_API_KEY` | the live Telnyx provider; otherwise the fixture only records |
| `ALLOW_LIVE_OUTBOUND_CALLS=true` | without it a live dial is refused before any request (mock) |
| `TELNYX_CONNECTION_ID`, `TELNYX_FROM_NUMBER` | the Call Control app and caller ID; no number: a placeholder (read) |
| `TELNYX_PUBLIC_KEY` | webhook signature; unset, every delivery is refused 400 (read) |
| `PUBLIC_BASE_URL` | public HTTPS for webhooks, the recording link, the `wss` stream |
| `VOICE_BRIDGE_ENABLED=true`, `XAI_API_KEY` | with live mode, puts Grok on the call |
| `PUBLIC_PATH_GUARD=true` | the tunnel guard below |

No dial reaches Telnyx without `VOICE_PROVIDER_MODE=live`, `TELNYX_API_KEY`, `ALLOW_LIVE_OUTBOUND_CALLS=true` and an APPROVED approval. From the commit after the `instruct` wiring, a live dial is also refused (403, before any request to Telnyx; the approval stays APPROVED) while `TELNYX_CONNECTION_ID`, `TELNYX_FROM_NUMBER` (`+` and 8 to 15 digits), `TELNYX_PUBLIC_KEY`, or a public `https` `PUBLIC_BASE_URL` with no path and no trailing slash is missing, and with `VOICE_BRIDGE_ENABLED` also `XAI_API_KEY`; the refusal names each setting, and `/ready` shows the same list as `checks.telnyx_live_dial` before any attempt (**mock** carrier only). Live mode without `TELNYX_API_KEY` is not refused: the fixture provider is selected and the call is simulated; `/ready` names the key. Not checked: that the public key is the right one, or that the address reaches this server; with either wrong a phone can still ring while the server gets no event. **The bridge switches do not gate the dial**: with them off and the others on, the older flow (fixed questions by Telnyx speech) would place a real call. Each switch alone at run time: read, not run.

**Guard and tunnel** (mock; never met a real tunnel). With the guard on, a request not sent to a loopback host name, or carrying a forwarding header, gets 404, except `POST /webhooks/telnyx`, `GET /api/v1/voice/stream/{id}`, `GET`/`HEAD /api/v1/media/public/{token}`, `GET /record`, `POST /record/check`, `POST /record/upload`. Before Telnyx gets the address, both must print `404`:

```
curl -s -o /dev/null -w '%{http_code}\n' https://<tunnel>/health
curl -s -o /dev/null -w '%{http_code}\n' -X POST https://<tunnel>/api/v1/dev/token
```

Also: `AUTH_SECRET` must not be a published placeholder (guard F1: a stranger could sign media and recording links); no `--http-host-header localhost`; bind to 127.0.0.1.

## 4. Never run against reality

- **xAI from Rust:** the `wss:` TLS connection (only local `ws://` ran); `instruct` (its `input_text` type, client id and delete); `pcmu` audio regrouped to 160 bytes.
- **xAI from anywhere:** a function tool call and its output (and `response_id` on it); the phone instructions; `max_duration`.
- **Telnyx:** a dial; `streaming_start`'s answer; `streaming.failed`'s payload; the media stream (`media.track`, `chunk` order, `clear` and `mark`, pacing, which leg `opposite` is); machine detection; playback events and their `status`; a WAV played with no `audio_type`; a webhook signed with a real key; the conference; carrier speech while the stream is paused.
- **A phone**: none has rung. **A microphone, a browser**: the recording page has recorded nothing. **A tunnel, a process restart, real speech.**

## 5. The first real call: what to watch

Before: the two curls; no `log`-crate backend at trace level (bridge V8, R5: it would print the xAI key and each frame).

1. `call.answered` passes the signature (request log `POST /webhooks/telnyx`; 400 = key).
2. A person taken for a machine is hung up on silently (transcript "[answering machine detected; …]"), even with the client on the line.
3. Recording: Telnyx fetches `/api/v1/media/public/[redacted]` (200), plays the WAV, and `call.playback.ended` carries our `media_url` and `status: "completed"`. If not: transcript "did not play to its end (…)", apology, hangup.
4. `streaming_start` answers `{"data":{"result":"ok"}}`.
5. Telnyx reaches the stream route with its token (401 = lost); the upgrade through the guard is untested.
6. Rust reaches xAI; a failure is the bridge's end reason in the transcript (`provider_handshake…`, `provider_connect_timeout`). With `instruct`: `conversation.item.created` and `.deleted` come back, no `error`.
7. The callee hears us (if not, try target leg `self`): smooth, no echo of ours, interruption works, short delay.
8. The opening line is heard first, in full; a cut ends the call `opening_line_not_spoken`.
9. Harmless sentences hold the call (three bare amounts; "a sign out front"): state HANDOFF_PENDING.
10. The model calls a tool before it says a time or a number, and says the returned line; the server cannot stop a number said without the tool. The withheld closing does not leave the call hanging.
11. Hold: carrier speech while paused is audible; "{client} has joined the call." only after the conference join.
12. End: the hangup 8 s after the closing line may cut a goodbye; the client's summary text.

## 5a. Who speaks when (voice-turns-1, after the second real call)

One speaker per counterparty turn. xAI's server VAD answers every sentence of the other side by
itself; its documentation has no setting that switches that off (`turn_detection` has `type` and
`idle_timeout_ms` only, and `type: null` means committing every turn by hand). So the model
leads from its instructions and the server watches.

- **The model** (its automatic response): every reply to the other side, and the approved
  questions, one at a time, from the numbered list in its instructions.
- **The server, as a silent direction** (`instruct`, a system item, nothing spoken): the state of
  the questions by their numbers (asked, open, still to ask). It is made from the ASSISTANT's
  transcript line (`on_assistant_line`), so it is in place before the other side answers. The
  server never pushes a next question and never says a cannot-answer line on a bridged call.
- **The server, spoken** (`say` / `close`), only in these cases: the disclosure (opening line),
  a hold line and the owner-joined line (on the carrier, the bridge paused), the "I'm still
  here, take your time." line after 25 s of silence (at most twice until they speak), the
  closing line, and the apology when the provider fails.
- **The end**: when the line shows every askable question was put and none is open, the server
  calls `BridgeHandle::close`: the other side's audio stops going to the model, a response
  already being spoken finishes, any later one is cancelled unheard, the closing line is spoken
  (not interruptible), and the bridge ends `closed` when the carrier's mark says it was played
  (or after the line's length plus 3 s). Then the call is hung up.
- **A hang-up by the other side** (`carrier_closed`) is a normal end: a system line, no apology,
  no `speak`. Only a real failure (provider closed, idle timeout, an error) gets the apology.
- **"Hold on, let me look that up"** (`asks_for_time`): nothing is decided and nothing is said
  by the server; the open question stays open and the model acknowledges once.
- Every assistant line the server authored is marked where it is written
  (`VoiceCall::server_lines`, positions in the transcript; process-local) and is not judged by
  the summary's commitment check; the model's own words are.

## 6. Known limits and owed findings

- **Response Agent** (round-3 review): N1 "it ends in 2019" and N2 "… each" let a real identifier through, stored whole; N3 spoken prices in pairs and N4 (MAJOR) three bare amounts hold a harmless call as sensitive; N5 "Yeah, …" answers an open yes/no question as yes; N6 a bare "Yes." closes a value question with a null. Round-1 F11: a manager's words are stored `OWNER_REPORTED`. Its section 4: also stored whole (no keyword near): an identifier said in words, one under 9 digits, a birth date, a driver's licence. Fit for a supervised call, not unattended volume.
- **Purposes** (round-2 review F1; glue review R4): the last counter is the maximum itself, so holding out finds it when 2 or more counters are approved: **approve 0 or 1 counter if the maximum must stay private**. Probing continues across calls. F2: the outcome names their lowest number, not their latest. F4: the client's lines show the last 8 events. Round 1 R10: a window's offset is not checked against its date.
- **Bridge**: round-2 review R3 (MAJOR, owed before the first real call): a transcript handler stuck on an early line is never noticed, so the model speaks without the server's decisions; R6 if the apology's transaction fails the call stays open, silent; R4 its tests can time out (5 in about 300). Round 1: V8 above; V9 no `input_audio_buffer.clear` on pause, idle timer unbounded while paused; no reconnection. Glue review R2: a tool-call id already answered is not refused.
- **Wiring** (its review): R6 the recording's timeout task is untested; R7 no sweep if `call.hangup` never arrives; resume after the client leaves is unconditional; nothing can approve a commitment on a call; `play` sends no `audio_type: "wav"`; no log captured through a whole call. Glue review: R3 with `purposes` the Python service computes another approval hash; R5 the notice "{contact} said $X" is the model's tool argument, unchecked; R6 questions without purposes are outside the digest; R7 an unknown key inside `purposes` is ignored (Python: 422).
- **Process-local state** (wiring, glue): the stashed request, limits and Response Agent state live in memory. A restart before start: `APPROVAL_PAYLOAD_MISMATCH`, ask again; during a call: tools answer `not_approved`. Read, not run.
- **Recorded introduction** (review): R2 a revoked recording stops playing only because its file is deleted; R3 trailing bytes accepted; R4 a deleted user's audio stays. Log review M1: odd forms of a valid media token that 404 are logged whole.
- **Summary** (review): R5 a numeric finding value of 9+ digits is not redacted in the JSON; R7 the client's text can drop a short "No".
- **Guard** (review): F1 in section 3; F2 a refused path is logged whole, no rate limit.
- **Voice session route** (review): F1 at `LOG_LEVEL=DEBUG` the provider's response headers are logged; F2 the browser sends `session.update` itself and can add tools.
- **Disclosure text**: the contract default says "on the owner's behalf"; to a landlord "the owner" is the property's (wiring review R4).

**Reports that disagree** (writer / review). Recording: a live call needs a recording / wiring, by the owner's 07:16Z word: optional. Stream commands: ignored webhooks tested in INTRO and ACTIVE / F1: DIALING and INTRO. Glue: purposes R7 and R8 "still open" / purposes round 2: done.

## 7. Not built

- S7: sending the recording link through Photon (the route returns it with `"sent": false`).
- S8: the onboarding interview (the teammate's `inbound.py`).
- The call card in the page: no voice report shows it; the page sends no `purposes`.
- A voicemail policy: no message is left; the owner's to decide.
- `Say::Instruct` on the wire: not in the voice commit (glue review: "still sends nothing"); wired in the commit after it (**mock**; reviewed: adopt, no defect found. The first real session must show that a direction lands before the model answers the same line, is followed and is never read out): see the `voice_purposes` tests named `…instruct…`. Not fixed: the opening direction asks the bank's question, not the request's.

## 8. The owner's rules, quoted

- "No calls yet. The first test goes to my phone only after I explicitly say go."
- "Keep approvals enforced by our server. The assistant must request approval before commitments or sensitive disclosures, and only say the client has joined after the backend confirms it."
- "The assistant should identify itself as an AI assistant calling on the client's behalf."
- "we can use our own phone number": read to him as the account's one number as caller ID, nothing changed in the Telnyx account.
- "Maintain separate credentials for Second Site and Amyna. Never overwrite Amyna's existing configuration."
