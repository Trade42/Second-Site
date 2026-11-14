//! Byte-compatible re-implementations of Python's `json.dumps` output.
//!
//! - [`dumps_canonical`]: `json.dumps(v, sort_keys=True, separators=(",", ":"))` (the approval
//!   `payload_hash` input). Keys are sorted explicitly, so the result does not depend on whether
//!   serde_json's `preserve_order` feature is enabled anywhere in the build.
//! - [`dumps`]: `json.dumps(v)` with the default separators `", "` / `": "` (the SSE `data:`
//!   line and the structured log line). Field order is the serializer's order.
//!
//! Both use `ensure_ascii=True` (non-ASCII as lower-case `\uXXXX`, astral characters as surrogate
//! pairs) and Python's `float.__repr__` for floats.

use std::io;

use serde::Serialize;
use serde_json::ser::{CharEscape, Formatter};
use serde_json::Value;

/// `float.__repr__` (shortest round-trip digits; exponent form when exp < -4 or exp >= 16).
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    // Rust's `{:e}` prints the shortest round-trip digits: "-1.2345e-7", "1e16", "0e0".
    let shortest = format!("{f:e}");
    // When two strings of that length round-trip (the value is an exact decimal tie, e.g.
    // 1682373064852680.25), Python picks the one nearest the exact value, ties to even; `{:e}`
    // may pick the other. Exact formatting to the same digit count rounds half to even, so use
    // it whenever it still round-trips.
    let n = shortest
        .split('e')
        .next()
        .unwrap_or("")
        .bytes()
        .filter(u8::is_ascii_digit)
        .count();
    let exact = format!("{:.*e}", n.saturating_sub(1), f);
    let s = if exact
        .parse::<f64>()
        .is_ok_and(|g| g.to_bits() == f.to_bits())
    {
        exact
    } else {
        shortest
    };
    let (mantissa, exp) = match s.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (s.as_str(), 0),
    };
    let (neg, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        let n = digits.len() as i32;
        if exp < 0 {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        } else if exp + 1 >= n {
            out.push_str(&digits);
            for _ in 0..(exp + 1 - n) {
                out.push('0');
            }
            out.push_str(".0");
        } else {
            let split = (exp + 1) as usize;
            out.push_str(&digits[..split]);
            out.push('.');
            out.push_str(&digits[split..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exp.unsigned_abs()));
    }
    out
}

fn push_escaped_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        push_escaped_char(out, ch);
    }
    out.push('"');
}

fn push_escaped_char(out: &mut String, ch: char) {
    match ch {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{8}' => out.push_str("\\b"),
        '\u{c}' => out.push_str("\\f"),
        ' '..='~' => out.push(ch),
        _ => {
            let mut buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
}

fn number_repr(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else {
        float_repr(n.as_f64().unwrap_or(f64::NAN))
    }
}

fn canonical_into(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_repr(n)),
        Value::String(s) => push_escaped_str(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_into(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            // Python sorts str keys by code point; UTF-8 byte order is the same order.
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push('{');
            for (i, (k, val)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_escaped_str(out, k);
                out.push(':');
                canonical_into(out, val);
            }
            out.push('}');
        }
    }
}

/// `json.dumps(v, sort_keys=True, separators=(",", ":"))`.
pub fn dumps_canonical(v: &Value) -> String {
    let mut out = String::new();
    canonical_into(&mut out, v);
    out
}

/// A serde_json formatter that reproduces `json.dumps` defaults: `", "` and `": "` separators,
/// `ensure_ascii=True`, Python float repr.
#[derive(Default)]
pub struct PyFormatter;

impl Formatter for PyFormatter {
    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }

    fn write_f64<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        w.write_all(float_repr(value).as_bytes())
    }

    fn write_f32<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f32) -> io::Result<()> {
        w.write_all(float_repr(f64::from(value)).as_bytes())
    }

    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut out = String::with_capacity(fragment.len());
        for ch in fragment.chars() {
            push_escaped_char(&mut out, ch);
        }
        w.write_all(out.as_bytes())
    }

    fn write_char_escape<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        escape: CharEscape,
    ) -> io::Result<()> {
        let s: String = match escape {
            CharEscape::Quote => "\\\"".into(),
            CharEscape::ReverseSolidus => "\\\\".into(),
            CharEscape::Solidus => "/".into(),
            CharEscape::Backspace => "\\b".into(),
            CharEscape::FormFeed => "\\f".into(),
            CharEscape::LineFeed => "\\n".into(),
            CharEscape::CarriageReturn => "\\r".into(),
            CharEscape::Tab => "\\t".into(),
            CharEscape::AsciiControl(b) => format!("\\u{:04x}", b),
        };
        w.write_all(s.as_bytes())
    }
}

/// `json.dumps(v)` (default separators, `ensure_ascii=True`).
pub fn dumps<T: Serialize + ?Sized>(v: &T) -> Result<String, serde_json::Error> {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PyFormatter);
    v.serialize(&mut ser)?;
    // The formatter only writes ASCII.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1.5, "1.5"),
            (100.0, "100.0"),
            (123.456, "123.456"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.2345e16, "1.2345e+16"),
            (1e100, "1e+100"),
            (-2.5e-300, "-2.5e-300"),
            (f64::MAX, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (9007199254740993.0, "9007199254740992.0"),
        ];
        for (f, want) in cases {
            assert_eq!(float_repr(f), want, "{f:e}");
        }
    }

    #[test]
    fn float_repr_breaks_decimal_ties_like_python() {
        // Python `repr(float(s))` for doubles whose two shortest round-trip strings are equally
        // close (exact value ends in ...25 / ...75 at the 17th digit). Rust's `{:e}` gives the
        // other string for these (e.g. "1.6823730648526803e15").
        let cases = [
            ("1682373064852680.2", "1682373064852680.2"),
            ("-1425502010969177.2", "-1425502010969177.2"),
            ("26363981746409.312", "26363981746409.312"),
            ("633601810380023.2", "633601810380023.2"),
        ];
        for (text, want) in cases {
            let f: f64 = text.parse().unwrap();
            assert_eq!(float_repr(f), want, "{text}");
        }
    }

    #[test]
    fn canonical_sorts_and_escapes() {
        let v = json!({"b": 1, "a": [true, null, "é😀\n\u{7f}"], "A": {"z": 1.0, "y": -3}});
        assert_eq!(
            dumps_canonical(&v),
            r#"{"A":{"y":-3,"z":1.0},"a":[true,null,"\u00e9\ud83d\ude00\n\u007f"],"b":1}"#
        );
    }

    #[test]
    fn default_separators_and_order() {
        #[derive(Serialize)]
        struct S {
            z: u8,
            a: Vec<f64>,
            s: &'static str,
        }
        let out = dumps(&S {
            z: 1,
            a: vec![1.0, 1e-5],
            s: "\"é\u{1}",
        })
        .unwrap();
        assert_eq!(out, r#"{"z": 1, "a": [1.0, 1e-05], "s": "\"\u00e9\u0001"}"#);
    }
}
