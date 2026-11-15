//! Two things the voice assistant may do on a call besides asking questions: book an in-person
//! tour inside the owner's times, and negotiate the rent inside the owner's limits.
//!
//! Pure rules: no I/O, no clock (the caller passes `now`), no store. **The model never decides a
//! time or a number itself.** Before it proposes, accepts or counters it calls a tool; the server
//! answers here from what the owner approved for this call; the model says the line returned.
//! Nothing here is ever an agreement on rent: a number inside the limits goes to the owner as a
//! decision (`needs_owner_decision: true`). A tour is a visit, never a commitment to lease.
//!
//! The owner's maximum, target and window list never appear in the instructions; the tools return
//! them one step at a time, and no text for the model contains the maximum unless the maximum is
//! itself the counter being made (or the counterparty said that number).
//!
//! The maximum cannot be found by probing either. A counterparty number gets a verdict (a counter,
//! a decline, or "I'll bring that to the owner") only until the first number is taken to the owner
//! and only for the first `2 * max_counters + 2` numbers of the call. Every later number, whatever
//! it is, gets one neutral line; the owner's records still say which numbers were inside the limit.

use chrono::{DateTime, Duration, FixedOffset, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::response_agent::{format_money, redact};

pub const TOOL_CHECK_TOUR: &str = "check_tour_time";
pub const TOOL_EVALUATE_RENT: &str = "evaluate_rent";
pub const MAX_WINDOWS: usize = 10;
pub const MAX_TEXT_CHARS: usize = 200;
pub const MAX_DAYS_AHEAD: i64 = 60;
pub const MAX_COUNTERS: i64 = 5;
pub const DEFAULT_TOUR_MINUTES: i64 = 30;
pub const DEFAULT_COUNTERS: i64 = 3;
/// Terms the assistant may put forward as arguments, when the owner lists them.
pub const MAY_MENTION: &[&str] = &["longer_lease_term", "quick_move_in", "good_tenant_history"];
/// Intermediate counter steps are rounded down to this many cents ($25).
pub const COUNTER_ROUNDING_CENTS: i64 = 2_500;
/// The largest monthly rent the tool accepts ($10,000,000).
pub const MAX_RENT_CENTS: i64 = 1_000_000_000;
const MAX_EVENTS: usize = 32;
/// The largest value of `2 * max_counters + 2`: the event list never forgets this many
/// counterparty numbers (see [`push_event`]).
const MAX_PROBES: usize = (2 * MAX_COUNTERS + 2) as usize;
/// The one line for a counterparty number that gets no verdict.
const NEUTRAL_LINE: &str = "I'll pass that along as well.";

// ---- What the owner approved (the request) ----------------------------------------------------

/// What the owner approved for this call. Both optional: an old request means neither.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CallPurposes {
    pub tour: Option<TourRequest>,
    pub negotiation: Option<NegotiationLimits>,
}

/// A window of local time with the property's UTC offset, e.g. `2026-11-17T15:00:00-08:00`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeWindow {
    pub start: DateTime<FixedOffset>,
    pub end: DateTime<FixedOffset>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TourRequest {
    pub windows: Vec<TimeWindow>,
    pub duration_minutes: i64,
    /// Who will come: the owner's name as the profile has it (the owner's name when absent).
    pub attendee_name: Option<String>,
    pub notes: Option<String>,
}

impl Default for TourRequest {
    fn default() -> Self {
        Self {
            windows: Vec::new(),
            duration_minutes: DEFAULT_TOUR_MINUTES,
            attendee_name: None,
            notes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NegotiationLimits {
    pub target_monthly_rent_cents: Option<i64>,
    pub max_monthly_rent_cents: Option<i64>,
    pub max_counters: i64,
    pub may_mention: Vec<String>,
}

impl Default for NegotiationLimits {
    fn default() -> Self {
        Self {
            target_monthly_rent_cents: None,
            max_monthly_rent_cents: None,
            max_counters: DEFAULT_COUNTERS,
            may_mention: Vec::new(),
        }
    }
}

// ---- Validated (server side only; never shown to the model) -----------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovedTour {
    /// The owner's windows in the owner's order (`window_index` refers to this order).
    pub windows: Vec<TimeWindow>,
    pub duration_minutes: i64,
    pub attendee: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovedNegotiation {
    pub target_cents: i64,
    pub max_cents: i64,
    pub max_counters: i64,
    pub may_mention: Vec<String>,
    /// The counter schedule from the target toward the maximum (see [`counter_schedule`]).
    pub schedule: Vec<i64>,
}

/// What may actually be done on this call, after validation. A purpose with invalid limits is
/// refused whole (`refusals` says why, in plain words, for the owner).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Approved {
    pub owner_name: String,
    pub tour: Option<ApprovedTour>,
    pub negotiation: Option<ApprovedNegotiation>,
    pub refusals: Vec<String>,
}

/// Free text from the request, made safe for one sentence: control characters, newlines and
/// invisible format characters become spaces or vanish, double quotes become single quotes,
/// whitespace collapses, at most [`MAX_TEXT_CHARS`] characters, then [`redact`].
pub fn clean_text(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in text.chars().take(MAX_TEXT_CHARS * 4) {
        let c = match c {
            '"' | '\u{201C}' | '\u{201D}' => '\'',
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}' => continue,
            c if c.is_control() || c.is_whitespace() => ' ',
            c => c,
        };
        if c == ' ' {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    let cut: String = out.chars().take(MAX_TEXT_CHARS).collect();
    redact(cut.trim()).0
}

/// The counter schedule: `max_counters` steps evenly from the target to the maximum, both ends
/// included (one counter: the target only). Intermediate steps are rounded down to $25, never
/// below the step before. For 3: target, halfway, maximum.
pub fn counter_schedule(target: i64, max: i64, counters: i64) -> Vec<i64> {
    let n = counters.clamp(0, MAX_COUNTERS);
    if n == 0 {
        return Vec::new();
    }
    if n == 1 || target == max {
        return vec![target];
    }
    let mut out: Vec<i64> = Vec::new();
    for i in 0..n {
        let raw = if i == n - 1 {
            max
        } else {
            let step = target as i128 + (max - target) as i128 * i as i128 / (n - 1) as i128;
            let step = step as i64;
            (step - step.rem_euclid(COUNTER_ROUNDING_CENTS)).max(target)
        };
        let prev = out.last().copied().unwrap_or(target);
        let v = raw.max(prev).min(max);
        if out.last() != Some(&v) {
            out.push(v);
        }
    }
    out
}

/// Validate what the owner approved. `monthly_rent_limit_cents` is the business profile's limit
/// when it has one.
pub fn approve(
    p: &CallPurposes,
    owner_name: &str,
    monthly_rent_limit_cents: Option<i64>,
) -> Approved {
    let owner = {
        let o = clean_text(owner_name);
        if o.is_empty() {
            "the owner".to_string()
        } else {
            o
        }
    };
    let mut out = Approved {
        owner_name: owner.clone(),
        ..Default::default()
    };
    if let Some(t) = &p.tour {
        let mut why = Vec::new();
        if t.windows.is_empty() {
            why.push("no tour times were given".to_string());
        }
        if t.windows.len() > MAX_WINDOWS {
            why.push(format!(
                "{} tour windows were given; at most {MAX_WINDOWS} are allowed",
                t.windows.len()
            ));
        }
        if !(15..=240).contains(&t.duration_minutes) {
            why.push("a tour must last 15 to 240 minutes".into());
        }
        if why.is_empty() {
            let d = Duration::minutes(t.duration_minutes);
            for (i, w) in t.windows.iter().enumerate() {
                if w.end <= w.start || w.end - w.start < d {
                    why.push(format!("tour window {} is shorter than the tour", i + 1));
                }
            }
        }
        let attendee = t
            .attendee_name
            .as_deref()
            .map(clean_text)
            .filter(|s| !s.is_empty());
        if why.is_empty() {
            out.tour = Some(ApprovedTour {
                windows: t.windows.clone(),
                duration_minutes: t.duration_minutes,
                attendee: attendee.unwrap_or_else(|| owner.clone()),
                notes: t.notes.as_deref().map(clean_text).filter(|s| !s.is_empty()),
            });
        } else {
            out.refusals.push(format!(
                "The assistant will not book a tour: {}.",
                why.join("; ")
            ));
        }
    }
    if let Some(n) = &p.negotiation {
        let mut why = Vec::new();
        match (n.target_monthly_rent_cents, n.max_monthly_rent_cents) {
            (None, _) => why.push("the target rent is missing".to_string()),
            (_, None) => why.push("the maximum rent is missing".to_string()),
            (Some(t), Some(m)) => {
                if t <= 0 {
                    why.push("the target rent must be above zero".into());
                }
                if t > m {
                    why.push("the target rent is above the maximum".into());
                }
                if m > MAX_RENT_CENTS {
                    why.push("the maximum rent is too large".into());
                }
                if let Some(limit) = monthly_rent_limit_cents {
                    if m > limit {
                        why.push(format!(
                            "the maximum rent is above the profile's monthly limit of {}",
                            format_money(limit, "USD")
                        ));
                    }
                }
            }
        }
        if !(0..=MAX_COUNTERS).contains(&n.max_counters) {
            why.push(format!("counter-offers must be 0 to {MAX_COUNTERS}"));
        }
        for m in &n.may_mention {
            if !MAY_MENTION.contains(&m.as_str()) {
                why.push(format!(
                    "'{}' is not a term the assistant may mention",
                    clean_text(m)
                ));
            }
        }
        match (n.target_monthly_rent_cents, n.max_monthly_rent_cents) {
            (Some(t), Some(m)) if why.is_empty() => {
                out.negotiation = Some(ApprovedNegotiation {
                    target_cents: t,
                    max_cents: m,
                    max_counters: n.max_counters,
                    may_mention: n.may_mention.clone(),
                    schedule: counter_schedule(t, m, n.max_counters),
                });
            }
            _ => out.refusals.push(format!(
                "The assistant will not negotiate the rent: {}.",
                why.join("; ")
            )),
        }
    }
    out
}

// ---- Tools ------------------------------------------------------------------------------------

/// One function tool; converts field for field to `ss_server::integrations::xai_realtime::FunctionTool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Only the tools for what was approved; none when nothing was.
pub fn tools(p: &Approved) -> Vec<ToolDef> {
    let mut out = Vec::new();
    if p.tour.is_some() {
        out.push(ToolDef {
            name: TOOL_CHECK_TOUR.into(),
            description: "Call this every time the other person proposes a tour time, before you \
                          offer one, when they want to move a tour already set, and when none of \
                          the times you offered work for them. Then say exactly the `say` text it \
                          returns. Never state, offer or accept a tour time this tool has not just \
                          returned."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "start": {"type": "string", "description": "The proposed start as an ISO local date-time, e.g. 2026-11-17T15:00. Leave out only when none of the offered times work."},
                    "duration_minutes": {"type": "integer", "description": "How long they want the visit to be, if they said."},
                    "moving_booked_tour": {"type": "boolean", "description": "True when they want to move the tour already set."},
                    "none_of_offered_times_work": {"type": "boolean", "description": "True when they turned down every time you offered."}
                },
                "required": []
            }),
        });
    }
    if p.negotiation.is_some() {
        out.push(ToolDef {
            name: TOOL_EVALUATE_RENT.into(),
            description: "Call this before you state any rent number, and every time the other \
                          person states a rent number or a counter-offer. Then say exactly the \
                          `say` text it returns. Never state a number this tool has not just \
                          returned, and never agree to any number: a number they offer goes to \
                          the owner."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "monthly_rent_dollars": {"type": "number", "description": "The amount in US dollars, as said."},
                    "said_by": {"type": "string", "enum": ["counterparty", "assistant"], "description": "counterparty: they said it; assistant: you want to say it."},
                    "is_final_offer": {"type": "boolean", "description": "True when they said it is their final or best number."},
                    "kind": {"type": "string", "enum": ["monthly_rent", "annual_rent", "per_square_foot", "deposit", "fee", "other"], "description": "What the amount is. Default monthly_rent."}
                },
                "required": ["monthly_rent_dollars", "said_by"]
            }),
        });
    }
    out
}

// ---- State ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaidBy {
    Counterparty,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Booking {
    pub start: DateTime<FixedOffset>,
    pub end: DateTime<FixedOffset>,
    pub window_index: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PurposeOutcome {
    TourBooked {
        start: DateTime<FixedOffset>,
        end: DateTime<FixedOffset>,
        window_index: usize,
    },
    TourMoved {
        from_start: DateTime<FixedOffset>,
        start: DateTime<FixedOffset>,
        end: DateTime<FixedOffset>,
        window_index: usize,
    },
    /// A follow-up for the owner (not a hold): no window left, or none of the offered times work.
    TourNeedsOwner { reason: String },
    RentWithinLimits {
        amount_cents: i64,
        at_or_under_target: bool,
        needs_owner_decision: bool,
    },
    RentAboveLimits {
        amount_cents: i64,
        lowest_counterparty_cents: i64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TourState {
    pub booked: Option<Booking>,
    pub moved_from: Option<DateTime<FixedOffset>>,
    pub offers_made: u32,
    pub needs_owner: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentEvent {
    pub said_by: SaidBy,
    pub amount_cents: i64,
    /// "asked", "opened", "countered", "repeated".
    pub what: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NegotiationState {
    pub highest_offered: Option<i64>,
    pub counters_made: i64,
    pub last_counterparty: Option<i64>,
    pub lowest_counterparty: Option<i64>,
    /// The latest counterparty number at or under the maximum (taken to the owner).
    pub with_owner: Option<i64>,
    pub declined: Option<i64>,
    pub at_or_under_target: bool,
    /// The latest outcome to report (within or above the limits).
    pub outcome: Option<PurposeOutcome>,
    pub events: Vec<RentEvent>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PurposeState {
    pub tour: TourState,
    pub negotiation: NegotiationState,
}

// ---- Tour decisions ---------------------------------------------------------------------------

/// The `check_tour_time` arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TourAsk {
    pub start: Option<String>,
    pub duration_minutes: Option<i64>,
    pub moving_booked_tour: bool,
    pub none_of_offered_times_work: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TourKind {
    Book,
    Move,
    OfferTimes,
    AskOwner,
    AlreadySet,
    AskAgain,
    NotApproved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TourDecision {
    pub decision: TourKind,
    /// The exact line to say, when there is one.
    pub say: Option<String>,
    /// What the model does next.
    pub instruction: String,
    pub offered: Vec<DateTime<FixedOffset>>,
    pub outcome: Option<PurposeOutcome>,
}

/// "Tuesday 17 November, 3:00 PM": weekday, date and time on the clock of the time's own offset
/// (the window's, which is the property's). Every line that uses it adds "local time"; a numeric
/// offset is never printed for a person.
pub fn spoken_time(t: &DateTime<FixedOffset>) -> String {
    t.format("%A %-d %B, %-I:%M %p").to_string()
}

fn fits(w: &TimeWindow, start: DateTime<FixedOffset>, d: Duration) -> bool {
    start >= w.start && start + d <= w.end
}

/// An ISO local date-time, read with the offset of the first window it fits, else with the offset
/// of the window nearest in local time. An offset the model adds ("Z", "-05:00") is ignored: the
/// counterparty speaks the property's local time, and the line said back must name that time.
fn parse_start(s: &str, windows: &[TimeWindow], d: Duration) -> Option<DateTime<FixedOffset>> {
    let s = s.trim();
    if s.is_empty() || s.len() > 40 {
        return None;
    }
    let naive = match DateTime::parse_from_rfc3339(s) {
        Ok(t) => Some(t.naive_local()),
        Err(_) => None,
    };
    let naive = naive.or_else(|| {
        [
            "%Y-%m-%dT%H:%M:%S",
            "%Y-%m-%dT%H:%M",
            "%Y-%m-%d %H:%M:%S",
            "%Y-%m-%d %H:%M",
        ]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
    })?;
    // First the windows whose local clock range holds the time, with their own offsets (a window
    // that spans a daylight-saving change has two).
    for w in windows {
        if w.start.naive_local() <= naive && naive + d <= w.end.naive_local() {
            for o in [*w.start.offset(), *w.end.offset()] {
                if let Some(t) = o.from_local_datetime(&naive).single() {
                    if fits(w, t, d) {
                        return Some(t);
                    }
                }
            }
        }
    }
    let nearest = windows
        .iter()
        .min_by_key(|w| (w.start.naive_local() - naive).num_seconds().abs())
        .map(|w| *w.start.offset())?;
    nearest.from_local_datetime(&naive).single()
}

/// Up to three still-open slots, one per window in time order: the window's start, or the next
/// half hour after `now` in the window's own offset.
fn open_slots(
    t: &ApprovedTour,
    booked: Option<&Booking>,
    now: DateTime<FixedOffset>,
    d: Duration,
) -> Vec<DateTime<FixedOffset>> {
    let mut ws: Vec<&TimeWindow> = t.windows.iter().collect();
    ws.sort_by_key(|w| w.start);
    let limit = now + Duration::days(MAX_DAYS_AHEAD);
    let mut out = Vec::new();
    for w in ws {
        let mut s = w.start;
        if s <= now {
            let off = w.start.offset().local_minus_utc() as i64;
            let local = now.timestamp() + off;
            let next = (local.div_euclid(1800) + 1) * 1800 - off;
            match w.start.offset().timestamp_opt(next, 0).single() {
                Some(v) => s = v,
                None => continue,
            }
        }
        if let Some(b) = booked {
            if s < b.end && b.start < s + d {
                s = b.end.max(s);
            }
        }
        if s > now && s + d <= w.end && s <= limit && !out.contains(&s) {
            out.push(s);
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

fn tour_refusal(kind: TourKind, instruction: &str) -> TourDecision {
    TourDecision {
        decision: kind,
        say: None,
        instruction: instruction.into(),
        offered: Vec::new(),
        outcome: None,
    }
}

pub fn decide_tour(
    p: &Approved,
    s: &TourState,
    ask: &TourAsk,
    now: DateTime<FixedOffset>,
) -> (TourDecision, TourState) {
    let Some(t) = &p.tour else {
        return (
            tour_refusal(
                TourKind::NotApproved,
                "This call has no permission to set a tour. Say you will ask the owner.",
            ),
            s.clone(),
        );
    };
    let owner = &p.owner_name;
    let mut st = s.clone();
    let d = Duration::minutes(
        ask.duration_minutes
            .unwrap_or(0)
            .clamp(0, 240)
            .max(t.duration_minutes),
    );
    let ask_owner = |st: &mut TourState, reason: &str| {
        let first = !st.needs_owner;
        st.needs_owner = true;
        TourDecision {
            decision: TourKind::AskOwner,
            say: Some(format!(
                "I'll check with {owner} and get back to you with other times."
            )),
            instruction: "Say the line and move on. Do not offer or accept any other time.".into(),
            offered: Vec::new(),
            outcome: first.then(|| PurposeOutcome::TourNeedsOwner {
                reason: reason.into(),
            }),
        }
    };
    if ask.none_of_offered_times_work {
        let dec = ask_owner(&mut st, "none of the offered times work for them");
        return (dec, st);
    }
    let Some(start) = ask
        .start
        .as_deref()
        .and_then(|x| parse_start(x, &t.windows, d))
    else {
        return (
            tour_refusal(
                TourKind::AskAgain,
                "That date and time could not be read. Ask for the date and time again.",
            ),
            st,
        );
    };
    if start <= now {
        return (
            tour_refusal(
                TourKind::AskAgain,
                "That time has already passed. Ask for another date and time.",
            ),
            st,
        );
    }
    if start > now + Duration::days(MAX_DAYS_AHEAD) {
        return (
            tour_refusal(
                TourKind::AskAgain,
                "That is more than 60 days away. Ask for a date within the next two months.",
            ),
            st,
        );
    }
    if let Some(b) = &st.booked {
        if !ask.moving_booked_tour || b.start == start {
            let when = spoken_time(&b.start);
            return (
                TourDecision {
                    decision: TourKind::AlreadySet,
                    say: Some(format!("We already have a tour set for {when} local time.")),
                    instruction: "A tour is already set. Ask whether they want to move it; only then call again with moving_booked_tour true.".into(),
                    offered: Vec::new(),
                    outcome: None,
                },
                st,
            );
        }
    }
    let when = spoken_time(&start);
    let attendee = &t.attendee;
    if let Some(i) = t.windows.iter().position(|w| fits(w, start, d)) {
        let end = start + d;
        let (kind, say, outcome) = match st.booked.take() {
            Some(b) => {
                st.moved_from = Some(b.start);
                (
                    TourKind::Move,
                    format!("Then let's move it to {when} local time. I'll put that down for {attendee} and let {owner} know."),
                    PurposeOutcome::TourMoved { from_start: b.start, start, end, window_index: i },
                )
            }
            None => (
                TourKind::Book,
                format!("{when} local time works. I'll put that down for {attendee} and let {owner} know."),
                PurposeOutcome::TourBooked { start, end, window_index: i },
            ),
        };
        st.booked = Some(Booking {
            start,
            end,
            window_index: i,
        });
        return (
            TourDecision {
                decision: kind,
                say: Some(say),
                instruction: "Say the line exactly. A tour is a visit only: do not discuss leasing terms, holds or payments for it.".into(),
                offered: Vec::new(),
                outcome: Some(outcome),
            },
            st,
        );
    }
    let slots = open_slots(t, st.booked.as_ref(), now, d);
    if slots.is_empty() {
        let dec = ask_owner(&mut st, "no tour time the owner gave is still open");
        return (dec, st);
    }
    st.offers_made += 1;
    let list: Vec<String> = slots.iter().map(spoken_time).collect();
    let joined = match list.len() {
        1 => list[0].clone(),
        2 => format!("{} or {}", list[0], list[1]),
        _ => format!("{}, {} or {}", list[0], list[1], list[2]),
    };
    (
        TourDecision {
            decision: TourKind::OfferTimes,
            say: Some(format!("That time doesn't suit {attendee}. Could you do {joined}, local time?")),
            instruction: "Say the line. When they pick one, call check_tour_time with it; if none work, call it with none_of_offered_times_work true.".into(),
            offered: slots,
            outcome: None,
        },
        st,
    )
}

// ---- Rent decisions ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmountKind {
    #[default]
    MonthlyRent,
    AnnualRent,
    PerSquareFoot,
    Deposit,
    Fee,
    Other,
}

/// The `evaluate_rent` arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentAsk {
    pub monthly_rent_dollars: f64,
    pub said_by: SaidBy,
    #[serde(default)]
    pub is_final_offer: bool,
    #[serde(default)]
    pub kind: AmountKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RentKind {
    /// The assistant may say this number (its opening, or a number it already offered).
    Say,
    Counter,
    Decline,
    TakeToOwner,
    NotApproved,
    NotRent,
    AskAgain,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentDecision {
    pub decision: RentKind,
    pub say: Option<String>,
    pub instruction: String,
    /// Always false: nothing returned reveals the owner's limits.
    pub reveal: bool,
    pub outcome: Option<PurposeOutcome>,
}

fn rent_line(
    kind: RentKind,
    say: Option<String>,
    instruction: &str,
    outcome: Option<PurposeOutcome>,
) -> RentDecision {
    RentDecision {
        decision: kind,
        say,
        instruction: instruction.into(),
        reveal: false,
        outcome,
    }
}

fn argument(n: &ApprovedNegotiation, i: i64, owner: &str) -> String {
    if n.may_mention.is_empty() {
        return String::new();
    }
    let k = &n.may_mention[(i.max(0) as usize) % n.may_mention.len()];
    match k.as_str() {
        "longer_lease_term" => format!(" {owner} would look at a longer lease term."),
        "quick_move_in" => format!(" {owner} could move in quickly."),
        "good_tenant_history" => format!(" {owner} has a good history as a tenant."),
        _ => String::new(),
    }
}

/// How many counterparty numbers this call has heard (as far as the event list remembers; never
/// under [`MAX_PROBES`] once it has reached it).
fn numbers_heard(st: &NegotiationState) -> usize {
    st.events
        .iter()
        .filter(|e| e.said_by == SaidBy::Counterparty)
        .count()
}

/// After this many counterparty numbers every further one gets the neutral line.
fn probe_limit(n: &ApprovedNegotiation) -> usize {
    (2 * n.max_counters.clamp(0, MAX_COUNTERS) + 2) as usize
}

fn push_event(st: &mut NegotiationState, said_by: SaidBy, amount_cents: i64, what: &str) {
    if st.events.len() >= MAX_EVENTS {
        // The probing bound counts the counterparty's numbers in this list, so the count must not
        // fall back under the bound: while few of theirs are held, the oldest of ours goes first.
        let drop = if numbers_heard(st) <= MAX_PROBES {
            st.events
                .iter()
                .position(|e| e.said_by == SaidBy::Assistant)
                .unwrap_or(0)
        } else {
            0
        };
        st.events.remove(drop);
    }
    st.events.push(RentEvent {
        said_by,
        amount_cents,
        what: what.into(),
    });
}

pub fn decide_rent(
    p: &Approved,
    s: &NegotiationState,
    ask: &RentAsk,
    _now: DateTime<FixedOffset>,
) -> (RentDecision, NegotiationState) {
    let Some(n) = &p.negotiation else {
        return (
            rent_line(
                RentKind::NotApproved,
                None,
                "This call has no permission to discuss rent numbers. Do not state a number; say you will pass what they said to the owner.",
                None,
            ),
            s.clone(),
        );
    };
    let owner = &p.owner_name;
    let mut st = s.clone();
    if ask.kind != AmountKind::MonthlyRent {
        return (
            rent_line(
                RentKind::NotRent,
                None,
                "That amount is not a monthly rent. Ask what the monthly rent would be. Do not state or discuss any other amount; deposits, fees and holds are for the owner.",
                None,
            ),
            st,
        );
    }
    let d = ask.monthly_rent_dollars;
    if !d.is_finite() || d <= 0.0 || d * 100.0 > MAX_RENT_CENTS as f64 {
        return (
            rent_line(
                RentKind::AskAgain,
                None,
                "That number could not be used. Ask for the monthly rent again.",
                None,
            ),
            st,
        );
    }
    let cents = (d * 100.0).round() as i64;
    if cents <= 0 {
        return (
            rent_line(
                RentKind::AskAgain,
                None,
                "That number could not be used. Ask for the monthly rent again.",
                None,
            ),
            st,
        );
    }
    let money = |c: i64| format_money(c, "USD");
    match ask.said_by {
        SaidBy::Counterparty => {
            // No verdict once a number has gone to the owner, or after the call's share of
            // numbers: otherwise the answers ("to the owner" up to the maximum, a counter or a
            // decline above it) would let twenty numbers find the maximum to the cent.
            let neutral = st.with_owner.is_some() || numbers_heard(&st) >= probe_limit(n);
            push_event(&mut st, SaidBy::Counterparty, cents, "asked");
            st.last_counterparty = Some(cents);
            st.lowest_counterparty = Some(st.lowest_counterparty.map_or(cents, |l| l.min(cents)));
            if neutral {
                // For the owner only: the best (lowest) number inside the limit is the one kept
                // as their decision; nothing said or returned depends on which side this one is.
                if cents <= n.max_cents {
                    let best = st.with_owner.map_or(cents, |w| w.min(cents));
                    st.with_owner = Some(best);
                    st.declined = None;
                    st.at_or_under_target = best <= n.target_cents;
                    st.outcome = Some(PurposeOutcome::RentWithinLimits {
                        amount_cents: best,
                        at_or_under_target: best <= n.target_cents,
                        needs_owner_decision: true,
                    });
                } else if st.with_owner.is_none() {
                    st.declined = Some(cents);
                    st.outcome = Some(PurposeOutcome::RentAboveLimits {
                        amount_cents: cents,
                        lowest_counterparty_cents: st.lowest_counterparty.unwrap_or(cents),
                    });
                }
                return (
                    rent_line(
                        RentKind::TakeToOwner,
                        Some(NEUTRAL_LINE.to_string()),
                        "Say the line exactly. Do not say or imply that the number is accepted, refused, high or low. Do not state any number.",
                        None,
                    ),
                    st,
                );
            }
            if cents <= n.max_cents {
                st.with_owner = Some(cents);
                st.declined = None;
                st.at_or_under_target = cents <= n.target_cents;
                let say = format!(
                    "Thank you. I can't decide that myself: I'll bring {} a month to {owner} and get back to you today.",
                    money(cents)
                );
                let outcome = PurposeOutcome::RentWithinLimits {
                    amount_cents: cents,
                    at_or_under_target: cents <= n.target_cents,
                    needs_owner_decision: true,
                };
                st.outcome = Some(outcome.clone());
                return (
                    rent_line(
                        RentKind::TakeToOwner,
                        Some(say),
                        "Say the line exactly. Do not say or imply that the number is accepted. Do not state any other number.",
                        Some(outcome),
                    ),
                    st,
                );
            }
            st.with_owner = None;
            st.at_or_under_target = false;
            st.outcome = None;
            let next = n
                .schedule
                .iter()
                .copied()
                .find(|v| st.highest_offered.is_none_or(|h| *v > h));
            match next {
                Some(v)
                    if !ask.is_final_offer && st.counters_made < n.max_counters && v < cents =>
                {
                    st.counters_made += 1;
                    st.highest_offered = Some(v);
                    push_event(&mut st, SaidBy::Assistant, v, "countered");
                    let say = format!(
                        "{owner} is looking at about {} a month. Could you come to that?{}",
                        money(v),
                        argument(n, st.counters_made - 1, owner)
                    );
                    (
                        rent_line(
                            RentKind::Counter,
                            Some(say),
                            "Say the line exactly. Do not state any other number.",
                            None,
                        ),
                        st,
                    )
                }
                _ => {
                    st.declined = Some(cents);
                    st.at_or_under_target = false;
                    let lowest = st.lowest_counterparty.unwrap_or(cents);
                    st.outcome = Some(PurposeOutcome::RentAboveLimits {
                        amount_cents: cents,
                        lowest_counterparty_cents: lowest,
                    });
                    let say = format!(
                        "That is above what {owner} can do. I'll let them know where you are."
                    );
                    (
                        rent_line(
                            RentKind::Decline,
                            Some(say),
                            "Say the line, politely. Do not state another number. You may continue with your other questions.",
                            Some(PurposeOutcome::RentAboveLimits { amount_cents: cents, lowest_counterparty_cents: lowest }),
                        ),
                        st,
                    )
                }
            }
        }
        SaidBy::Assistant => {
            if numbers_heard(&st) >= probe_limit(n) {
                // The same answer whatever their numbers were: see the neutral line above.
                let instr = format!(
                    "Do not state a new number. Say you will pass what they said to {owner}."
                );
                return (rent_line(RentKind::NotApproved, None, &instr, None), st);
            }
            if let Some(w) = st.with_owner {
                let instr = format!(
                    "Do not state a new number. Say you will take {} a month to {owner} and get back to them today.",
                    money(w)
                );
                return (rent_line(RentKind::NotApproved, None, &instr, None), st);
            }
            if st.declined.is_some() {
                return (
                    rent_line(RentKind::NotApproved, None, "Do not state a new number. Say you will let the owner know where they are.", None),
                    st,
                );
            }
            if n.max_counters <= 0 {
                // No counter was approved: the assistant states no number of its own.
                let instr = format!(
                    "Do not state a number. Ask what monthly rent they have in mind; any number they give goes to {owner}."
                );
                return (rent_line(RentKind::NotApproved, None, &instr, None), st);
            }
            let allowed = st.highest_offered.unwrap_or(n.target_cents);
            if cents == allowed {
                let first = st.highest_offered.is_none();
                st.highest_offered = Some(allowed);
                push_event(
                    &mut st,
                    SaidBy::Assistant,
                    allowed,
                    if first { "opened" } else { "repeated" },
                );
                let say = format!("{owner} is looking at about {} a month.", money(allowed));
                return (
                    rent_line(
                        RentKind::Say,
                        Some(say),
                        "Say the line exactly. Do not state any other number.",
                        None,
                    ),
                    st,
                );
            }
            let instr = format!(
                "You may not say that number. You may say {} a month instead, or no number.",
                money(allowed)
            );
            (rent_line(RentKind::NotApproved, None, &instr, None), st)
        }
    }
}

// ---- Dispatch (for the wiring) ----------------------------------------------------------------

/// Run one tool call by name with the model's JSON arguments. Returns the JSON to send back to the
/// model (`decision`, `say`, `instruction`; never the limits) and the new state.
pub fn dispatch(
    p: &Approved,
    s: &PurposeState,
    name: &str,
    args: &Value,
    now: DateTime<FixedOffset>,
) -> (Value, PurposeState) {
    let mut st = s.clone();
    let bad = |m: &str| json!({"decision": "ask_again", "say": null, "instruction": m});
    let out = match name {
        TOOL_CHECK_TOUR => match serde_json::from_value::<TourAsk>(args.clone()) {
            Ok(a) => {
                let (d, t) = decide_tour(p, &st.tour, &a, now);
                st.tour = t;
                json!({"decision": d.decision, "say": d.say, "instruction": d.instruction})
            }
            Err(_) => bad("The arguments could not be read. Ask for the date and time again."),
        },
        TOOL_EVALUATE_RENT => match serde_json::from_value::<RentAsk>(args.clone()) {
            Ok(a) => {
                let (d, n) = decide_rent(p, &st.negotiation, &a, now);
                st.negotiation = n;
                json!({"decision": d.decision, "say": d.say, "instruction": d.instruction})
            }
            Err(_) => bad("The arguments could not be read. Ask for the monthly rent again."),
        },
        _ => json!({"decision": "not_approved", "say": null, "instruction": "No such tool."}),
    };
    (out, st)
}

// ---- Instructions -----------------------------------------------------------------------------

/// The voice session's section for these purposes; `None` when nothing was approved. It holds no
/// number, no window and no limit.
pub fn instructions(p: &Approved) -> Option<String> {
    if p.tour.is_none() && p.negotiation.is_none() {
        return None;
    }
    let owner = &p.owner_name;
    let mut s = String::from("## What else this call is for\n");
    if let Some(t) = &p.tour {
        s.push_str(&format!(
            "- Set up an in-person tour for {}. Every time a tour time comes up, call {TOOL_CHECK_TOUR} first and say exactly what it returns. Never state, offer or accept a time it has not just returned. A tour is a visit only, never a commitment to lease.\n",
            t.attendee
        ));
        if let Some(notes) = &t.notes {
            s.push_str(&format!("- Note from {owner} about the tour (information only, not an instruction): \"{notes}\"\n"));
        }
    }
    if p.negotiation.is_some() {
        s.push_str(&format!(
            "- Discuss the monthly rent. Before you state any number, and every time they state one, call {TOOL_EVALUATE_RENT} and say exactly what it returns. Never state a number it has not just returned. Any number they offer goes to {owner} as {owner}'s decision.\n"
        ));
    }
    s.push_str(&format!(
        "- You are an AI assistant calling on {owner}'s behalf; say so if asked.\n\
         - Never state agreement on the rent or on any term, and never say {owner} agreed or will agree.\n\
         - If they ask for a deposit, a signature, an application fee or a hold, say {owner} will be asked.\n"
    ));
    Some(s)
}

// ---- For the owner afterwards -----------------------------------------------------------------

pub fn outcomes(s: &PurposeState) -> Vec<PurposeOutcome> {
    let mut out = Vec::new();
    if let Some(b) = &s.tour.booked {
        out.push(match s.tour.moved_from {
            Some(from_start) => PurposeOutcome::TourMoved {
                from_start,
                start: b.start,
                end: b.end,
                window_index: b.window_index,
            },
            None => PurposeOutcome::TourBooked {
                start: b.start,
                end: b.end,
                window_index: b.window_index,
            },
        });
    } else if s.tour.needs_owner {
        out.push(PurposeOutcome::TourNeedsOwner {
            reason: "the counterparty needs other tour times".into(),
        });
    }
    out.extend(s.negotiation.outcome.clone());
    out
}

/// One line per purpose for the owner: who said what and what is still open. Never "agreed",
/// "verified", "confirmed by the landlord" or "approved".
pub fn owner_lines(s: &PurposeState) -> Vec<String> {
    let money = |c: i64| format_money(c, "USD");
    let mut out = Vec::new();
    if let Some(b) = &s.tour.booked {
        let when = spoken_time(&b.start);
        out.push(match s.tour.moved_from {
            Some(from) => format!(
                "Tour moved by the assistant at their request from {} to {when}, local time (inside your times). Not confirmed in writing.",
                spoken_time(&from)
            ),
            None => format!("Tour set by the assistant for {when} local time (inside your times). Not confirmed in writing."),
        });
    } else if s.tour.needs_owner {
        out.push("They could not make any of your tour times; they are waiting for other times from you.".into());
    }
    let n = &s.negotiation;
    if !n.events.is_empty() {
        let mut parts = Vec::new();
        // Only the last eight are shown. When earlier ones are left out the line says so, and the
        // first number shown is not called their opening ask unless it was.
        let skip = n.events.len().saturating_sub(8);
        let mut seen_counterparty = n.events.len() >= MAX_EVENTS
            || n.events[..skip]
                .iter()
                .any(|e| e.said_by == SaidBy::Counterparty);
        for e in &n.events[skip..] {
            parts.push(match (e.said_by, e.what.as_str()) {
                (SaidBy::Counterparty, _) if !seen_counterparty => {
                    seen_counterparty = true;
                    format!("they asked {}", money(e.amount_cents))
                }
                (SaidBy::Counterparty, _) => format!("they came to {}", money(e.amount_cents)),
                (SaidBy::Assistant, "opened") => {
                    format!("the assistant opened at {}", money(e.amount_cents))
                }
                (SaidBy::Assistant, "repeated") => {
                    format!("the assistant repeated {}", money(e.amount_cents))
                }
                (SaidBy::Assistant, _) => {
                    format!("the assistant countered {}", money(e.amount_cents))
                }
            });
        }
        let mut line = parts.join("; ");
        if let Some(first) = line.get(..1) {
            line = first.to_uppercase() + &line[1..];
        }
        if skip > 0 {
            line = format!("Earlier numbers are not shown. {line}");
        }
        if let Some(w) = n.with_owner {
            let side = if n.at_or_under_target {
                "at or under your target"
            } else {
                "inside your limit"
            };
            match n.last_counterparty {
                // They named a higher number after it: the lowest stays the one to decide on.
                Some(last) if last != w => line.push_str(&format!(
                    ". Their lowest number, {}, is {side}; their latest number is {}",
                    money(w),
                    money(last)
                )),
                _ => line.push_str(&format!(": {side}")),
            }
            line.push_str(". Nothing is agreed. Your decision.");
        } else if let Some(last) = n.declined {
            let lowest = n.lowest_counterparty.unwrap_or(last);
            line.push_str(&if lowest >= last {
                format!(
                    ". They would not go under {}: above your limit. Nothing is agreed.",
                    money(last)
                )
            } else {
                format!(
                    ". Their last number, {}, is above your limit. Nothing is agreed.",
                    money(last)
                )
            });
        } else if n.last_counterparty.is_some() {
            line.push_str(
                ". Still open: their latest number is above your limit. Nothing is agreed.",
            );
        } else {
            line.push_str(". Still open: they have not named a number. Nothing is agreed.");
        }
        out.push(line);
    }
    out
}
