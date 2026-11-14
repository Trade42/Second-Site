//! Communications (part 4): the database-free Python tests that cover the ported functions,
//! assertion for assertion, plus schema conformance of every public communications contract.
//!
//! Ported:
//! - `tests/test_contracts.py::test_call_state_machine`;
//! - `tests/test_comms_flow.py::test_call_script_and_extraction`;
//! - `tests/test_approvals_comms.py::test_escalation_policy`;
//! - the `decode_client_state` round trip of `tests/test_comms_flow.py::test_photon_and_telnyx_request_shapes`
//!   (the rest of that test drives the live HTTP providers, which are the server's).

mod common;

use indexmap::IndexMap;
use serde_json::{json, Value};
use ss_core::{Contract, UtcDateTime};
use ss_domain::communications::call_script::{availability, build_script, describe, extract};
use ss_domain::communications::{call_payload, detect_escalation};
use ss_domain::contracts::communications::{
    AuthorityScope, CallQuestion, CallSession, CallState, Channel, CommunicationSession,
    HandoffReason, HandoffStatus, OutreachRequest, OwnerChannel, OwnerHandoffRequest,
    ParticipantState, StructuredFinding, TranscriptTurn,
};
use ss_domain::contracts::infrastructure::{AssessmentStatus, InfrastructureAssessment};
use ss_domain::integrations::telnyx::{
    client_state, decode_client_state, decode_client_state_json, encode_client_state,
    FixtureTelephonyProvider, TelephonyCommand, TelephonyError, TelephonyProvider,
};
use ss_domain::integrations::xai_voice::{build_session_update, status as xai_status};
use uuid::Uuid;

fn u() -> Uuid {
    Uuid::new_v4()
}

fn phone(p: &str) -> IndexMap<String, String> {
    let mut m = IndexMap::new();
    m.insert("phone".to_owned(), p.to_owned());
    m
}

#[test]
fn test_call_state_machine() {
    let mut c = CallSession::new(u(), u(), phone("+15555550100"), u());
    for s in [
        CallState::Dialing,
        CallState::Intro,
        CallState::Active,
        CallState::HandoffPending,
        CallState::OwnerJoining,
        CallState::HumanJoined,
        CallState::AiResumed,
        CallState::Completed,
    ] {
        c = c.transition(s).expect("allowed");
    }
    let err = c.transition(CallState::Active).unwrap_err();
    assert_eq!(
        err.to_string(),
        "call: transition COMPLETED -> ACTIVE is not allowed"
    );
    assert!(CallSession::new(u(), u(), IndexMap::new(), u())
        .transition(CallState::HumanJoined)
        .is_err());
}

#[test]
fn handoff_state_machine_and_copy_semantics() {
    let h = OwnerHandoffRequest::from_json_value(json!({
        "call_id": u(), "project_id": u(), "reason": "FINANCIAL_DETAILS",
        "question": "deposit?", "owner_id": u(), "expires_at": "2026-11-14T12:00:00Z",
    }))
    .unwrap();
    assert_eq!(h.status, HandoffStatus::Pending);
    assert_eq!(h.urgency, "HIGH");
    let n = h.transition(HandoffStatus::Notified).unwrap();
    assert_eq!(
        h.status,
        HandoffStatus::Pending,
        "transition returns a copy"
    );
    assert_eq!(
        n.transition(HandoffStatus::Accepted).unwrap().status,
        HandoffStatus::Accepted
    );
    assert_eq!(
        n.transition(HandoffStatus::Pending)
            .unwrap_err()
            .to_string(),
        "handoff: transition NOTIFIED -> PENDING is not allowed"
    );
    assert!(h.transition(HandoffStatus::Accepted).is_err());
}

fn assessment(rid: &str, st: AssessmentStatus) -> InfrastructureAssessment {
    InfrastructureAssessment::from_json_value(json!({
        "project_id": Uuid::from_u128(1),
        "property_id": Uuid::from_u128(2),
        "requirement_id": rid,
        "status": st,
        "uncertainty": "x",
        "verification_questions": ["?"],
    }))
    .unwrap()
}

#[test]
fn test_call_script_and_extraction() {
    let script = build_script(&[
        assessment("fs.electrical.tenant_service", AssessmentStatus::Unknown),
        assessment("fs.space.min_area", AssessmentStatus::Fails),
    ]);
    assert_eq!(
        script
            .iter()
            .map(|q| q.claim_type.as_str())
            .collect::<Vec<_>>(),
        ["tenant_subpanel_amps", "available_from"]
    );
    assert!(script[0].requirement_id.is_some());
    let q = &script[0];
    assert_eq!(
        extract(q, "It's a 400 amp service, 3 phase").value,
        json!(400)
    );
    // Unsure -> no number.
    assert_eq!(
        extract(q, "Not sure, maybe 200 amps? I'd have to check").value,
        Value::Null
    );
    let plumb = build_script(&[assessment(
        "fs.plumbing.treatment_sinks",
        AssessmentStatus::MayRequireWork,
    )])
    .remove(0);
    assert_eq!(
        extract(&plumb, "Yes, there's a sink in back").value,
        json!(true)
    );
    assert_eq!(extract(&plumb, "No, it's all open").value, json!(false));
    assert_eq!(
        extract(&availability(), "January 1st").value,
        json!("January 1st")
    );
}

#[test]
fn findings_stay_owner_reported_and_describe_like_python() {
    let q = CallQuestion {
        question: "How many amps?".into(),
        claim_type: "tenant_subpanel_amps".into(),
        requirement_id: None,
    };
    let f = extract(&q, "about 200A");
    assert_eq!(f.classification, "OWNER_REPORTED");
    assert_eq!(f.speaker.as_deref(), Some("counterparty"));
    assert_eq!(f.answer.as_deref(), Some("about 200A"));
    assert_eq!(f.unit.as_deref(), Some("A"));
    assert_eq!(describe(&f), "tenant_subpanel_amps: 200 A");
    assert_eq!(
        describe(&extract(&q, "no idea")),
        "tenant_subpanel_amps: unclear"
    );
    let mut b = extract(&q, "x");
    b.value = json!(false);
    assert_eq!(describe(&b), "tenant_subpanel_amps: no");
    b.value = json!(2.5);
    b.unit = Some(String::new());
    assert_eq!(describe(&b), "tenant_subpanel_amps: 2.5");
    b.claim_type = None;
    b.value = json!([1, "a"]);
    assert_eq!(describe(&b), "None: [1, 'a']");
}

#[test]
fn test_escalation_policy() {
    assert_eq!(
        detect_escalation("Will you need a guarantor?"),
        Some(HandoffReason::GuarantorRequested)
    );
    assert_eq!(
        detect_escalation("We need first month and deposit by Friday"),
        Some(HandoffReason::FinancialDetails)
    );
    assert_eq!(
        detect_escalation("Can I talk to the owner?"),
        Some(HandoffReason::CounterpartyRequestedOwner)
    );
    assert_eq!(detect_escalation("Is there a sink in the back room?"), None);
}

#[test]
fn client_state_round_trip_and_malformed_input() {
    // test_photon_and_telnyx_request_shapes: decode_client_state(dial["client_state"]).
    let state = client_state("c1", "counterparty");
    let enc = encode_client_state(&state);
    assert_eq!(enc, "eyJjYWxsX2lkIjoiYzEiLCJyb2xlIjoiY291bnRlcnBhcnR5In0=");
    assert_eq!(decode_client_state(Some(&enc)), state);
    assert!(decode_client_state(None).is_empty());
    assert!(decode_client_state(Some("")).is_empty());
    assert!(decode_client_state(Some("not base64 at all!")).is_empty());
    assert!(
        decode_client_state(Some("WzEsMl0=")).is_empty(),
        "a JSON list is not a state"
    );
    assert!(decode_client_state(Some("é")).is_empty());
    assert!(decode_client_state_json(Some(&json!(0)))
        .unwrap()
        .is_empty());
    assert!(decode_client_state_json(Some(&json!(12))).is_err());
    assert_eq!(decode_client_state_json(Some(&json!(enc))).unwrap(), state);
}

#[test]
fn fixture_telephony_records_and_never_claims_a_real_call() {
    let p = FixtureTelephonyProvider::new();
    assert_eq!(p.mode(), "fixture");
    let st = p.status();
    assert_eq!(common::to_value(&st)["status"], json!("MOCK"));
    assert_eq!(
        common::to_value(&st)["note"],
        json!("Simulated calls; no PSTN traffic.")
    );
    let cs = client_state("c1", "counterparty");
    let d = p
        .dial(
            "+15555550100",
            "+15555550111",
            "https://x/webhooks/telnyx",
            "idem",
            Some(&cs),
        )
        .unwrap();
    assert!(d.call_control_id.starts_with("fixture-call-"));
    assert_eq!(d.state, "initiated");
    let again = p.dial("+1999", "+1", "u", "idem", None).unwrap();
    assert_eq!(
        again.call_control_id, d.call_control_id,
        "idempotent on the key"
    );
    assert_eq!(p.calls().len(), 1);
    p.set_fail_next_dial(true);
    match p.dial("+1", "+1", "u", "other", None) {
        Err(TelephonyError::Provider(e)) => {
            assert_eq!(e.message, "simulated dial failure");
            assert!(e.retryable());
        }
        other => panic!("{other:?}"),
    }
    assert!(p.dial("+1", "+1", "u", "other", None).is_ok(), "fails once");
    p.speak(&d.call_control_id, "hello").unwrap();
    p.play(&d.call_control_id, "https://x/a.mp3").unwrap();
    p.start_transcription(&d.call_control_id).unwrap();
    let conf = p.create_conference(&d.call_control_id, "call-1").unwrap();
    assert!(conf.starts_with("fixture-conf-"));
    p.join_conference(&conf, "v3:owner").unwrap();
    p.hangup(&d.call_control_id).unwrap();
    let cmds: Vec<&str> = p
        .commands()
        .iter()
        .map(TelephonyCommand::cmd)
        .collect::<Vec<_>>();
    assert_eq!(
        cmds,
        ["speak", "playback_start", "transcription_start", "hangup"]
    );
    assert_eq!(
        p.commands()[0].to_json(),
        json!({"cmd": "speak", "leg": d.call_control_id, "text": "hello"})
    );
    assert_eq!(
        p.conferences()[&conf],
        vec![d.call_control_id.clone(), "v3:owner".into()]
    );
}

#[test]
fn session_update_and_voice_status() {
    let call = CallSession::new(u(), u(), phone("+1"), u());
    let su = build_session_update(&call, "6922 W Hollywood Blvd #302", &["Q1?", "Q2?"]);
    assert_eq!(su["type"], "session.update");
    let instr = su["session"]["instructions"].as_str().unwrap();
    assert!(instr.starts_with(
        "This call is being handled by an automated assistant on the owner's behalf. You are the business owner's assistant."
    ));
    assert!(instr.ends_with("Property: 6922 W Hollywood Blvd #302. Questions: Q1? | Q2?"));
    let st = common::to_value(&xai_status(false));
    assert_eq!(st["status"], "NOT_STARTED");
    assert_eq!(st["mode"], "fixture");
}

#[test]
fn call_payload_shape() {
    let pid = Uuid::from_u128(7);
    let mut r = IndexMap::new();
    r.insert("role".to_owned(), "broker".to_owned());
    r.insert("name".to_owned(), "B".to_owned());
    r.insert("phone".to_owned(), "+15555550100".to_owned());
    let p = call_payload(pid, &r, None);
    assert_eq!(
        Value::Object(p),
        json!({"project_id": "00000000-0000-0000-0000-000000000007", "property_id": null,
               "recipient": {"role": "broker", "name": "B", "phone": "+15555550100"}})
    );
}

#[test]
fn communications_contracts_conform_to_their_schemas() {
    let now = UtcDateTime::parse("2026-11-14T12:00:00Z").unwrap();
    let q = build_script(&[assessment(
        "fs.electrical.tenant_service",
        AssessmentStatus::Unknown,
    )]);
    let finding = extract(&q[0], "400 amps");
    let mut call = CallSession::new(u(), u(), phone("+15555550100"), u());
    call.script = q.clone();
    call.findings = vec![finding.clone()];
    call.participant_states = vec![ParticipantState {
        role: "assistant".into(),
        leg_id: Some("v3:abc".into()),
        state: "joined".into(),
        updated_at: now,
    }];
    call.transcript = vec![TranscriptTurn {
        speaker: "counterparty".into(),
        text: "400 amps".into(),
        at: now,
    }];
    call.started_at = Some(now);
    let call = call.transition(CallState::Dialing).unwrap();
    common::assert_conforms("CallSession", &common::to_value(&call));
    common::assert_conforms("CallQuestion", &common::to_value(&q[0]));
    common::assert_conforms("StructuredFinding", &common::to_value(&finding));
    common::assert_conforms(
        "AuthorityScope",
        &common::to_value(&AuthorityScope::default()),
    );
    common::assert_conforms(
        "ParticipantState",
        &common::to_value(&call.participant_states[0]),
    );
    common::assert_conforms("TranscriptTurn", &common::to_value(&call.transcript[0]));
    let comm = CommunicationSession::from_json_value(json!({
        "project_id": u(), "channel": "VOICE", "authorized_owner_id": u(), "purpose": "Ask",
        "external_participants": [{"role": "broker", "name": "B", "handle": "+1"}],
        "structured_findings": [common::to_value(&finding)],
    }))
    .unwrap();
    assert_eq!(comm.channel, Channel::Voice);
    common::assert_conforms("CommunicationSession", &common::to_value(&comm));
    let h = OwnerHandoffRequest::from_json_value(json!({
        "call_id": call.call_id, "project_id": call.project_id, "reason": "GUARANTOR_REQUESTED",
        "question": "guarantor?", "owner_id": call.owner_id, "expires_at": "2026-11-14T12:05:00Z",
    }))
    .unwrap();
    common::assert_conforms("OwnerHandoffRequest", &common::to_value(&h));
    let o = OutreachRequest::from_json_value(json!({
        "property_id": u(), "channel": "VOICE", "purpose": "Verify electrical",
        "recipient": {"role": "broker", "name": "Dana Broker", "phone": "+15555550100"},
    }))
    .unwrap();
    common::assert_conforms("OutreachRequest", &common::to_value(&o));
    let ch =
        OwnerChannel::from_json_value(json!({"owner_id": u(), "handle": "+15555550142"})).unwrap();
    assert_eq!(ch.platform, "imessage");
    common::assert_conforms("OwnerChannel", &common::to_value(&ch));
    // The schemas have teeth.
    let mut bad = common::to_value(&call);
    bad["state"] = json!("RINGING");
    common::assert_rejected_by_schema("CallSession", &bad);
    let mut bad = common::to_value(&finding);
    bad["extra"] = json!(1);
    common::assert_rejected_by_schema("StructuredFinding", &bad);
    // Unknown fields are refused like Pydantic's extra="forbid".
    let err =
        StructuredFinding::from_json_value(json!({"question": "q", "confidence": 1})).unwrap_err();
    assert_eq!(err.errors[0].kind, "extra_forbidden");
    let err =
        CallSession::from_json_value(json!({"project_id": u(), "owner_id": u(), "recipient": {}}))
            .unwrap_err();
    assert_eq!(err.errors[0].kind, "missing");
}
