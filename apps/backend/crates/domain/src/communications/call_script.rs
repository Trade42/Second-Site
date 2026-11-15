//! Port of `apps/api/app/domains/communications/call_script.py`: what the assistant asks a
//! broker, and how answers become structured findings.
//!
//! Questions are grounded in the property's open feasibility assessments (UNKNOWN /
//! MAY_REQUIRE_WORK), so every call exists to resolve a specific unknown. Extraction is
//! deterministic and conservative: an answer that does not clearly state a value yields a finding
//! with `value = null` ("unclear"), never a guessed number. Every finding is
//! `classification = "OWNER_REPORTED"` and `speaker = "counterparty"`: what a broker says stays
//! owner- or broker-reported evidence.

use std::sync::OnceLock;

use serde_json::Value;

use crate::contracts::communications::{CallQuestion, StructuredFinding};
use crate::contracts::infrastructure::{AssessmentStatus, InfrastructureAssessment};
use crate::py::{self, PyMatch, PyRegex};

/// `(requirement suffix, question)` in Python's order: `QUESTION_BANK`.
pub const QUESTION_BANK: [(&str, &str, &str); 4] = [
    (
        "electrical.tenant_service",
        "How many amps is the electrical service to the suite's own panel, and roughly how much is in use?",
        "tenant_subpanel_amps",
    ),
    (
        "plumbing.treatment_sinks",
        "Is there existing plumbing inside the suite, like a sink or a floor drain?",
        "in_unit_plumbing",
    ),
    (
        "hvac.ventilation",
        "Could the HVAC be zoned so that separate small rooms each get supply and return air?",
        "hvac_zonable",
    ),
    (
        "space.min_area",
        "What is the usable square footage of the suite?",
        "usable_area_sqft",
    ),
];

/// `AVAILABILITY.question` / `.claim_type`.
pub const AVAILABILITY_QUESTION: &str = "When would the space be available to start a lease?";
pub const AVAILABILITY_CLAIM: &str = "available_from";

/// `MAX_QUESTIONS`.
pub const MAX_QUESTIONS: usize = 4;

/// `AVAILABILITY`: the question every script ends with.
pub fn availability() -> CallQuestion {
    CallQuestion {
        question: AVAILABILITY_QUESTION.to_owned(),
        claim_type: AVAILABILITY_CLAIM.to_owned(),
        requirement_id: None,
    }
}

/// Python's patterns, verbatim (all compiled with `re.I`). `YES` keeps the original alternatives
/// in the original order, grouped: 1 interjections, 2 predicates, 3 adverbs.
pub const YES_PATTERN: &str = r"\b(?:(yes|yeah|yep)|(there is|there's|it has|it does|we have)|(definitely|absolutely|correct|sure))\b";
pub const NO_PATTERN: &str =
    r"\b(no|nope|none|not really|there isn'?t|doesn'?t|don'?t have|it's all open|no plumbing)\b";
pub const UNSURE_PATTERN: &str =
    r"\b(not sure|don'?t know|no idea|would have to check|have to check|unsure|i'?ll find out)\b";
pub const AMPS_PATTERN: &str = r"(\d{2,4})\s*(?:-|\s)?(?:amps?|amperes?|a\b)";
pub const SQFT_PATTERN: &str =
    r"(\d[\d,]{2,6})\s*(?:usable\s*)?(?:sq(?:uare)?\.?\s*(?:ft|feet)|sf\b)";

// Truth rules for `extract`, written as Python's `call_script.py` writes them; the golden vectors
// (`gen_domain4.py`) compare the two answer by answer. Characters: a word character is Python's
// `\w` ([`py::is_py_word`]), a space is `str.isspace()` ([`py::is_space`]), a digit that counts as
// part of a reported number is ASCII 0-9 only (any other Unicode decimal next to a number makes it
// unclear), and the apostrophes below are read as `'` before any pattern runs.

/// Read as `'`: right and left single quotation marks, modifier letter apostrophe.
pub const APOSTROPHES: [char; 3] = ['\u{2019}', '\u{2018}', '\u{2BC}'];
/// A word, with inner apostrophes ("doesn't", "there's").
pub const WORD_PATTERN: &str = r"\w+(?:'\w+)*";
/// Idioms whose "no" is not an answer (`re.I`).
pub const NO_IDIOM_PATTERN: &str =
    r"\b(?:no|not\s+an?)\s+(?:problems?|issues?|worries|doubts?|questions?|reason)\b";
pub const NEGATIONS: [&str; 15] = [
    "not", "no", "nope", "none", "never", "nor", "neither", "without", "cannot", "hardly",
    "barely", "scarcely", "nothing", "nobody", "nowhere",
];
/// Contractions written without an apostrophe; any word ending in `n't` is one too.
pub const CONTRACTIONS: [&str; 20] = [
    "aint", "arent", "cant", "couldnt", "didnt", "doesnt", "dont", "hadnt", "hasnt", "havent",
    "isnt", "mightnt", "mustnt", "neednt", "shant", "shouldnt", "wasnt", "werent", "wont",
    "wouldnt",
];
/// Words that negate the affirmation right after them (and every contraction).
pub const NEGATE_BEFORE: [&str; 6] = ["not", "never", "cannot", "hardly", "barely", "scarcely"];
/// Words that negate an adverb affirmation right before them.
pub const NEGATE_AFTER_ADVERB: [&str; 2] = ["not", "never"];
/// Characters allowed directly before a reported number (besides a space or the start).
pub const NUMBER_OPENERS: &str = "([{\"\u{201C}~:";
/// Characters that join a number to an earlier one: hyphens, dashes, minus, `~ / , . : + &`.
pub const NUMBER_JOINERS: &str = "-\u{2010}\u{2011}\u{2012}\u{2013}\u{2014}\u{2015}\u{2212}~/,.:+&";
pub const JOIN_WORDS_BEFORE: [&str; 9] = [
    "to", "and", "or", "through", "thru", "till", "until", "vs", "versus",
];
pub const JOIN_WORDS_AFTER: [&str; 6] = ["to", "or", "through", "thru", "till", "until"];

struct Patterns {
    yes: PyRegex,
    no: PyRegex,
    unsure: PyRegex,
    amps: PyRegex,
    sqft: PyRegex,
    word: PyRegex,
    no_idiom: PyRegex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        yes: PyRegex::new(YES_PATTERN, true),
        no: PyRegex::new(NO_PATTERN, true),
        unsure: PyRegex::new(UNSURE_PATTERN, true),
        amps: PyRegex::new(AMPS_PATTERN, true),
        sqft: PyRegex::new(SQFT_PATTERN, true),
        word: PyRegex::new(WORD_PATTERN, false),
        no_idiom: PyRegex::new(NO_IDIOM_PATTERN, true),
    })
}

/// `build_script(assessments)`: the bank questions whose requirement suffix (the part after the
/// first `.`) belongs to an UNKNOWN or MAY_REQUIRE_WORK assessment, in bank order, at most
/// `MAX_QUESTIONS - 1` of them, each carrying that assessment's `requirement_id`, then
/// [`availability`]. When several open assessments share a suffix, the last one wins (Python
/// builds a dict).
pub fn build_script(assessments: &[InfrastructureAssessment]) -> Vec<CallQuestion> {
    let mut open: Vec<(&str, &str)> = Vec::new();
    for a in assessments {
        if !matches!(
            a.status,
            AssessmentStatus::Unknown | AssessmentStatus::MayRequireWork
        ) {
            continue;
        }
        let Some((_, suffix)) = a.requirement_id.split_once('.') else {
            continue;
        };
        match open.iter_mut().find(|(s, _)| *s == suffix) {
            Some(entry) => entry.1 = &a.requirement_id,
            None => open.push((suffix, &a.requirement_id)),
        }
    }
    let mut script: Vec<CallQuestion> = QUESTION_BANK
        .iter()
        .filter_map(|(suffix, question, claim)| {
            open.iter()
                .find(|(s, _)| s == suffix)
                .map(|(_, rid)| CallQuestion {
                    question: (*question).to_owned(),
                    claim_type: (*claim).to_owned(),
                    requirement_id: Some((*rid).to_owned()),
                })
        })
        .take(MAX_QUESTIONS - 1)
        .collect();
    script.push(availability());
    script
}

fn is_word(c: char) -> bool {
    py::is_py_word(c)
}

/// ASCII lower case; a word with any non-ASCII character is never one of the listed words.
fn folded(w: &str) -> String {
    if w.is_ascii() {
        w.to_ascii_lowercase()
    } else {
        String::new()
    }
}

fn listed(list: &[&str], w: &str) -> bool {
    let f = folded(w);
    list.contains(&f.as_str())
}

fn is_contraction(w: &str) -> bool {
    let mut tail = w.chars().rev();
    let ends_nt = matches!(
        (tail.next(), tail.next(), tail.next()),
        (Some('t' | 'T'), Some('\''), Some('n' | 'N'))
    );
    ends_nt || listed(&CONTRACTIONS, w)
}

fn is_negation(w: &str) -> bool {
    listed(&NEGATIONS, w) || is_contraction(w)
}

fn char_before(t: &str, i: usize) -> Option<char> {
    t[..i].chars().next_back()
}

fn char_at(t: &str, i: usize) -> Option<char> {
    t[i..].chars().next()
}

fn skip_space_back(t: &str, mut i: usize) -> usize {
    while let Some(c) = char_before(t, i).filter(|&c| py::is_space(c)) {
        i -= c.len_utf8();
    }
    i
}

fn skip_space_fwd(t: &str, mut i: usize) -> usize {
    while let Some(c) = char_at(t, i).filter(|&c| py::is_space(c)) {
        i += c.len_utf8();
    }
    i
}

/// Python's `_affirmed`: a YES match is an affirmation unless a negation is next to it, separated
/// by spaces only: before it one of [`NEGATE_BEFORE`] or a contraction ("isn't correct"); after a
/// predicate (group 2: there is, there's, it has, it does, we have) any negation word ("it does
/// not", "there's no"); after an adverb (group 3: definitely, absolutely, correct, sure) one of
/// [`NEGATE_AFTER_ADVERB`] ("absolutely not"). `words` are the word spans, sorted and not
/// overlapping, so both their starts and their ends are sorted (Python looks them up in dicts).
fn affirmed(t: &str, m: &PyMatch<'_>, words: &[(usize, usize)]) -> bool {
    let (start, end) = m.span();
    let i = skip_space_back(t, start);
    let before = if i < start {
        words
            .binary_search_by_key(&i, |w| w.1)
            .ok()
            .map(|k| &t[words[k].0..words[k].1])
    } else {
        None
    };
    let j = skip_space_fwd(t, end);
    let after = if j > end {
        words
            .binary_search_by_key(&j, |w| w.0)
            .ok()
            .map(|k| &t[words[k].0..words[k].1])
    } else {
        None
    };
    if before.is_some_and(|w| listed(&NEGATE_BEFORE, w) || is_contraction(w)) {
        return false;
    }
    if m.group(2).is_some() && after.is_some_and(is_negation) {
        return false;
    }
    if m.group(3).is_some() && after.is_some_and(|w| listed(&NEGATE_AFTER_ADVERB, w)) {
        return false;
    }
    true
}

/// Python's `_negation_before_no`: a negation word outside every NO match that comes before a NO
/// match in the same clause (no punctuation between them), a double negative such as "it's not
/// that there isn't a sink". Punctuation is any character that is not a word character, a space
/// or `'`. NO matches are sorted and do not overlap, so only the last NO match starting at or
/// before the word can contain it, and only the first one after it can be in its clause.
fn negation_before_no(t: &str, negations: &[(usize, usize)], noes: &[(usize, usize)]) -> bool {
    let breaks: Vec<usize> = t
        .char_indices()
        .filter(|&(_, c)| !(is_word(c) || py::is_space(c) || c == '\''))
        .map(|(i, _)| i)
        .collect();
    for &(s, e) in negations {
        let k = noes.partition_point(|&(ns, _)| ns <= s);
        if k > 0 && e <= noes[k - 1].1 {
            continue;
        }
        let k = noes.partition_point(|&(ns, _)| ns < e);
        if let Some(&(ns, _)) = noes.get(k) {
            let b = breaks.partition_point(|&p| p < e);
            if breaks.get(b).is_none_or(|&p| p >= ns) {
                return true;
            }
        }
    }
    false
}

fn yes_no(p: &Patterns, t: &str) -> Option<bool> {
    let words: Vec<(usize, usize)> = p.word.search_all(t).iter().map(PyMatch::span).collect();
    let noes: Vec<(usize, usize)> = p.no.search_all(t).iter().map(PyMatch::span).collect();
    let affirmation = p.yes.search_all(t).iter().any(|m| affirmed(t, m, &words));
    let negations: Vec<(usize, usize)> = words
        .iter()
        .copied()
        .filter(|&(s, e)| is_negation(&t[s..e]))
        .collect();
    if affirmation && noes.is_empty() && negations.is_empty() {
        return Some(true);
    }
    if !noes.is_empty()
        && !affirmation
        && !p.no_idiom.is_match(t)
        && !negation_before_no(t, &negations, &noes)
    {
        return Some(false);
    }
    None
}

/// Python's `_plain_number`: ASCII digits, with commas only as thousands separators ("1,128";
/// not "11,28", "1,1280" or "1,,128").
fn plain_number(digits: &str) -> bool {
    let groups: Vec<&str> = digits.split(',').collect();
    if !groups
        .iter()
        .all(|g| !g.is_empty() && g.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    groups.len() == 1 || (groups[0].len() <= 3 && groups[1..].iter().all(|g| g.len() == 3))
}

/// Python's `_stands_alone`: the number at `t[start..]` (its match ends at `end`) is a whole
/// token, not part of a longer number or a range. The character before it is a space, the start
/// or one of [`NUMBER_OPENERS`]; no digit is joined to it on the left by spaces alone ("1 128"),
/// by one of [`NUMBER_JOINERS`] ("200-400", "200, 400") or by one of [`JOIN_WORDS_BEFORE`] ("200 to
/// 400 amps"), nor on the right by one of [`JOIN_WORDS_AFTER`], unless that digit starts another
/// unit match (in `starts`, sorted), which the caller compares ("200 amps or 400").
fn stands_alone(t: &str, start: usize, end: usize, starts: &[usize]) -> bool {
    if let Some(c) = char_before(t, start) {
        if !(py::is_space(c) || NUMBER_OPENERS.contains(c)) {
            return false;
        }
    }
    let mut k = skip_space_back(t, start);
    match char_before(t, k) {
        Some(c) if NUMBER_JOINERS.contains(c) => k = skip_space_back(t, k - c.len_utf8()),
        _ => {
            let mut j = k;
            while let Some(c) = char_before(t, j).filter(|&c| is_word(c)) {
                j -= c.len_utf8();
            }
            if listed(&JOIN_WORDS_BEFORE, &t[j..k]) {
                k = skip_space_back(t, j);
            }
        }
    }
    if char_before(t, k).is_some_and(py::is_decimal) {
        return false;
    }
    let mut i = skip_space_fwd(t, end);
    let mut j = i;
    while let Some(c) = char_at(t, j).filter(|&c| is_word(c)) {
        j += c.len_utf8();
    }
    if i > end && listed(&JOIN_WORDS_AFTER, &t[i..j]) {
        i = skip_space_fwd(t, j);
        if char_at(t, i).is_some_and(py::is_decimal) && starts.binary_search(&i).is_err() {
            return false;
        }
    }
    true
}

/// Python's `_quantity`: the one value every unit match states; `None` if any match is not a
/// whole plain number, if the matches disagree, or if the value is zero. Group 1 (the digits)
/// starts each match.
fn quantity(re: &PyRegex, t: &str) -> Option<i64> {
    let matches = re.search_all(t);
    let starts: Vec<usize> = matches.iter().map(|m| m.span().0).collect();
    let mut values: Vec<i64> = Vec::new();
    for m in &matches {
        let digits = m.group(1)?;
        let (start, end) = m.span();
        if !(plain_number(digits) && stands_alone(t, start, end, &starts)) {
            return None;
        }
        let n: i64 = digits.replace(',', "").parse().ok()?;
        if !values.contains(&n) {
            values.push(n);
        }
        if values.len() > 1 {
            return None;
        }
    }
    match values.as_slice() {
        [n] if *n != 0 => Some(*n),
        _ => None,
    }
}

/// `extract(question, answer)`: one finding, value `null` ("unclear") unless the answer clearly
/// states one.
///
/// - Typographic apostrophes ([`APOSTROPHES`]) are read as `'`; any hedge ("not sure", "would
///   have to check", ...) leaves every claim unclear.
/// - `tenant_subpanel_amps` (unit `A`) and `usable_area_sqft` (unit `sqft`, commas dropped): every
///   unit match must be a whole plain number ([`plain_number`], [`stands_alone`]), all of them the
///   same non-zero value; otherwise unclear. No other bounds than the patterns' own (amps 2 to 4
///   digits, area 3 to 7 characters).
/// - `in_unit_plumbing`, `hvac_zonable`: `true` only for an affirmation that is not negated
///   ([`affirmed`]), no NO match and no negation word anywhere; `false` only for a NO match with no
///   un-negated affirmation, no "no problem"-style idiom and no double negative
///   ([`negation_before_no`]); otherwise unclear.
/// - any other claim: the stripped answer's first 200 characters (as given), or `null` when empty.
pub fn extract(question: &CallQuestion, answer: &str) -> StructuredFinding {
    let p = patterns();
    let text: String = answer
        .chars()
        .map(|c| if APOSTROPHES.contains(&c) { '\'' } else { c })
        .collect();
    let mut value = Value::Null;
    let mut unit: Option<String> = None;
    if !p.unsure.is_match(&text) {
        match question.claim_type.as_str() {
            "tenant_subpanel_amps" => {
                if let Some(n) = quantity(&p.amps, &text) {
                    value = Value::from(n);
                    unit = Some("A".into());
                }
            }
            "usable_area_sqft" => {
                if let Some(n) = quantity(&p.sqft, &text) {
                    value = Value::from(n);
                    unit = Some("sqft".into());
                }
            }
            "in_unit_plumbing" | "hvac_zonable" => {
                if let Some(b) = yes_no(p, &text) {
                    value = Value::Bool(b);
                }
            }
            _ => {
                let s = py::prefix_chars(py::strip(answer), 200);
                if !s.is_empty() {
                    value = Value::String(s.to_owned());
                }
            }
        }
    }
    StructuredFinding {
        question: question.question.clone(),
        answer: Some(answer.to_owned()),
        claim_type: Some(question.claim_type.clone()),
        value,
        unit,
        speaker: Some("counterparty".into()),
        transcript_offset_s: None,
        classification: "OWNER_REPORTED".into(),
    }
}

/// `describe(finding)`: `"<claim_type>: unclear"`, `": yes"` / `": no"` for booleans, otherwise
/// `str(value)` followed by `" <unit>"` when the unit is non-empty. A missing claim type prints
/// as `None`, as in Python.
pub fn describe(f: &StructuredFinding) -> String {
    let claim = f.claim_type.as_deref().unwrap_or("None");
    match &f.value {
        Value::Null => format!("{claim}: unclear"),
        Value::Bool(b) => format!("{claim}: {}", if *b { "yes" } else { "no" }),
        v => {
            let unit = match f.unit.as_deref() {
                Some(u) if !u.is_empty() => format!(" {u}"),
                _ => String::new(),
            };
            format!("{claim}: {}{unit}", py::value_str(v))
        }
    }
}
