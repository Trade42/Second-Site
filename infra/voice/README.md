# Voice infrastructure (owner: communications)

Implemented with Telnyx speak + transcription turns (see services/call_orchestration); xAI realtime voice is the planned upgrade for step 3:
1. Telnyx Call Control dials the broker (`POST /v2/calls`) after an APPROVED `PLACE_CALL` approval.
2. Owner's genuine recorded intro is played (never synthesized), followed by the required automation disclosure.
3. The assistant leg is xAI realtime voice over SIP (`realtime.call.incoming` webhook ->
   `wss://api.x.ai/v1/realtime?call_id=...`).
4. Escalation -> `OwnerHandoffRequest` -> owner notified -> Telnyx conference (`POST /v2/conferences`,
   `/actions/join`) -> `HUMAN_JOINED`. No response -> `AI_RESUMED` and the question is deferred.

The WebSocket bridge needs a persistent process with a public HTTPS/WSS endpoint (see infra/oracle).
