//! Port of `apps/api/app/domains/logistics/timeline.py`: the opening timeline. Deterministic
//! milestone offsets, labelled `placeholder offset`, until procurement lead times and permit
//! durations are integrated.

use chrono::{Datelike, Days, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::DomainError;

/// `MILESTONES`: `(milestone, days after the start date)`.
pub const MILESTONES: &[(&str, u64)] = &[
    ("lease_signed", 0),
    ("permits_submitted", 14),
    ("permits_issued", 56),
    ("construction_complete", 98),
    ("ffe_delivered", 105),
    ("opening", 120),
];

/// One milestone: `{"milestone", "target_date", "basis"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub milestone: String,
    /// ISO date (`YYYY-MM-DD`).
    pub target_date: String,
    pub basis: String,
}

/// `opening_timeline(start)`. `Err` where Python's `date` arithmetic overflows (past
/// 9999-12-31).
pub fn opening_timeline(start: NaiveDate) -> Result<Vec<Milestone>, DomainError> {
    MILESTONES
        .iter()
        .map(|(k, d)| {
            let day = start
                .checked_add_days(Days::new(*d))
                .filter(|x| x.year() <= 9999)
                .ok_or_else(|| DomainError::Value("date value out of range".into()))?;
            Ok(Milestone {
                milestone: (*k).to_owned(),
                target_date: day.format("%Y-%m-%d").to_string(),
                basis: "placeholder offset".into(),
            })
        })
        .collect()
}
