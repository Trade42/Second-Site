//! The Response Agent (`ss_domain::communications::response_agent`): a pure, deterministic
//! decision per counterparty turn.

use std::time::{Duration, Instant};

use ss_core::EvidenceClassification;
use ss_domain::communications::call_script::QUESTION_BANK;
use ss_domain::communications::response_agent::{
    cut, extract_findings, format_money, plan_questions, redact, resume, start, step,
    topic_of_question, AnswerKind, CallContext, ConversationState, Decision, EscalationRule,
    MoneyPeriod, OwnerRequirement, QuestionSource, Say, Speaker, Specialist, Utterance,
    CONCESSION_LINE, DONT_KNOW_LINE, HOLD_LINE, IDENTITY_LINE, MAX_ANALYSIS_CHARS,
    MAX_FINDINGS_PER_TURN, OWNER_REQUESTED_LINE, SENSITIVE_LINE,
};
use ss_domain::communications::{availability, detect_escalation};
use ss_domain::contracts::communications::{AuthorityScope, CallQuestion, HandoffReason};

fn bank() -> Vec<CallQuestion> {
    let mut v: Vec<CallQuestion> = QUESTION_BANK
        .iter()
        .map(|(suffix, q, claim)| CallQuestion {
            question: (*q).to_owned(),
            claim_type: (*claim).to_owned(),
            requirement_id: Some(format!("req.{suffix}")),
        })
        .collect();
    v.push(availability());
    v
}

fn ctx_with(request: &[&str]) -> CallContext {
    let req: Vec<String> = request.iter().map(|s| (*s).to_owned()).collect();
    CallContext {
        questions: plan_questions(&req, &bank()),
        owner_requirements: vec![OwnerRequirement {
            topic: "rent".into(),
            statement: format!(
                "the owner is looking at up to {} a month",
                format_money(450_000, "USD")
            ),
        }],
        ..CallContext::default()
    }
}

fn ctx() -> CallContext {
    ctx_with(&[])
}

fn cp(turn_id: u64, text: &str) -> Utterance {
    Utterance {
        turn_id,
        speaker: Speaker::Counterparty,
        text: text.to_owned(),
    }
}

/// Start the call, then feed one counterparty turn.
fn one(ctx: &CallContext, text: &str) -> (Decision, ConversationState) {
    let (_, s0) = start(ctx);
    step(ctx, &s0, &cp(1, text))
}

fn say_text(d: &Decision) -> &str {
    match &d.say {
        Say::Exact(t) | Say::Instruct(t) => t,
        Say::Silent => "",
    }
}

// ---- The specification's own example -------------------------------------------------------

#[test]
fn forty_thousand_dollar_plumbing_retrofit() {
    for phrase in [
        "Honestly there's no plumbing in there; it would need a forty thousand dollar plumbing retrofit.",
        "There's no plumbing in the suite, it needs a $40,000 plumbing retrofit.",
    ] {
        let c = ctx();
        let (_, s0) = start(&c);
        // Ask the plumbing question first: answer the amps one.
        let (d1, s1) = step(&c, &s0, &cp(1, "About 200 amps."));
        assert_eq!(d1.next_question.as_ref().unwrap().claim_type, "in_unit_plumbing");
        let (d, s2) = step(&c, &s1, &cp(2, phrase));
        let money: Vec<_> = d.findings.iter().filter(|f| f.amount_cents.is_some()).collect();
        assert_eq!(money.len(), 1, "{phrase}: {:?}", d.findings);
        let m = money[0];
        assert_eq!(m.amount_cents, Some(4_000_000));
        assert_eq!(m.claim_type, "improvement_cost");
        assert_eq!(m.purpose.as_deref(), Some("plumbing retrofit"));
        assert_eq!(m.reported_by, "counterparty");
        assert_eq!(m.classification, EvidenceClassification::OwnerReported);
        assert!(m.needs_verification);
        assert!(m.specialists.contains(&Specialist::Finance));
        assert!(m.specialists.contains(&Specialist::Feasibility));
        assert!(d.specialists.contains(&Specialist::Finance) && d.specialists.contains(&Specialist::Feasibility));
        // The plumbing answer itself, through the existing `extract`.
        let ans = d.findings.iter().find(|f| f.question_id.is_some()).unwrap();
        assert_eq!(ans.claim_type, "in_unit_plumbing");
        assert_eq!(ans.answer_kind, Some(AnswerKind::No));
        // The next question is still asked; no escalation.
        assert!(d.escalation.is_none());
        assert_eq!(d.answered_question.as_deref(), Some("bank:in_unit_plumbing"));
        let next = d.next_question.as_ref().expect("next question");
        assert_eq!(next.claim_type, "hvac_zonable");
        assert!(say_text(&d).contains(&next.text));
        assert!(!d.end_call);
        assert_eq!(s2.findings.len(), s1.findings.len() + d.findings.len());
    }
}

// ---- Extraction --------------------------------------------------------------------------------

#[test]
fn money_phrases_become_integer_cents() {
    let cases: [(&str, i64, &str, Option<MoneyPeriod>); 8] = [
        (
            "The rent is $4,500 a month.",
            450_000,
            "rent",
            Some(MoneyPeriod::Monthly),
        ),
        (
            "Rent is eighteen fifty a month.",
            185_000,
            "rent",
            Some(MoneyPeriod::Monthly),
        ),
        (
            "The retrofit would be about 40k.",
            4_000_000,
            "improvement_cost",
            None,
        ),
        (
            "The deposit is two months, so $9,000.",
            900_000,
            "security_deposit",
            None,
        ),
        (
            "Asking is $1,850 per month.",
            185_000,
            "rent",
            Some(MoneyPeriod::Monthly),
        ),
        (
            "Rent is $3.25 per square foot plus utilities",
            325,
            "rent",
            Some(MoneyPeriod::PerSqft),
        ),
        (
            "CAM runs about 1,200 dollars monthly.",
            120_000,
            "operating_expenses",
            Some(MoneyPeriod::Monthly),
        ),
        (
            "Utilities average twenty five hundred dollars a year.",
            250_000,
            "utilities_cost",
            Some(MoneyPeriod::Annual),
        ),
    ];
    for (phrase, cents, claim, period) in cases {
        let f = extract_findings(None, phrase);
        let m = f
            .iter()
            .find(|f| f.amount_cents.is_some())
            .unwrap_or_else(|| panic!("{phrase}: {f:?}"));
        assert_eq!(m.amount_cents, Some(cents), "{phrase}");
        assert_eq!(m.claim_type, claim, "{phrase}");
        assert_eq!(m.period, period, "{phrase}");
        assert!(m.needs_verification && m.reported_by == "counterparty");
        assert!(m.specialists.contains(&Specialist::Finance));
    }
    // "$3.25 per square foot plus utilities": the per-sqft rate is not utilities' cost; the
    // nearest cost word wins. Not money: amps, areas, years, a bare count.
    for phrase in [
        "It has 200 amps.",
        "About 1,200 square feet.",
        "A five year lease.",
        "We have 3 suites.",
    ] {
        let f = extract_findings(None, phrase);
        assert!(
            f.iter().all(|f| f.amount_cents.is_none()),
            "{phrase}: {f:?}"
        );
    }
}

#[test]
fn terms_parking_utilities_availability() {
    let f = extract_findings(None, "It's a five-year lease. Available March 1st. There are 4 parking spaces. Utilities are not included.");
    let get = |c: &str| {
        f.iter()
            .find(|x| x.claim_type == c)
            .unwrap_or_else(|| panic!("{c}: {f:?}"))
    };
    assert_eq!(get("lease_term_months").value, 60);
    assert_eq!(get("available_from").value, "march 1st");
    assert_eq!(get("parking_spaces").value, 4);
    assert_eq!(get("utilities_included").value, false);
    let f = extract_findings(None, "It may be available soon.");
    assert!(f.iter().all(|x| x.claim_type != "available_from"), "{f:?}");
    let f = extract_findings(None, "Street parking only, and utilities are included.");
    assert!(f
        .iter()
        .any(|x| x.claim_type == "parking_description" && x.value == "street"));
    assert!(f
        .iter()
        .any(|x| x.claim_type == "utilities_included" && x.value == true));
    let f = extract_findings(None, "The lease is Month-to-Month right now.");
    assert!(
        f.iter()
            .any(|x| x.claim_type == "lease_term_months" && x.value == "month-to-month"),
        "{f:?}"
    );
    // Infrastructure stated outside its question goes through the existing extract.
    let f = extract_findings(None, "The suite is 1,400 sq ft with a 100 amp panel.");
    assert!(f
        .iter()
        .any(|x| x.claim_type == "usable_area_sqft" && x.value == 1400));
    assert!(f.iter().any(|x| x.claim_type == "tenant_subpanel_amps"
        && x.value == 100
        && x.specialists == vec![Specialist::Feasibility]));
}

#[test]
fn yes_no_dont_know_to_the_question_just_asked() {
    let c = ctx_with(&["Is the space available for showings this week?"]);
    for (text, kind) in [
        ("Yes, definitely.", AnswerKind::Yes),
        ("No, not this week.", AnswerKind::No),
        ("I don't know, I'd have to check.", AnswerKind::DontKnow),
    ] {
        let (d, _) = one(&c, text);
        let a = d
            .findings
            .iter()
            .find(|f| f.question_id.as_deref() == Some("request:0"))
            .unwrap();
        assert_eq!(a.answer_kind, Some(kind), "{text}");
        assert_eq!(a.reported_by, "counterparty");
        assert_eq!(d.answered_question.as_deref(), Some("request:0"));
        if kind == AnswerKind::DontKnow {
            assert!(a.value.is_null() && !a.needs_verification);
        }
    }
}

// ---- Escalation rules: positive and near-miss -----------------------------------------------

fn rule(c: &CallContext, text: &str) -> Option<(EscalationRule, HandoffReason, bool)> {
    let (d, _) = one(c, text);
    d.escalation.map(|e| (e.rule, e.reason, e.hold))
}

#[test]
fn every_escalation_rule_has_a_positive_and_a_near_miss() {
    let c = ctx();
    let cases: [(&str, EscalationRule, HandoffReason, &str); 9] = [
        (
            "Can you hold the space for us until Friday?",
            EscalationRule::Commitment,
            HandoffReason::BindingCommitment,
            "The landlord holds open houses on Fridays.",
        ),
        (
            "Would the owner go to $5,000 a month?",
            EscalationRule::Concession,
            HandoffReason::ExceedsAuthority,
            "Would the owner like a tour on Friday?",
        ),
        (
            "Can I get the owner on the phone?",
            EscalationRule::OwnerRequested,
            HandoffReason::CounterpartyRequestedOwner,
            "The owner of the building lives in Texas.",
        ),
        (
            "Do they have a cosmetology license?",
            EscalationRule::LegalOrLicensing,
            HandoffReason::ExceedsAuthority,
            "It's a legacy building.",
        ),
        (
            "We'll need a guarantor on this one.",
            EscalationRule::Legacy,
            HandoffReason::GuarantorRequested,
            "The guard rail is new.",
        ),
        (
            "How would the first month payment work?",
            EscalationRule::Legacy,
            HandoffReason::FinancialDetails,
            "The first floor is all open.",
        ),
        (
            "Could we have your client call me?",
            EscalationRule::OwnerRequested,
            HandoffReason::CounterpartyRequestedOwner,
            "My client list is long.",
        ),
        (
            "What's the owner's credit history? I need to know that before we can continue.",
            EscalationRule::BlockingUnknown,
            HandoffReason::InsufficientEvidence,
            "What's the owner's credit history?",
        ),
        (
            "Could you send over the owner's date of birth?",
            EscalationRule::SensitiveData,
            HandoffReason::SensitivePersonalInfo,
            "The building's birth year is unclear.",
        ),
    ];
    for (pos, r, reason, near) in cases {
        let got = rule(&c, pos).unwrap_or_else(|| panic!("no escalation: {pos}"));
        assert_eq!((got.0, got.1), (r, reason), "{pos}");
        assert!(rule(&c, near).is_none(), "near miss escalated: {near}");
    }
    // The legacy rules are called unchanged: "speak to the owner", "talk to your client".
    assert_eq!(
        rule(&c, "Can I talk to your client directly?"),
        Some((
            EscalationRule::Legacy,
            HandoffReason::CounterpartyRequestedOwner,
            true
        ))
    );
    assert_eq!(
        rule(&c, "I'd like to speak to the owner."),
        Some((
            EscalationRule::Legacy,
            HandoffReason::CounterpartyRequestedOwner,
            true
        ))
    );
    // A concession or a legal question is taken back to the owner; the call continues.
    let (d, s) = one(&c, "Would the owner go to $5,000 a month?");
    assert!(matches!(&d.say, Say::Exact(t) if t.starts_with(CONCESSION_LINE)));
    assert!(!s.awaiting_owner && d.next_question.is_some() && s.owner_followups.len() == 1);
    // An unanswerable question that does not block: "I don't have that", no escalation.
    let (d, s) = one(&c, "What's the owner's credit history?");
    assert!(d.escalation.is_none() && say_text(&d).starts_with(DONT_KNOW_LINE));
    assert!(d.owner_followup.is_some() && s.owner_followups.len() == 1);
    // A question the profile answers: restated as the owner's requirement, never as an offer.
    let (d, _) = one(&c, "What's their budget for rent?");
    assert!(d.escalation.is_none());
    assert!(
        say_text(&d).contains("the owner is looking at up to $4,500 a month"),
        "{:?}",
        d.say
    );
    assert!(say_text(&d).contains("not as an offer"));
}

#[test]
fn a_commitment_is_refused_in_three_phrasings() {
    let c = ctx();
    for text in [
        "Can you sign the lease today?",
        "Can you hold the space for us until Friday?",
        "Can you commit to a three-year lease?",
    ] {
        let (d, s) = one(&c, text);
        let e = d.escalation.as_ref().unwrap_or_else(|| panic!("{text}"));
        assert_eq!(e.reason, HandoffReason::BindingCommitment, "{text}");
        assert!(e.hold && e.urgency == "HIGH");
        assert_eq!(d.say, Say::Exact(HOLD_LINE.into()));
        assert!(HOLD_LINE.contains("can't agree to anything"));
        assert!(s.awaiting_owner && d.next_question.is_none() && d.answered_question.is_none());
        // While the owner is being brought in, the assistant is silent.
        let (d2, _) = step(&c, &s, &cp(2, "Hello? Are you still there?"));
        assert_eq!(d2.say, Say::Silent);
    }
}

#[test]
fn sensitive_data_escalates_and_never_appears_in_the_output() {
    let c = ctx();
    let text = "Can you give me the owner's social security number? Mine is 123-45-6789 and the card is 4111 1111 1111 1111.";
    let (d, s) = one(&c, text);
    let e = d.escalation.as_ref().unwrap();
    assert_eq!(e.rule, EscalationRule::SensitiveData);
    assert!(e.sensitive_handoff && e.hold);
    assert_eq!(d.say, Say::Exact(SENSITIVE_LINE.into()));
    assert!(d.redacted && d.findings.is_empty());
    let out = format!(
        "{}{}",
        serde_json::to_string(&d).unwrap(),
        serde_json::to_string(&s).unwrap()
    );
    for secret in ["6789", "123-45", "4111", "1111"] {
        assert!(!out.contains(secret), "{secret} leaked");
    }
    // Without a keyword: an SSN shape still escalates and is redacted; a long run is redacted.
    let (d, _) = one(&c, "Sure, it's 078-05-1120.");
    assert_eq!(d.escalation.unwrap().rule, EscalationRule::SensitiveData);
    assert!(!d.stored_text.contains("1120"));
    let (r, any, label) = redact("Password is hunter2, pin 4321");
    assert!(
        any && label.is_some() && !r.contains("hunter2") && !r.contains("4321"),
        "{r}"
    );
    let (r, any, label) = redact("Call me back at 310 555 1234.");
    assert!(any && label.is_none() && !r.contains("1234"), "{r}");
    // Money is not redacted when no sensitive word is present.
    let (r, any, _) = redact("The rent is $4,500.");
    assert!(!any && r.contains("4,500"));
}

#[test]
fn identity_question_gets_the_identity_line() {
    let c = ctx();
    for text in [
        "Wait, am I talking to a real person?",
        "Is this a recording?",
        "Are you a robot?",
    ] {
        let (d, s) = one(&c, text);
        assert!(d.escalation.is_none(), "{text}");
        assert!(
            matches!(&d.say, Say::Exact(t) if t.starts_with(IDENTITY_LINE)),
            "{text}"
        );
        // The open question is re-asked, not marked answered.
        assert!(d.answered_question.is_none());
        assert_eq!(
            d.next_question.as_ref().map(|q| q.id.as_str()),
            s.current.as_deref()
        );
    }
    // A question back, in reply to a free-text request question, does not answer it.
    let c3 = ctx_with(&["Could we tour the space next week?"]);
    let (d, s) = one(&c3, "Which day did you have in mind?");
    assert!(d.answered_question.is_none() && s.current.as_deref() == Some("request:0"));
    assert!(d.findings.iter().all(|f| f.question_id.is_none()));
    let (d, _) = one(&c, "Is this a good time to talk about the suite?");
    assert!(!say_text(&d).starts_with(IDENTITY_LINE));
}

// ---- Authority scope and the question plan --------------------------------------------------

#[test]
fn questions_outside_scope_are_never_asked_and_may_not_topics_escalate() {
    let c = ctx_with(&[
        "What is the monthly rent?",
        "Could we tour the space next week?",
        "Is a deposit required?",
    ]);
    // Request questions first, then the bank's.
    assert_eq!(c.questions[0].source, QuestionSource::Request);
    assert_eq!(c.questions[3].source, QuestionSource::Bank);
    let (d0, mut s) = start(&c);
    // "rent" is not in the default may_ask_about; "deposit" is in may_not: both skipped.
    assert_eq!(d0.next_question.as_ref().unwrap().id, "request:1");
    assert!(s
        .skipped
        .iter()
        .any(|(id, why)| id == "request:0" && why.contains("may_ask_about")));
    let mut asked = vec![d0.next_question.unwrap().id];
    for t in 1..20u64 {
        let (d, n) = step(&c, &s, &cp(t, "I'm not sure."));
        s = n;
        if let Some(q) = &d.next_question {
            asked.push(q.id.clone());
        }
        if d.end_call {
            break;
        }
    }
    assert!(s.ended);
    assert!(!asked.contains(&"request:0".to_owned()) && !asked.contains(&"request:2".to_owned()));
    assert!(s
        .skipped
        .iter()
        .any(|(id, why)| id == "request:2" && why.contains("may_not")));
    // A topic named in may_not always escalates, even as a plain statement.
    let mut c2 = ctx();
    c2.authority = AuthorityScope {
        may_ask_about: AuthorityScope::default().may_ask_about,
        may_not: vec!["parking".into()],
    };
    let (d, _) = one(&c2, "There are 3 parking spots behind the building.");
    assert_eq!(d.escalation.unwrap().rule, EscalationRule::MayNotTopic);
    let (d, _) = one(&c2, "The suite has 200 amps.");
    assert!(d.escalation.is_none());
    // Every topic of a mixed utterance is checked, not only the first.
    let (d, _) = one(&c2, "The rent is $4,000 and there are 3 parking spots.");
    assert_eq!(d.escalation.unwrap().rule, EscalationRule::MayNotTopic);
    // Topics match at word starts: "currently" is not rent, "example" is not amps.
    assert_eq!(
        topic_of_question("Is the space currently vacant?"),
        "availability"
    );
    assert_eq!(
        topic_of_question("For example, could we tour it?"),
        "showing_times"
    );
}

#[test]
fn every_question_is_asked_once_and_the_call_ends_after_the_last() {
    let c = ctx_with(&["Could we tour the space next week?"]);
    let (d0, mut s) = start(&c);
    let mut asked = vec![d0.next_question.unwrap().id];
    let mut ended_at = None;
    for t in 1..20u64 {
        let (d, n) = step(&c, &s, &cp(t, "Yes."));
        s = n;
        if let Some(q) = &d.next_question {
            asked.push(q.id.clone());
        }
        if d.end_call {
            assert!(matches!(&d.say, Say::Exact(x) if x.starts_with("That's everything I needed")));
            ended_at = Some(t);
            break;
        }
    }
    let mut dedup = asked.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), asked.len(), "{asked:?}");
    assert_eq!(asked.len(), c.questions.len());
    assert_eq!(ended_at, Some(asked.len() as u64));
    // After the end, nothing more is said.
    let (d, _) = step(&c, &s, &cp(99, "One more thing"));
    assert!(d.ignored && d.say == Say::Silent);
}

#[test]
fn resume_after_a_handoff_reasks_the_open_question() {
    let c = ctx();
    let (d, s) = one(&c, "Can I get the owner on the phone?");
    assert_eq!(d.say, Say::Exact(OWNER_REQUESTED_LINE.into()));
    let open = s.current.clone().unwrap();
    let (r, s2) = resume(&c, &s, 2);
    assert_eq!(r.next_question.unwrap().id, open);
    assert!(!s2.awaiting_owner);
}

// ---- Determinism, idempotency, bounds -------------------------------------------------------

#[test]
fn same_input_same_decision_and_a_repeated_turn_returns_the_stored_one() {
    let c = ctx();
    let a = one(&c, "There's a sink and the rent is $4,200 a month.");
    let b = one(&c, "There's a sink and the rent is $4,200 a month.");
    assert_eq!(a, b);
    let (d, s) = a;
    let (again, s_again) = step(&c, &s, &cp(1, "something else entirely"));
    assert_eq!(again, d);
    assert_eq!(s_again, s);
    // An older turn id that was never decided is stale: stored, ignored.
    let (d3, s3) = step(&c, &s, &cp(5, "ok"));
    let (stale, _) = step(&c, &s3, &cp(4, "late"));
    assert!(!d3.ignored && stale.ignored);
}

#[test]
fn hostile_input_is_cut_and_bounded() {
    let c = ctx();
    let mut big = String::new();
    while big.len() < 100_000 {
        big.push_str("(((($1,1,1.1k ??!! [[{{\"'nineteen eighty -- a a a per per month))))");
    }
    let t0 = Instant::now();
    let (d, _) = one(&c, &big);
    let el = t0.elapsed();
    assert!(el < Duration::from_secs(5), "took {el:?}");
    assert!(d.truncated);
    assert!(d.stored_text.chars().count() <= MAX_ANALYSIS_CHARS + 64 * 10);
    assert!(d.findings.len() <= MAX_FINDINGS_PER_TURN);
    let (s, cut_) = cut(&"x".repeat(100_000));
    assert!(cut_ && s.chars().count() == MAX_ANALYSIS_CHARS);
    // Many money mentions: still at most MAX_FINDINGS_PER_TURN.
    let many = (1..200)
        .map(|i| format!("${i},000 retrofit"))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(extract_findings(None, &many).len() <= MAX_FINDINGS_PER_TURN);
}

#[test]
fn existing_escalation_is_unchanged_by_this_module() {
    // A spot check that the module calls the legacy rules rather than replacing them.
    assert_eq!(
        detect_escalation("we need a guarantor"),
        Some(HandoffReason::GuarantorRequested)
    );
    assert_eq!(detect_escalation("the suite is open"), None);
}

// ---- Review response-agent-1 (second-site-reviewer-b8ba6f78) --------------------------------

#[test]
fn review_indirect_commitments_hold_the_call() {
    for t in [
        "Great, so we're agreed at forty-five hundred.",
        "I'll put you down for the corner unit.",
        "I'll take that as a yes.",
        "I'll mark it as pending for your client then.",
        "Let's lock in a move-in date of June 1.",
        "Shall I send over the lease for signature?",
        "Can I tell the landlord you're in?",
        "Can you give me a verbal yes?",
        "Okay so it's yours at forty-four hundred, deal?",
        "Do we have a deal at $4,200?",
        "Can you confirm the owner wants it?",
        "Can you get me a check by Monday?",
        "Do you want it or not?",
    ] {
        let (d, s) = one(&ctx(), t);
        let e = d
            .escalation
            .as_ref()
            .unwrap_or_else(|| panic!("no escalation: {t}"));
        assert_eq!(e.rule, EscalationRule::Commitment, "{t}");
        assert!(e.hold && s.awaiting_owner, "{t}");
        assert_eq!(say_text(&d), HOLD_LINE, "{t}");
    }
    for t in [
        "The lock in the back door is new.",
        "It has been off the market for a while.",
    ] {
        let (d, _) = one(&ctx(), t);
        assert!(d.escalation.is_none(), "false positive: {t}");
    }
}

#[test]
fn review_sensitive_shapes_escalate_and_vanish() {
    for (t, secrets) in [
        (
            "Routing 021000021 account 12345678",
            &["021000021", "12345678"][..],
        ),
        (
            "zero seven eight zero five one one two zero",
            &["seven eight zero"][..],
        ),
        ("born 4/12/85", &["4/12/85", "85"][..]),
        (
            "I was born on April twelfth nineteen eighty five.",
            &["nineteen eighty five"][..],
        ),
        (
            "Social is oh seven eight, oh five, one one two oh.",
            &["seven eight", "one one two"][..],
        ),
        (
            "What's your client's social? Mine is seven eight zero zero five one one two zero",
            &["five one one"][..],
        ),
    ] {
        let (d, s) = one(&ctx(), t);
        let e = d
            .escalation
            .as_ref()
            .unwrap_or_else(|| panic!("no escalation: {t}"));
        assert!(e.sensitive_handoff, "{t}");
        let js = serde_json::to_string(&(&d, &s)).unwrap();
        for x in secrets {
            assert!(!js.contains(x), "{x} survives in {t}");
        }
    }
    // A phone number is redacted but does not hold the call.
    let (d, _) = one(&ctx(), "Call me back at 310 555 1234.");
    assert!(d.escalation.is_none());
    assert!(!d.stored_text.contains("555"));
}

// ---- Response Agent 2: the owed review findings (F5 to F10) ---------------------------------

#[test]
fn f5_a_number_given_in_parts_is_redacted_and_holds() {
    for (t, parts) in [
        ("078 05 and the rest, um, 1120.", &["078", "05", "1120"][..]),
        ("It's 078, 05, and then 1120.", &["078", "1120"][..]),
        (
            "oh seven eight, then oh five, and one one two oh",
            &["seven eight", "one one two"][..],
        ),
        ("4111 1111 then 1111 and 1111 exp 12/27", &["4111", "12/27", "27"][..]),
        (
            "Card 4111 1111 1111 1111, expires 12/27",
            &["12/27", "27"][..],
        ),
        ("The EIN is twelve dash three four five six seven eight nine", &["twelve"][..]),
        ("born on April twelfth nineteen eighty five", &["April", "twelfth"][..]),
        ("The account ending 4321", &["4321"][..]),
    ] {
        let (d, s) = one(&ctx(), t);
        let e = d
            .escalation
            .as_ref()
            .unwrap_or_else(|| panic!("no escalation: {t}"));
        assert!(e.sensitive_handoff && e.hold, "{t}");
        assert_eq!(d.say, Say::Exact(SENSITIVE_LINE.into()), "{t}");
        assert!(d.findings.is_empty(), "{t}");
        let js = serde_json::to_string(&(&d, &s)).unwrap();
        for x in parts {
            assert!(!js.contains(x), "{x} survives in {t}: {}", d.stored_text);
        }
    }
    // Near misses: numbers that are explained do not add up to a sensitive number.
    for t in [
        "Suite 1200, 3 parking spaces, 200 amps, 1,400 square feet.",
        "It's 1,400 sq ft at 123 Main Street, renovated in 2019, 200 amps.",
        "Rent is $4,500 a month, 5 year lease, available March 1st, 4 spots.",
        "Call me back at 310 555 1234.",
        "We have 12 units, 30 percent are 900 square feet.",
    ] {
        let (d, _) = one(&ctx(), t);
        assert!(
            d.escalation.as_ref().is_none_or(|e| !e.sensitive_handoff),
            "false positive: {t} -> {:?}",
            d.escalation
        );
    }
}

#[test]
fn f9_an_unrelated_statement_does_not_close_the_open_question() {
    let c = ctx();
    let (d0, s0) = start(&c);
    let open = d0.next_question.unwrap().id;
    assert_eq!(open, "bank:tenant_subpanel_amps");
    let (d1, s1) = step(&c, &s0, &cp(1, "Rent is $4,500 a month."));
    assert!(d1.answered_question.is_none(), "{d1:?}");
    assert_eq!(s1.current.as_deref(), Some(open.as_str()));
    assert_eq!(d1.next_question.as_ref().map(|q| q.id.as_str()), Some(open.as_str()));
    assert!(d1.findings.iter().all(|f| f.question_id.is_none()));
    assert!(d1.findings.iter().all(|f| !f.value.is_null()));
    assert!(d1.findings.iter().any(|f| f.claim_type == "rent"));
    // A second unrelated statement: the question has been put twice, it is skipped, the call moves on.
    let (d2, s2) = step(&c, &s1, &cp(2, "The building is very nice."));
    assert!(d2.answered_question.is_none());
    assert!(d2.findings.iter().all(|f| !f.value.is_null()));
    assert!(s2.skipped.iter().any(|(id, _)| *id == open));
    assert_ne!(d2.next_question.as_ref().map(|q| q.id.as_str()), Some(open.as_str()));
    // An answer still closes it: a value, yes, no, "I don't know".
    for a in ["About 100 amps.", "I don't know.", "Yes.", "No."] {
        let (d, s) = step(&c, &s0, &cp(1, a));
        assert_eq!(d.answered_question.as_deref(), Some(open.as_str()), "{a}");
        assert!(s.current.as_deref() != Some(open.as_str()), "{a}");
    }
}

#[test]
fn f6_f10_a_number_asked_or_proposed_is_not_a_claim() {
    for t in [
        "Does $4,600 work for your client?",
        "So we're agreed at $4,500?",
        "Would the owner go to $5,000 a month?",
        "How about $4,400 a month?",
        "Is the owner OK with $5,000 a month?",
    ] {
        let (d, _) = one(&ctx(), t);
        assert!(
            d.findings.iter().all(|f| f.amount_cents.is_none()),
            "{t}: {:?}",
            d.findings
        );
        assert!(d.escalation.is_some() || d.owner_followup.is_some(), "{t}");
    }
    // Near misses: statements keep their claim.
    for (t, claim, cents) in [
        ("Rent's $4500/mo", "rent", 450_000),
        ("Rent is $4,500 a month, does that work for your client?", "rent", 450_000),
        ("The asking rent is $4,500 a month.", "rent", 450_000),
    ] {
        let found: Vec<_> = extract_findings(None, t)
            .into_iter()
            .filter(|f| f.amount_cents.is_some())
            .collect();
        assert_eq!(found.len(), 1, "{t}: {found:?}");
        assert_eq!(found[0].claim_type, claim, "{t}");
        assert_eq!(found[0].amount_cents, Some(cents), "{t}");
    }
}

#[test]
fn f7_a_range_is_one_finding_without_an_amount() {
    for (t, whole) in [
        ("Rent is $4,000 to $5,000 a month.", "$4,000 to $5,000 a month"),
        ("Rent is between four and five thousand a month.", "four and five thousand a month"),
        ("Rent runs 4000-5000 a month.", "4000-5000 a month"),
    ] {
        let f: Vec<_> = extract_findings(None, t)
            .into_iter()
            .filter(|f| f.unit.as_deref().is_some_and(|u| u.starts_with("USD")))
            .collect();
        assert_eq!(f.len(), 1, "{t}: {f:?}");
        assert_eq!(f[0].amount_cents, None, "{t}");
        assert!(f[0].needs_verification, "{t}");
        assert_eq!(f[0].value.as_str(), Some(whole), "{t}");
        assert!(f[0].reported.contains(whole), "{t}");
    }
    // Near misses: two separate amounts stay two claims.
    let f = extract_findings(None, "Rent is $4,500 and the deposit is $9,000.");
    assert_eq!(
        f.iter().filter_map(|f| f.amount_cents).collect::<Vec<_>>(),
        vec![450_000, 900_000]
    );
    let f = extract_findings(None, "It needs $40,000 to fix the plumbing.");
    assert_eq!(f.iter().filter_map(|f| f.amount_cents).collect::<Vec<_>>(), vec![4_000_000]);
}
