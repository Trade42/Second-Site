//! Money conversion between Nessie's representations and integer minor units.
//!
//! Nessie v2 keeps amounts as integer cents (`amount_cents`, `balance_cents`, ...), which map to
//! minor units one to one. The one exception is a bill's `payment_amount`, a dollar float ("Bills
//! keep a dollar `payment_amount` for compatibility", Nessie house rules). For it:
//!
//! - **cents to dollars** ([`minor_to_dollars`]) writes the exact decimal `d.cc` and parses it to
//!   the nearest `f64`. For `|cents| <= MAX_DOLLAR_MINOR` (15 significant digits) the shortest
//!   representation of that double, which is what serde_json writes, is the same decimal again,
//!   so the JSON body carries exactly `d.cc`. Larger amounts are refused.
//! - **dollars to cents** ([`dollars_to_minor`]) never does float arithmetic. It takes the
//!   shortest decimal representation of the double (Rust's `Display`, which round-trips and never
//!   uses an exponent), and rounds the third and later decimals **half away from zero** (Python's
//!   `ROUND_HALF_UP` on a `Decimal`, the finance engine's `_round_cents` rule). The result says
//!   whether rounding happened, so a sub-cent bill amount is visible rather than silently changed.

use thiserror::Error;

/// Largest magnitude (in cents) that converts to dollars exactly: 15 significant decimal digits,
/// below the 15.95 digits a double holds. That is just under 10 trillion dollars.
pub const MAX_DOLLAR_MINOR: i64 = 999_999_999_999_999;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MoneyError {
    #[error("amount is not a finite number")]
    NotFinite,
    #[error("amount {0} is outside the range this integration converts exactly")]
    OutOfRange(String),
    #[error("amount must not be negative: {0}")]
    Negative(String),
}

/// Result of converting a Nessie dollar amount to cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DollarConversion {
    pub minor: i64,
    /// False when the dollar amount had fractions of a cent and was rounded.
    pub exact: bool,
}

/// Integer cents to the dollar `f64` Nessie's bill `payment_amount` expects.
pub fn minor_to_dollars(minor: i64) -> Result<f64, MoneyError> {
    if minor.unsigned_abs() > MAX_DOLLAR_MINOR.unsigned_abs() {
        return Err(MoneyError::OutOfRange(minor.to_string()));
    }
    let abs = minor.unsigned_abs();
    let sign = if minor < 0 { "-" } else { "" };
    let text = format!("{sign}{}.{:02}", abs / 100, abs % 100);
    let value: f64 = text
        .parse()
        .map_err(|_| MoneyError::OutOfRange(text.clone()))?;
    // Defensive: the bound above guarantees an exact round trip; check it anyway.
    match dollars_to_minor(value) {
        Ok(DollarConversion {
            minor: back,
            exact: true,
        }) if back == minor => Ok(value),
        _ => Err(MoneyError::OutOfRange(text)),
    }
}

/// A Nessie dollar amount to integer cents, rounding half away from zero (see the module docs).
pub fn dollars_to_minor(dollars: f64) -> Result<DollarConversion, MoneyError> {
    if !dollars.is_finite() {
        return Err(MoneyError::NotFinite);
    }
    let text = format!("{dollars}");
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
    };
    let (int_part, frac_part) = match digits.split_once('.') {
        Some((i, f)) => (i, f),
        None => (digits, ""),
    };
    let out_of_range = || MoneyError::OutOfRange(text.clone());
    if int_part.is_empty()
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(out_of_range());
    }
    let whole: i64 = int_part.parse().map_err(|_| out_of_range())?;
    let frac = frac_part.as_bytes();
    let digit = |i: usize| -> i64 { frac.get(i).map_or(0, |b| i64::from(b - b'0')) };
    let cents_part = digit(0) * 10 + digit(1);
    let rest = frac.get(2..).unwrap_or(&[]);
    let round_up = rest.first().is_some_and(|b| *b >= b'5');
    let exact = rest.iter().all(|b| *b == b'0');
    let magnitude = whole
        .checked_mul(100)
        .and_then(|v| v.checked_add(cents_part))
        .and_then(|v| v.checked_add(i64::from(round_up)))
        .ok_or_else(out_of_range)?;
    if magnitude > MAX_DOLLAR_MINOR {
        return Err(out_of_range());
    }
    Ok(DollarConversion {
        minor: if negative { -magnitude } else { magnitude },
        exact,
    })
}

/// Like [`dollars_to_minor`] for amounts that must not be negative (bill amounts).
pub fn non_negative_dollars_to_minor(dollars: f64) -> Result<DollarConversion, MoneyError> {
    let c = dollars_to_minor(dollars)?;
    if c.minor < 0 {
        return Err(MoneyError::Negative(format!("{dollars}")));
    }
    Ok(c)
}

/// `$1,234.56` (and `-$1,234.56`): exact, from integer cents, for human-readable text.
pub fn format_usd(minor: i64) -> String {
    let abs = minor.unsigned_abs();
    let dollars = (abs / 100).to_string();
    let mut grouped = String::with_capacity(dollars.len() + dollars.len() / 3);
    for (i, ch) in dollars.chars().enumerate() {
        if i > 0 && (dollars.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let sign = if minor < 0 { "-" } else { "" };
    format!("{sign}${grouped}.{:02}", abs % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(d: f64) -> (i64, bool) {
        let c = dollars_to_minor(d).unwrap();
        (c.minor, c.exact)
    }

    #[test]
    fn dollars_to_minor_exact_values() {
        assert_eq!(conv(0.0), (0, true));
        assert_eq!(conv(-0.0), (0, true));
        assert_eq!(conv(0.01), (1, true));
        assert_eq!(conv(0.1), (10, true));
        assert_eq!(conv(1.0), (100, true));
        assert_eq!(conv(12.34), (1234, true));
        assert_eq!(conv(6900.0), (690000, true));
        assert_eq!(conv(1234.56), (123456, true));
        assert_eq!(conv(-12.34), (-1234, true));
        // 0.1 + 0.2 is 0.30000000000000004 as a double: shortest repr has a 17th decimal.
        assert_eq!(conv(0.1 + 0.2), (30, false));
    }

    #[test]
    fn dollars_to_minor_rounds_half_away_from_zero() {
        assert_eq!(conv(10.005), (1001, false));
        assert_eq!(conv(10.0049), (1000, false));
        assert_eq!(conv(10.015), (1002, false));
        assert_eq!(conv(10.025), (1003, false));
        assert_eq!(conv(0.005), (1, false));
        assert_eq!(conv(0.004999), (0, false));
        assert_eq!(conv(-10.005), (-1001, false));
        assert_eq!(conv(-0.004), (0, false));
        assert_eq!(conv(2.675), (268, false)); // the classic binary-float trap, decided on the decimal
        assert_eq!(conv(1.0e-7), (0, false));
    }

    #[test]
    fn dollars_to_minor_refuses_non_finite_and_huge() {
        assert_eq!(dollars_to_minor(f64::NAN), Err(MoneyError::NotFinite));
        assert_eq!(dollars_to_minor(f64::INFINITY), Err(MoneyError::NotFinite));
        assert!(matches!(
            dollars_to_minor(1e300),
            Err(MoneyError::OutOfRange(_))
        ));
        assert!(matches!(
            dollars_to_minor(1e13),
            Err(MoneyError::OutOfRange(_))
        ));
        assert_eq!(conv(9_999_999_999_999.99), (MAX_DOLLAR_MINOR, true));
        assert!(matches!(
            non_negative_dollars_to_minor(-0.01),
            Err(MoneyError::Negative(_))
        ));
        assert_eq!(non_negative_dollars_to_minor(-0.0).unwrap().minor, 0);
    }

    #[test]
    fn minor_to_dollars_round_trips_exactly() {
        // Every cent value in a dense low range, and a deterministic spread up to the bound.
        for minor in (0..200_000).chain((0..200_000).map(|i: i64| i * 4_999_999_999 + 7)) {
            let m = minor.min(MAX_DOLLAR_MINOR);
            for v in [m, -m] {
                let d = minor_to_dollars(v).unwrap();
                assert_eq!(
                    dollars_to_minor(d).unwrap(),
                    DollarConversion {
                        minor: v,
                        exact: true
                    }
                );
                // What serde_json writes is the exact decimal (trailing zeros trimmed, ".0" kept).
                let json = serde_json::to_string(&d).unwrap();
                let abs = v.unsigned_abs();
                let sign = if v < 0 && abs != 0 { "-" } else { "" };
                let frac = format!("{:02}", abs % 100);
                let frac = match frac.trim_end_matches('0') {
                    "" => "0",
                    f => f,
                };
                assert_eq!(json, format!("{sign}{}.{frac}", abs / 100), "{v} cents");
            }
        }
        assert_eq!(
            serde_json::to_string(&minor_to_dollars(123456).unwrap()).unwrap(),
            "1234.56"
        );
        assert_eq!(
            serde_json::to_string(&minor_to_dollars(690000).unwrap()).unwrap(),
            "6900.0"
        );
        assert_eq!(
            serde_json::to_string(&minor_to_dollars(1).unwrap()).unwrap(),
            "0.01"
        );
        assert_eq!(
            serde_json::to_string(&minor_to_dollars(MAX_DOLLAR_MINOR).unwrap()).unwrap(),
            "9999999999999.99"
        );
        assert!(matches!(
            minor_to_dollars(MAX_DOLLAR_MINOR + 1),
            Err(MoneyError::OutOfRange(_))
        ));
        assert!(matches!(
            minor_to_dollars(i64::MIN),
            Err(MoneyError::OutOfRange(_))
        ));
    }

    #[test]
    fn usd_formatting() {
        assert_eq!(format_usd(0), "$0.00");
        assert_eq!(format_usd(5), "$0.05");
        assert_eq!(format_usd(123456), "$1,234.56");
        assert_eq!(format_usd(21618350), "$216,183.50");
        assert_eq!(format_usd(100000000), "$1,000,000.00");
        assert_eq!(format_usd(-120000), "-$1,200.00");
        assert_eq!(format_usd(i64::MIN), "-$92,233,720,368,547,758.08");
    }
}
