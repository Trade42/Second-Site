// TEMPORARY reviewer probe (call-summary-1-review); deleted after the review.
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use ss_core::UtcDateTime;
use ss_domain::communications::call_summary::*;
use ss_domain::communications::response_agent::MoneyPeriod;
use ss_domain::contracts::communications::TranscriptTurn;
use uuid::Uuid;

fn t(s: &str, x: &str) -> TranscriptTurn { TranscriptTurn { speaker: s.into(), text: x.into(), at: UtcDateTime::from_datetime(Utc.with_ymd_and_hms(2026,11,15,10,0,0).unwrap()) } }
fn f(claim: &str, topic: &str, rep: Option<&str>, v: Value, c: Option<i64>, p: Option<MoneyPeriod>, purpose: Option<&str>) -> InputFinding {
    InputFinding { question: None, claim_type: claim.into(), topic: topic.into(), reported: rep.map(Into::into), value: v, unit: None, amount_cents: c, period: p, purpose: purpose.map(Into::into) }
}
fn q(id: &str, text: &str, topic: &str, claim: &str) -> OutreachQuestion { OutreachQuestion { id: id.into(), text: text.into(), topic: topic.into(), claim_type: claim.into() } }
fn base() -> SummaryInput {
    SummaryInput { call_id: Uuid::from_u128(1), property_id: None, property_label: None, contact_name: Some("Pat".into()), contact_role: Some("property manager".into()), started_at: None, ended_at: None, ended: CallEnd::Completed, questions: vec![], transcript: vec![], findings: vec![], deferred_questions: vec![], handoffs: vec![], simulated: true, monthly_rent_limit_cents: Some(400_000) }
}

#[test]
fn probe() {
    let mut i = base();
    i.findings = vec![
        f("monthly_rent","rent",Some("Rent is $4,200.50 a month, verified by the city."),json!(4200.5),Some(420_050),Some(MoneyPeriod::Monthly),None),
        f("deposit","deposit",Some("Deposit is two months, confirmed by the city."),json!("two months"),Some(840_100),None,None),
        f("annual_tax","cost",Some("Taxes $12,000 a year"),json!(12000),Some(1_200_000),Some(MoneyPeriod::Annual),None),
        f("electrical_amps","infrastructure",Some("200 amps"),json!(200),None,None,None),
        f("in_unit_plumbing","infrastructure",Some("No."),json!(false),None,None,None),
        f("improvement_cost","improvement",Some("needs a $40,000 retrofit"),json!(40000),Some(4_000_000),None,Some("retrofit SSN 123-45-6789")),
        f("hvac_zonable","infrastructure",Some("No."),json!(false),None,None,None),
        f("acct","listing",None,json!("account 123456789012"),None,None,None),
        f("acctnum","listing",None,json!(123456789012u64),None,None,None),
        f("parking","listing",Some("Yes."),json!(true),None,None,None),
        f("available_from","listing",Some("I don't know"),Value::Null,None,None,None),
        f("lease_term","lease_terms",Some("I'd rather not say"),Value::Null,None,None,None),
    ];
    i.questions = vec![q("a","When is it available?","listing","available_from"), q("b","Lease term?","lease_terms","lease_term"), q("c","Parking?","listing","parking"), q("d","Ceiling?","listing","ceiling_height")];
    i.transcript = vec![
        t("counterparty","great, we'll hold it for you"),
        t("counterparty","so you'll send the deposit Friday"),
        t("counterparty","the owner agreed to 4,200"),
        t("assistant","the owner agreed to 4,200"),
        t("assistant","Sure, we'll send the deposit Friday, no problem."),
        t("assistant","Great, we'll take it."),
        t("counterparty","My card is 4111 1111 1111 1111 and SSN 123-45-6789, account 000123456789"),
    ];
    let s = summarize(&i);
    let js = serde_json::to_string_pretty(&s).unwrap();
    println!("{js}");
    let txt = owner_text(&s);
    println!("TEXT({}): {txt}", txt.chars().count());
    assert_eq!(js, serde_json::to_string_pretty(&summarize(&i)).unwrap());
    // long text truncation
    let mut l = base();
    l.findings = (0..30).map(|k| f(&format!("c{k}"),"cost",Some(&format!("fee number {k} is $1,234,567.89 one time")),json!(k),Some(123_456_789),None,None)).collect();
    let ls = summarize(&l); let lt = owner_text(&ls);
    println!("LONG({}): {lt}", lt.chars().count());
    // empty calls
    for e in [CallEnd::NoAnswer, CallEnd::Voicemail, CallEnd::HungUp] {
        let mut x = base(); x.ended = e; x.questions = i.questions.clone();
        if e == CallEnd::HungUp { x.transcript = vec![t("counterparty","Not interested."), t("assistant","When is it available?")]; }
        let es = summarize(&x);
        println!("EMPTY {e:?}: learned_nothing={} text={}", es.nothing_learned, owner_text(&es));
    }
}
