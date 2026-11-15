//! `extract` never reads a negation as yes and never reports a cut number (commission
//! rust-domain-5). The same cases as `apps/api/tests/test_call_script_truth.py`; the golden vectors
//! (`gen_domain4.py`, `TRUTH_ANSWERS`, checked in `golden_comms.rs`) compare the two languages on
//! every answer.

mod common;

use serde_json::{json, Value};
use ss_domain::communications::call_script::extract;
use ss_domain::contracts::communications::CallQuestion;

fn q(claim: &str) -> CallQuestion {
    CallQuestion {
        question: "q".into(),
        claim_type: claim.into(),
        requirement_id: None,
    }
}

fn value(claim: &str, answer: &str) -> Value {
    extract(&q(claim), answer).value
}

const YES_NO: [&str; 2] = ["in_unit_plumbing", "hvac_zonable"];

/// (answer, value): a negated affirmation is never `true`.
const NEGATED: &[(&str, Option<bool>)] = &[
    ("Absolutely not", None),
    ("Absolutely not.", None),
    ("Definitely not.", None),
    ("It does not", None),
    ("it does NOT", None),
    ("it does\u{a0}not", None),
    ("it does n't", None),
    ("There is not a sink.", None),
    ("There's not a sink.", None),
    ("There\u{2019}s not a sink.", None),
    ("We have not had plumbing there.", None),
    ("Not correct.", None),
    ("That's not correct.", None),
    ("That isn\u{2019}t correct.", None),
    ("Not definitely.", None),
    ("Not absolutely.", None),
    ("Not yes.", None),
    ("Definitely never.", None),
    ("It has never had a sink.", None),
    ("We have nothing like that.", None),
    ("There is hardly any plumbing.", None),
    ("\u{131}t doe\u{17f} not", None),
    ("No, it does not.", Some(false)),
    ("No, absolutely not.", Some(false)),
    ("Absolutely not, it's all open.", Some(false)),
    ("absolutely not \u{2013} it\u{2019}s all open", Some(false)),
    ("There is no plumbing", Some(false)),
    ("There's no sink.", Some(false)),
    ("There\u{2019}s no sink.", Some(false)),
    ("It has no plumbing.", Some(false)),
    ("We have no plumbing.", Some(false)),
    ("No, there's nothing", Some(false)),
];

/// Plain answers keep the value they had before the fix.
const PLAIN: &[(&str, bool)] = &[
    ("Yes, there's a sink in back", true),
    ("No, it's all open", false),
    ("yes", true),
    ("Yeah, there is.", true),
    ("It does.", true),
    ("We have a sink and a floor drain.", true),
    ("That's correct.", true),
    ("Sure thing, there's a sink.", true),
    ("YES THERE IS", true),
    ("ye\u{17f}", true),
    ("no", false),
    ("Nope, none.", false),
    ("Not really.", false),
    ("There isn't.", false),
    ("It doesn't.", false),
    ("We don't have one.", false),
    ("No, we don't.", false),
    ("No, never had any.", false),
    ("We don't, no.", false),
    ("no no no", false),
];

/// Mixed, hedged, idiomatic or double-negated answers are unclear.
const UNCLEAR: &[&str] = &[
    "Yes and no.",
    "yes no",
    "Sure, no problem.",
    "No problem.",
    "No, not a problem.",
    "We have no problem with that.",
    "Yes, it doesn't leak.",
    "Yes, without a doubt.",
    "Yes, it cannot leak.",
    "correct, none",
    "It's not that there isn't a sink.",
    "I wouldn't say there's no plumbing.",
    "It's not like we don't have one.",
    "There is not no sink.",
    "Not no.",
    "I\u{2019}ll find out, yes",
    "I don\u{2019}t know.",
    "",
];

const AMPS_KEEP: &[(&str, i64)] = &[
    ("It's a 400 amp service, 3 phase", 400),
    ("400a", 400),
    ("400 amps", 400),
    ("400-amp panel", 400),
    ("200 AMPS", 200),
    ("about 100a.", 100),
    ("(400 amps)", 400),
    ("Panel:400 amps", 400),
    ("a 120/208 volt, 400 amp service", 400),
    ("150\u{a0}amps", 150),
    ("\u{a0}400 amps", 400),
    ("400 amps, yes 400 amps", 400),
    ("400 amps or 400 amps", 400),
    ("200 amp panel, about 80 in use", 200),
    ("It's 400 amps or so", 400),
];

const SQFT_KEEP: &[(&str, i64)] = &[
    ("about 1,128 square feet", 1128),
    ("1,200 sq ft", 1200),
    ("1200sf", 1200),
    ("it's 1,128 SF per CoStar", 1128),
    ("950,000 square feet", 950_000),
    ("12,345 sq ft", 12_345),
    ("1234567 sf", 1_234_567),
    ("~1,128 sf", 1128),
    ("1,128\u{a0}sf", 1128),
    ("1,128 sf, I mean 1,128 sf", 1128),
    ("1,128 sf or 1128 sf", 1128),
];

const AMPS_UNCLEAR: &[&str] = &[
    "12345 amps",
    "123456 amps",
    "1,200 amps",
    "1.200 amps",
    "1 200 amps",
    "1\u{a0}200 amps",
    "1, 200 amps",
    "400.50 amps",
    "x400 amps",
    "-400 amps",
    "the panel is 60 amps, service 400 amps",
    "mostly 225 amp service; about 80 amps in use",
    "100 and 200 amps",
    "200 to 400 amps",
    "200-400 amps",
    "200 - 400 amps",
    "200\u{2013}400 amps",
    "200 or 400 amps",
    "200/400 amps",
    "between 200 and 400 amps",
    "200 amps or 400",
    "200 amps to 400 amps",
    "0 amps",
    "00 amps",
    "0000 amps",
    "\u{664}\u{660}\u{660} amps",
    "4\u{660}\u{660} amps",
    "\u{ff14}\u{ff10}\u{ff10} amps",
    "\u{661}\u{662}\u{663}45 amps",
    "I don\u{2019}t know, 100 amps?",
];

const SQFT_UNCLEAR: &[&str] = &[
    "9,500,000 sf",
    "1234567890 sf",
    "1.200 sq ft",
    "1 128 sq ft",
    "1\u{202f}128 sq ft",
    "1'128 sq ft",
    "1\u{2019}128 sq ft",
    "11,28 sf",
    "1,1280 sf",
    "1,,128 sf",
    "1,2 sq ft",
    "1,128,000 sf",
    "1128.500 sf",
    "1128.5 sf",
    "between 1,000 and 1,200 sf",
    "between 900 and 1,000 sq ft",
    "1,000 to 1,200 sf",
    "1,000-1,200 sf",
    "1,128 sf per CoStar, 1,100 sf measured",
    "000 sf",
    "0,000 sf",
    "\u{669}\u{665}\u{660} sq ft",
    "1\u{661}28 sf",
];

#[test]
fn a_negated_affirmation_is_never_yes() {
    for claim in YES_NO {
        for (answer, expected) in NEGATED {
            let want = expected.map_or(Value::Null, Value::Bool);
            assert_eq!(value(claim, answer), want, "{claim} {answer:?}");
        }
    }
}

#[test]
fn plain_yes_and_no_keep_their_value() {
    let long_yes = "yes ".repeat(300);
    let long_no = "no ".repeat(300);
    let plain = PLAIN
        .iter()
        .copied()
        .chain([(long_yes.as_str(), true), (long_no.as_str(), false)]);
    for (answer, expected) in plain {
        for claim in YES_NO {
            assert_eq!(value(claim, answer), json!(expected), "{claim} {answer:?}");
        }
    }
}

#[test]
fn mixed_hedged_and_double_negated_answers_are_unclear() {
    let long = format!(
        "Yes, {}it does not drain",
        "there is a sink and ".repeat(50)
    );
    for answer in UNCLEAR.iter().copied().chain([long.as_str()]) {
        assert_eq!(value("in_unit_plumbing", answer), Value::Null, "{answer:?}");
    }
}

#[test]
fn numbers_read_correctly_keep_their_value() {
    let long = format!("{} 400 amps", "x".repeat(3000));
    for (answer, n) in AMPS_KEEP.iter().copied().chain([(long.as_str(), 400)]) {
        let f = extract(&q("tenant_subpanel_amps"), answer);
        assert_eq!(
            (f.value, f.unit.as_deref()),
            (json!(n), Some("A")),
            "{answer:?}"
        );
    }
    for (answer, n) in SQFT_KEEP {
        let f = extract(&q("usable_area_sqft"), answer);
        assert_eq!(
            (f.value, f.unit.as_deref()),
            (json!(n), Some("sqft")),
            "{answer:?}"
        );
    }
}

#[test]
fn cut_joined_or_several_numbers_are_unclear() {
    for (claim, answers) in [
        ("tenant_subpanel_amps", AMPS_UNCLEAR),
        ("usable_area_sqft", SQFT_UNCLEAR),
    ] {
        for answer in answers {
            let f = extract(&q(claim), answer);
            assert_eq!((f.value, f.unit), (Value::Null, None), "{claim} {answer:?}");
        }
    }
}

/// Whether `form` occurs in `answer` with no word character, `.`, `,`, `'`, `’` or `-` directly
/// before it, and no digit (alone or after `.` or `,`) directly after it.
fn whole_token(answer: &str, form: &str) -> bool {
    answer.match_indices(form).any(|(i, _)| {
        let before = answer[..i].chars().next_back();
        let mut after = answer[i + form.len()..].chars();
        let joined_before =
            before.is_some_and(|c| c.is_alphanumeric() || "_.,'\u{2019}-".contains(c));
        let joined_after = match after.next() {
            Some(c) if c.is_numeric() => true,
            Some('.' | ',') => after.next().is_some_and(char::is_numeric),
            _ => false,
        };
        !joined_before && !joined_after
    })
}

#[test]
fn a_reported_number_is_a_whole_token_of_the_answer() {
    for (claim, keep, unclear) in [
        ("tenant_subpanel_amps", AMPS_KEEP, AMPS_UNCLEAR),
        ("usable_area_sqft", SQFT_KEEP, SQFT_UNCLEAR),
    ] {
        let answers = keep.iter().map(|(a, _)| *a).chain(unclear.iter().copied());
        for answer in answers {
            if let Some(n) = value(claim, answer).as_i64() {
                let grouped = ss_domain::py::group_thousands(i128::from(n));
                assert!(
                    whole_token(answer, &n.to_string()) || whole_token(answer, &grouped),
                    "{answer:?} -> {n}"
                );
            }
        }
    }
}

/// The canned answers of both call simulators, copied here (ss-domain cannot depend on
/// ss-server); the test checks the copies are still verbatim in both sources.
const SIMULATOR: &[(&str, &str)] = &[
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
const FALLBACKS: [&str; 2] = [
    "I'm not sure, I'd have to check.",
    "Not sure, I'd have to check the plans.",
];

#[test]
fn the_simulators_canned_answers_keep_their_values() {
    let root = common::repo_root();
    for src in [
        "apps/api/app/services/call_orchestration/simulator.py",
        "apps/backend/crates/server/src/services/call_simulator.rs",
    ] {
        let text = std::fs::read_to_string(root.join(src)).expect(src);
        for (_, answer) in SIMULATOR {
            assert!(text.contains(answer), "{src} no longer has {answer:?}");
        }
        assert!(text.contains(FALLBACKS[0]), "{src} fallback");
    }
    let expected = [
        json!(200),
        json!(true),
        json!(true),
        json!(1250),
        json!("It's available December first."),
    ];
    for ((claim, answer), want) in SIMULATOR.iter().zip(expected) {
        assert_eq!(value(claim, answer), want, "{claim}");
    }
    for fallback in FALLBACKS {
        for (claim, _) in SIMULATOR {
            assert_eq!(value(claim, fallback), Value::Null, "{claim} {fallback:?}");
        }
    }
}

#[test]
fn typographic_apostrophes_read_like_ascii_ones() {
    assert_eq!(
        value("in_unit_plumbing", "there\u{2019}s a sink"),
        json!(true)
    );
    assert_eq!(
        value("in_unit_plumbing", "We don\u{2019}t have one."),
        json!(false)
    );
    assert_eq!(
        value("in_unit_plumbing", "don\u{2bc}t have one"),
        json!(false)
    );
    assert_eq!(value("in_unit_plumbing", "It doesn\u{2018}t"), json!(false));
    assert_eq!(value("available_from", "I\u{2019}ll find out"), Value::Null);
    // The stored answer and a text value keep the original characters.
    let f = extract(&q("available_from"), "It\u{2019}s free in May");
    assert_eq!(f.answer.as_deref(), Some("It\u{2019}s free in May"));
    assert_eq!(f.value, json!("It\u{2019}s free in May"));
}

fn ranges(v: &Value) -> Vec<(u32, u32)> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_u64().unwrap() as u32, r[1].as_u64().unwrap() as u32))
        .collect()
}

fn in_ranges(rs: &[(u32, u32)], cp: u32) -> bool {
    rs.iter().any(|&(a, b)| a <= cp && cp <= b)
}

#[test]
fn is_decimal_is_cpythons_isdecimal() {
    // `str.isdecimal()` is what `\d` matches in a str pattern: the `digit` ranges CPython wrote
    // into part4_unicode.json. Every code point CPython's Unicode assigns must agree.
    let doc = common::golden("domain/part4_unicode.json");
    let (digit, assigned) = (ranges(&doc["digit"]), ranges(&doc["assigned"]));
    let mut checked = 0u32;
    for cp in assigned.iter().flat_map(|&(a, b)| a..=b) {
        let Some(c) = char::from_u32(cp) else {
            continue; // surrogates
        };
        checked += 1;
        assert_eq!(
            ss_domain::py::is_decimal(c),
            in_ranges(&digit, cp),
            "U+{cp:04X}"
        );
    }
    assert!(checked > 280_000, "{checked}");
}
