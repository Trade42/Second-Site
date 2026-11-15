//! The post-call summary: provenance, every question's state, no commitments, empty calls, the
//! simulated label, the owner text's bound, redaction and determinism.

use chrono::{TimeZone, Utc};
use indexmap::IndexMap;
use serde_json::Value;
use ss_core::UtcDateTime;
use ss_domain::communications::call_script::{availability, extract, QUESTION_BANK};
use ss_domain::communications::call_summary::*;
use ss_domain::communications::response_agent::extract_findings;
use ss_domain::contracts::communications::{
    CallQuestion, CallSession, HandoffReason, HandoffStatus, OwnerHandoffRequest, TranscriptTurn,
};
use uuid::Uuid;

fn at(min: u32) -> UtcDateTime {
    UtcDateTime::from_datetime(Utc.with_ymd_and_hms(2026, 11, 15, 10, min, 0).unwrap())
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn bank(i: usize) -> CallQuestion {
    let (req, q, claim) = QUESTION_BANK[i];
    CallQuestion {
        question: q.into(),
        claim_type: claim.into(),
        requirement_id: Some(req.into()),
    }
}

fn turn(speaker: &str, text: &str, m: u32) -> TranscriptTurn {
    TranscriptTurn {
        speaker: speaker.into(),
        text: text.into(),
        at: at(m),
    }
}

/// A completed call: amps answered, plumbing answered "no" with a $40,000 retrofit, HVAC asked
/// and not clearly answered, availability not reached; a guarantor handoff that expired.
fn fixture_call(simulated: bool) -> (CallSession, Vec<OwnerHandoffRequest>) {
    let mut recipient = IndexMap::new();
    recipient.insert("name".to_owned(), "Dana Reyes".to_owned());
    recipient.insert("role".to_owned(), "property manager".to_owned());
    recipient.insert("phone".to_owned(), "+15550100".to_owned());
    let mut call = CallSession::new(id(1), id(2), recipient, id(3));
    call.call_id = id(4);
    call.property_id = Some(id(5));
    call.simulated = simulated;
    call.started_at = Some(at(0));
    call.ended_at = Some(at(9));
    let (amps, plumb, hvac, avail) = (bank(0), bank(1), bank(2), availability());
    call.script = vec![amps.clone(), plumb.clone(), hvac.clone(), avail];
    let a1 = "It's a 200 amp panel.";
    let a2 = "No plumbing in the suite; it needs a $40,000 plumbing retrofit.";
    let a3 = "Hmm, I'd have to check on that.";
    call.transcript = vec![
        turn("assistant", &amps.question, 1),
        turn("counterparty", a1, 2),
        turn("assistant", &plumb.question, 3),
        turn("counterparty", a2, 4),
        turn("assistant", &hvac.question, 5),
        turn("counterparty", a3, 6),
        turn(
            "counterparty",
            "Can you send us the tax returns? And my number for the guarantor form is 123-45-6789.",
            7,
        ),
        turn(
            "assistant",
            "That's a decision for the owner, and I can't agree to anything on their behalf.",
            8,
        ),
    ];
    call.findings = vec![extract(&amps, a1), extract(&plumb, a2), extract(&hvac, a3)];
    call.deferred_questions = vec!["Will the owner sign a personal guarantee?".into()];
    let h = OwnerHandoffRequest {
        handoff_request_id: id(6),
        call_id: id(4),
        project_id: id(1),
        reason: HandoffReason::GuarantorRequested,
        question: "Will you sign a personal guarantee?".into(),
        urgency: "HIGH".into(),
        owner_id: id(2),
        join_token_reference: None,
        expires_at: at(12),
        status: HandoffStatus::Expired,
    };
    (call, vec![h])
}

/// The fixture plus the Response Agent's money finding for the retrofit (the newer shape).
fn fixture_input(simulated: bool) -> SummaryInput {
    let (call, handoffs) = fixture_call(simulated);
    let mut input = SummaryInput::from_call(&call, CallEnd::Completed, handoffs, Some(450_000));
    let money = extract_findings(None, "it needs a $40,000 plumbing retrofit.");
    input.findings.extend(
        money
            .iter()
            .filter(|f| f.amount_cents.is_some())
            .map(InputFinding::from),
    );
    input
}

/// Every JSON object that carries `statement` or `state` (an item) also carries `said_by` and
/// `kind`; returns how many items were checked.
fn walk(v: &Value, path: &str, n: &mut usize) {
    match v {
        Value::Object(m) => {
            if m.contains_key("statement") || m.contains_key("state") || m.contains_key("outcome") {
                assert!(
                    m.get("said_by").is_some_and(|s| s.is_string()),
                    "{path}: no said_by"
                );
                assert!(
                    m.get("kind").is_some_and(|s| s.is_string()),
                    "{path}: no kind"
                );
                *n += 1;
            }
            for (k, x) in m {
                walk(x, &format!("{path}.{k}"), n);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                walk(x, &format!("{path}[{i}]"), n);
            }
        }
        _ => {}
    }
}

#[test]
fn golden_summary_from_fixture_call() {
    let s = summarize(&fixture_input(false));
    let text = owner_text(&s);
    println!("{}", serde_json::to_string_pretty(&s).unwrap());
    println!("TEXT ({} chars): {text}", text.chars().count());
    assert!(!s.nothing_learned);
    assert_eq!(s.verification_note, NOTHING_VERIFIED);
    // Reported facts: amps and plumbing in infrastructure, the retrofit money there too.
    let infra: Vec<_> = s
        .infrastructure
        .reported_by_counterparty
        .iter()
        .map(|i| i.claim_type.clone().unwrap())
        .collect();
    assert_eq!(
        infra,
        [
            "tenant_subpanel_amps",
            "in_unit_plumbing",
            "improvement_cost"
        ]
    );
    assert_eq!(
        s.infrastructure.reported_by_counterparty[0].value,
        Value::from(200)
    );
    // Approvals requested: the expired handoff, as a system record.
    assert_eq!(s.approvals_requested.len(), 1);
    assert_eq!(s.approvals_requested[0].status, HandoffStatus::Expired);
    assert_eq!(s.approvals_requested[0].kind, StatementKind::SystemRecord);
    // The document request is quoted, and redacted.
    assert_eq!(s.requested_documents.len(), 1);
    assert!(s.requested_documents[0].statement.contains("tax returns"));
    assert_eq!(
        text,
        "Needs your decision: Will you sign a personal guarantee? Dana Reyes (property manager) \
         said: It's a 200 amp panel; No plumbing in the suite; it needs a $40,000 plumbing \
         retrofit. Not verified. Derived concern: the \
         reported plumbing retrofit of $40,000 is a cost the tenant may carry; it is unverified \
         and may change the property's ranking. Derived concern: the contact reported no in-suite \
         plumbing; the build-out may need that work. 2 of 4 questions still open."
    );
}

#[test]
fn forty_thousand_dollar_retrofit_is_reported_in_cents_and_a_concern_only_when_derived() {
    let s = summarize(&fixture_input(false));
    let r = s
        .infrastructure
        .reported_by_counterparty
        .iter()
        .find(|i| i.amount_cents.is_some())
        .expect("the retrofit");
    assert_eq!(r.amount_cents, Some(4_000_000));
    assert_eq!(r.kind, StatementKind::ReportedByCounterparty);
    assert_eq!(r.said_by, SaidBy::Counterparty);
    assert!(r.needs_verification);
    assert_eq!(r.period, Some(Period::OneTime));
    // The concern is ours, labelled derived, with what it came from.
    assert!(s.concerns.reported_by_counterparty.is_empty());
    let c = s
        .concerns
        .inferred_by_us
        .iter()
        .find(|c| c.statement.contains("$40,000"))
        .expect("a derived concern");
    assert_eq!(c.kind, StatementKind::InferredByUs);
    assert!(c.statement.starts_with("Derived concern:"));
    assert_eq!(c.derived_from, ["improvement_cost"]);
    let total = &s.costs.inferred_by_us[0];
    assert_eq!(total.amount_cents, Some(4_000_000));
    assert!(total.statement.starts_with("Derived:"));
    // A call verifies nothing.
    for sec in [&s.listing, &s.costs, &s.infrastructure, &s.concerns] {
        assert!(sec.verified.is_empty());
    }
}

#[test]
fn every_outreach_question_appears_with_its_state() {
    let s = summarize(&fixture_input(false));
    let states: Vec<(String, AnswerState)> = s
        .answers
        .iter()
        .map(|a| (a.question_id.clone(), a.state))
        .collect();
    assert_eq!(
        states,
        [
            (
                "bank:tenant_subpanel_amps".to_owned(),
                AnswerState::Answered
            ),
            ("bank:in_unit_plumbing".to_owned(), AnswerState::Answered),
            ("bank:hvac_zonable".to_owned(), AnswerState::NotAnswered),
            ("bank:available_from".to_owned(), AnswerState::NotReached),
        ]
    );
    // An unanswered question is never given a value.
    let hvac = &s.answers[2];
    assert_eq!(
        hvac.answer.as_deref(),
        Some("Hmm, I'd have to check on that.")
    );
    assert!(s
        .listing
        .reported_by_counterparty
        .iter()
        .all(|i| i.claim_type.as_deref() != Some("hvac_zonable")));
    // A declined answer.
    let mut input = fixture_input(false);
    input.findings[2].reported = Some("I'd rather not say until we meet.".into());
    let s = summarize(&input);
    assert_eq!(s.answers[2].state, AnswerState::Declined);
    // Open questions are listed, plus the owner-only question they asked.
    assert_eq!(s.remaining_questions.len(), 3);
}

#[test]
fn no_item_without_provenance() {
    let s = summarize(&fixture_input(true));
    let v = serde_json::to_value(&s).unwrap();
    let mut n = 0;
    walk(&v, "$", &mut n);
    assert!(n >= 15, "only {n} items walked");
}

#[test]
fn commitments_made_is_always_empty() {
    let mut input = fixture_input(false);
    input.transcript.push(turn(
        "assistant",
        "Great, we'll take it and the owner will sign the lease.",
        9,
    ));
    input
        .transcript
        .push(turn("counterparty", "Can you sign the lease today?", 9));
    let s = summarize(&input);
    assert!(s.commitments_made.is_empty());
    let att: Vec<_> = s
        .needs_owner_attention
        .iter()
        .map(|a| (a.said_by, a.kind))
        .collect();
    assert_eq!(
        att,
        [
            (SaidBy::Assistant, StatementKind::TranscriptQuote),
            (SaidBy::Counterparty, StatementKind::TranscriptQuote),
        ]
    );
    assert!(owner_text(&s).starts_with("Needs your decision:"));
    // The assistant's own refusal ("I can't agree") is not a commitment look-alike.
    assert!(summarize(&fixture_input(false))
        .needs_owner_attention
        .is_empty());
}

#[test]
fn an_empty_call_says_nothing_was_learned() {
    for end in [CallEnd::NoAnswer, CallEnd::Voicemail] {
        let (mut call, _) = fixture_call(false);
        call.transcript.clear();
        call.findings.clear();
        call.deferred_questions.clear();
        let s = summarize(&SummaryInput::from_call(&call, end, vec![], None));
        assert!(s.nothing_learned);
        assert!(s.answers.iter().all(|a| a.state == AnswerState::NotReached));
        assert_eq!(s.answers.len(), 4);
        for sec in [&s.listing, &s.costs, &s.infrastructure, &s.concerns] {
            assert!(sec.reported_by_counterparty.is_empty() && sec.inferred_by_us.is_empty());
        }
        let text = owner_text(&s);
        assert!(text.contains("nothing was learned"), "{text}");
        assert!(!text.contains("said:"), "{text}");
    }
}

#[test]
fn simulated_text_begins_with_the_label() {
    let text = owner_text(&summarize(&fixture_input(true)));
    assert!(text.starts_with(SIMULATED_LABEL), "{text}");
    assert_eq!(SIMULATED_LABEL, "[Simulated call] ");
    assert!(!owner_text(&summarize(&fixture_input(false))).starts_with(SIMULATED_LABEL));
}

#[test]
fn owner_text_is_bounded() {
    let mut input = fixture_input(true);
    for i in 0..60 {
        let q = CallQuestion {
            question: format!("Question number {i} about the space?"),
            claim_type: format!("extra_{i}"),
            requirement_id: None,
        };
        input.findings.push(InputFinding {
            question: Some(q.question.clone()),
            claim_type: q.claim_type.clone(),
            topic: "general".into(),
            reported: Some(format!(
                "A long answer number {i} with plenty of words in it."
            )),
            value: Value::from(i),
            unit: None,
            amount_cents: None,
            period: None,
            purpose: None,
        });
    }
    let text = owner_text(&summarize(&input));
    assert!(
        text.chars().count() <= MAX_OWNER_TEXT_CHARS,
        "{}",
        text.chars().count()
    );
    assert!(text.starts_with(SIMULATED_LABEL));
    assert!(text.ends_with('…'));
}

#[test]
fn a_sensitive_value_is_absent() {
    let mut input = fixture_input(false);
    input.findings[0].reported = Some("My card number is 4111 1111 1111 1111, 200 amps.".into());
    input.handoffs[0].question = "They want SSN 123-45-6789 for the guarantor form.".into();
    let s = summarize(&input);
    let json = serde_json::to_string(&s).unwrap();
    let text = owner_text(&s);
    for needle in ["123-45-6789", "6789", "4111 1111", "1111 1111 1111"] {
        assert!(!json.contains(needle), "json holds {needle}");
        assert!(!text.contains(needle), "text holds {needle}: {text}");
    }
    assert!(json.contains("[REDACTED]"));
}

#[test]
fn deterministic() {
    let a = serde_json::to_string(&summarize(&fixture_input(true))).unwrap();
    let b = serde_json::to_string(&summarize(&fixture_input(true))).unwrap();
    assert_eq!(a, b);
    assert_eq!(
        owner_text(&summarize(&fixture_input(true))),
        owner_text(&summarize(&fixture_input(true)))
    );
}
