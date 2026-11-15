//! Port of `apps/api/app/services/call_orchestration/simulator.py`: plays the carrier's role in
//! fixture mode by feeding Telnyx-shaped events into the same
//! [`CallOrchestrator::handle_event`] that real webhooks use. Every simulated call is labelled
//! `simulated = true`, its evidence is `FIXTURE_SYNTHETIC` with a "SIMULATED CALL" limitation,
//! and the owner summary says so. Refused (409 `SIMULATOR_DISABLED`) when the voice provider is
//! live, as in Python.
//!
//! One addition: the scripted loop stops after [`MAX_TURNS`] broker turns (Python's loop ends on
//! its own for every input; the bound only guards against a future change that would not).
//!
//! A second one: a request's own question that is not a bank question gets
//! [`NO_SCRIPTED_ANSWER`] (Python's simulated call never asks the request's questions).

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use ss_domain::contracts::{CallSession, CallState, HandoffStatus, TranscriptTurn};
use ss_domain::integrations::telnyx::{client_state, encode_client_state};
use uuid::Uuid;

use crate::app::App;
use crate::error::{AppError, AppResult};
use crate::services::call_orchestration::{is_request_question, CallOrchestrator};
use crate::store::comms::{CALLS, HANDOFFS};
use crate::store::Tx;

pub const DEFAULT_ANSWERS: [(&str, &str); 5] = [
    (
        "tenant_subpanel_amps",
        "The suite has its own 200 amp panel, I'd guess about a third of it is in use.",
    ),
    (
        "in_unit_plumbing",
        "Yes, there's a sink in the back room and a restroom inside the suite.",
    ),
    (
        "hvac_zonable",
        "Yes, the landlord's mechanical contractor said it can be split into zones.",
    ),
    ("usable_area_sqft", "It's about 1,250 usable square feet."),
    ("available_from", "It's available December first."),
];
pub const ESCALATION_LINE: &str = "Before we go further, the landlord will want first month and a deposit by Friday. Can you commit to that?";
pub const UNSURE: &str = "I'm not sure, I'd have to check.";
pub const OWNER_LINE: &str = "Hi, it's the owner. Let's talk deposit terms directly.";
/// What the simulated contact says to a request's own question that is not a bank question:
/// the script knows no answer, and none is made up. The question is recorded as unanswered.
pub const NO_SCRIPTED_ANSWER: &str = "I can't answer that one right now.";
/// The bound on a simulated call: broker turns (answers and the escalation line together).
/// Questions past it are not asked; the call's summary names the request's own among them.
pub const MAX_TURNS: usize = 200;

/// `_event(event_type, payload)`.
pub fn event(event_type: &str, payload: Map<String, Value>) -> Value {
    json!({
        "data": {
            "record_type": "event",
            "event_type": event_type,
            "id": Uuid::new_v4().to_string(),
            "payload": Value::Object(payload),
        }
    })
}

/// `CallSimulator`.
pub struct CallSimulator<'a> {
    pub orch: CallOrchestrator<'a>,
}

impl<'a> CallSimulator<'a> {
    /// Refuses with the live voice provider.
    pub fn new(app: &'a App) -> AppResult<Self> {
        if app.voice().mode() == "live" {
            return Err(AppError::conflict(
                "the simulator only runs with the fixture voice provider",
            )
            .code("SIMULATOR_DISABLED"));
        }
        Ok(Self {
            orch: CallOrchestrator::new(app),
        })
    }

    /// `_leg_events`: counterparty-leg events (with the call's `client_state` unless given).
    pub async fn leg_events(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        events: Vec<(&str, Map<String, Value>)>,
    ) -> AppResult<CallSession> {
        let mut call = call;
        for (et, extra) in events {
            let mut payload = Map::new();
            payload.insert(
                "call_control_id".into(),
                call.provider_call_id
                    .clone()
                    .map_or(Value::Null, Value::String),
            );
            let has_state = extra.contains_key("client_state");
            for (k, v) in extra {
                payload.insert(k, v);
            }
            if !has_state {
                payload.insert(
                    "client_state".into(),
                    Value::String(encode_client_state(&client_state(
                        &call.call_id.to_string(),
                        "counterparty",
                    ))),
                );
            }
            self.orch.handle_event(tx, &event(et, payload)).await?;
            call = CALLS.require(tx, call.call_id).await?;
        }
        Ok(call)
    }

    /// `run(call_id, owner_action, answers, escalate_after=1)`. `owner_action`: `accept` (the
    /// owner joins), `later` (the owner defers) or anything else (no response: the handoff
    /// expires).
    pub async fn run(
        &mut self,
        tx: &mut dyn Tx,
        call_id: Uuid,
        owner_action: &str,
        answers: &IndexMap<String, String>,
        escalate_after: usize,
    ) -> AppResult<CallSession> {
        self.run_bounded(
            tx,
            call_id,
            owner_action,
            answers,
            escalate_after,
            MAX_TURNS,
        )
        .await
    }

    /// [`Self::run`] with its bound on broker turns given (`run` uses [`MAX_TURNS`]).
    pub async fn run_bounded(
        &mut self,
        tx: &mut dyn Tx,
        call_id: Uuid,
        owner_action: &str,
        answers: &IndexMap<String, String>,
        escalate_after: usize,
        max_turns: usize,
    ) -> AppResult<CallSession> {
        let call = CALLS.require(tx, call_id).await?;
        if !call.simulated {
            return Err(
                AppError::conflict("this call was not created as a simulated call")
                    .code("NOT_SIMULATED"),
            );
        }
        let mut merged: IndexMap<String, String> = DEFAULT_ANSWERS
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        for (k, v) in answers {
            merged.insert(k.clone(), v.clone());
        }
        let mut status = Map::new();
        status.insert("status".into(), Value::from("completed"));
        let mut call = self
            .leg_events(
                tx,
                call,
                vec![("call.answered", Map::new()), ("call.speak.ended", status)],
            )
            .await?;
        let mut asked = 0usize;
        let mut turns = 0usize;
        while matches!(call.state, CallState::Active | CallState::AiResumed)
            && call.question_index < call.script.len() as i64
            && turns < max_turns
        {
            turns += 1;
            if asked == escalate_after && call.handoff_request_id.is_none() {
                call = self.say_broker(tx, call, ESCALATION_LINE).await?;
                call = self.owner_turn(tx, call, owner_action).await?;
                continue;
            }
            let idx = usize::try_from(call.question_index).unwrap_or(0);
            let question = call.script.get(idx);
            let claim = question.map(|q| q.claim_type.clone()).unwrap_or_default();
            // A request's own question has no scripted answer, and a caller's `answers` cannot
            // give it one: the contact says the fixed line.
            let answer = if question.is_some_and(is_request_question) {
                NO_SCRIPTED_ANSWER.to_owned()
            } else {
                merged
                    .get(&claim)
                    .cloned()
                    .unwrap_or_else(|| UNSURE.to_owned())
            };
            call = self.say_broker(tx, call, &answer).await?;
            asked += 1;
        }
        let mut hangup = Map::new();
        hangup.insert("hangup_cause".into(), Value::from("normal_clearing"));
        hangup.insert("hangup_source".into(), Value::from("callee"));
        self.leg_events(tx, call, vec![("call.hangup", hangup)])
            .await
    }

    /// `_say_broker`: one final transcription turn of the broker.
    pub async fn say_broker(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        text: &str,
    ) -> AppResult<CallSession> {
        let mut p = Map::new();
        p.insert(
            "transcription_data".into(),
            json!({"transcript": text, "is_final": true, "confidence": 0.95}),
        );
        self.leg_events(tx, call, vec![("call.transcription", p)])
            .await
    }

    async fn owner_turn(
        &mut self,
        tx: &mut dyn Tx,
        call: CallSession,
        owner_action: &str,
    ) -> AppResult<CallSession> {
        let Some(hid) = call.handoff_request_id else {
            return Ok(call);
        };
        if call.state != CallState::HandoffPending {
            return Ok(call);
        }
        if owner_action == "accept" {
            let call = self
                .orch
                .owner_response(tx, hid, call.owner_id, HandoffStatus::Accepted)
                .await?;
            let owner_leg = call
                .participant_states
                .iter()
                .find(|s| s.role == "owner")
                .and_then(|s| s.leg_id.clone());
            if let (CallState::OwnerJoining, Some(owner_leg)) = (call.state, owner_leg) {
                let state = encode_client_state(&client_state(&call.call_id.to_string(), "owner"));
                for et in ["call.answered", "conference.participant.joined"] {
                    let mut p = Map::new();
                    p.insert("call_control_id".into(), Value::from(owner_leg.clone()));
                    p.insert("client_state".into(), Value::from(state.clone()));
                    p.insert(
                        "conference_id".into(),
                        call.conference_id
                            .clone()
                            .map_or(Value::Null, Value::String),
                    );
                    self.orch.handle_event(tx, &event(et, p)).await?;
                }
                let mut call = CALLS.require(tx, call.call_id).await?;
                call.transcript.push(TranscriptTurn {
                    speaker: "owner".into(),
                    text: OWNER_LINE.into(),
                    at: ss_core::UtcDateTime::now(),
                });
                CALLS.upsert(tx, &call).await?;
                let mut p = Map::new();
                p.insert("call_control_id".into(), Value::from(owner_leg));
                p.insert("client_state".into(), Value::from(state));
                p.insert("hangup_cause".into(), Value::from("normal_clearing"));
                self.orch.handle_event(tx, &event("call.hangup", p)).await?;
                return CALLS.require(tx, call.call_id).await;
            }
            return Ok(call);
        }
        if owner_action == "later" {
            return self
                .orch
                .owner_response(tx, hid, call.owner_id, HandoffStatus::Deferred)
                .await;
        }
        let mut h = HANDOFFS.require(tx, hid).await?;
        h.expires_at =
            ss_core::UtcDateTime::from_datetime(chrono::Utc::now() - chrono::Duration::seconds(1));
        HANDOFFS.upsert(tx, &h).await?;
        self.orch.expire_stale_handoffs(tx, call.call_id).await
    }
}

// ---- Fixture voice bridge ---------------------------------------------------------------------

/// A bridge launcher for tests and simulations: it records every control the orchestrator
/// sends (`say:<text>`, `instruct:<text>`, `pause`, `resume`, `stop`) and keeps each call's spec, so a script can
/// drive the spec's [`crate::services::voice_bridge::BridgeEvents`] exactly as a live bridge
/// would (counterparty lines, tool calls, the end). Nothing reaches a carrier or xAI.
#[derive(Clone, Default)]
pub struct FixtureBridge {
    state: std::sync::Arc<std::sync::Mutex<FixtureBridgeState>>,
}

#[derive(Default)]
struct FixtureBridgeState {
    specs: Vec<(String, crate::services::voice_bridge::CallBridgeSpec)>,
    log: Vec<String>,
    removed: Vec<String>,
    refuse_instruct: bool,
}

impl FixtureBridge {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FixtureBridgeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The controls received, in order.
    pub fn log(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    /// The bridges registered: `(bridge_id, spec)`.
    pub fn specs(&self) -> Vec<(String, crate::services::voice_bridge::CallBridgeSpec)> {
        self.lock().specs.clone()
    }

    /// The directions received (`instruct:` entries of the log, without the prefix), in order.
    pub fn instructions(&self) -> Vec<String> {
        self.lock()
            .log
            .iter()
            .filter_map(|l| l.strip_prefix("instruct:").map(str::to_owned))
            .collect()
    }

    /// From now on a direction is refused, as a bridge that has ended refuses it.
    pub fn refuse_instructions(&self, refuse: bool) {
        self.lock().refuse_instruct = refuse;
    }

    /// The bridge ids removed (the call ended or the bridge was let go).
    pub fn removed(&self) -> Vec<String> {
        self.lock().removed.clone()
    }
}

struct FixtureControl {
    bridge: FixtureBridge,
}

#[async_trait::async_trait]
impl crate::services::call_orchestration::BridgeControl for FixtureControl {
    async fn say(&self, text: String) -> bool {
        self.bridge.lock().log.push(format!("say:{text}"));
        true
    }
    async fn instruct(&self, text: String) -> bool {
        let mut b = self.bridge.lock();
        if b.refuse_instruct {
            return false;
        }
        b.log.push(format!("instruct:{text}"));
        true
    }
    async fn pause(&self) -> bool {
        self.bridge.lock().log.push("pause".into());
        true
    }
    async fn resume(&self) -> bool {
        self.bridge.lock().log.push("resume".into());
        true
    }
    async fn stop(&self) -> bool {
        self.bridge.lock().log.push("stop".into());
        true
    }
}

impl crate::services::call_orchestration::BridgeLauncher for FixtureBridge {
    fn expect(
        &self,
        spec: crate::services::voice_bridge::CallBridgeSpec,
    ) -> (
        String,
        std::sync::Arc<dyn crate::services::call_orchestration::BridgeControl>,
    ) {
        let id = format!("fixture-bridge-{}", Uuid::new_v4().simple());
        self.lock().specs.push((id.clone(), spec));
        (
            id,
            std::sync::Arc::new(FixtureControl {
                bridge: self.clone(),
            }),
        )
    }
    fn remove(&self, bridge_id: &str) {
        self.lock().removed.push(bridge_id.to_owned());
    }
}
