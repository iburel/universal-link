// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Canonical JSON (RFC 8785, JCS) for the bytes a signature covers
//! (doc/sync-engine.md, section 2): object members sorted by the UTF-16 code
//! units of their names, minimal string escaping, no whitespace.
//!
//! Deliberately the INTEGER PROFILE of the RFC, not all of it: the records
//! this engine signs carry only integers (seq, gen, clocks, timestamps), and
//! JCS number serialization is IEEE-754 - an integer beyond 2^53 - 1 would
//! silently lose precision there. So a float, or an integer outside the safe
//! range, is an ERROR: refused at signing time (nothing we emit contains
//! one) and dropped at verification time like any other malformed record.

use serde_json::Value;

/// The IEEE-754 safe-integer bound, 2^53 - 1: beyond it, JCS's number
/// serialization no longer round-trips and two encoders may disagree.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Debug, PartialEq, Eq)]
pub enum CanonicalError {
    /// A float, or an integer outside the safe range (module header).
    UnsupportedNumber,
}

impl std::fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CanonicalError::UnsupportedNumber => {
                write!(f, "number outside the canonical integer profile")
            }
        }
    }
}

impl std::error::Error for CanonicalError {}

/// Serializes `value` per RFC 8785, integer profile. The output is the exact
/// byte sequence a record signature covers: two conforming encoders produce
/// it identically or not at all.
pub fn to_canonical_json(value: &Value) -> Result<String, CanonicalError> {
    let mut out = String::new();
    write_value(&mut out, value)?;
    Ok(out)
}

fn write_value(out: &mut String, value: &Value) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            // i64/u64 within the safe range only; everything else (floats
            // included, even float-typed whole numbers like 1.0, which JCS
            // would render "1" but serde_json cannot distinguish reliably
            // across arithmetic) is refused.
            let in_range = |v: i64| v.unsigned_abs() <= MAX_SAFE_INTEGER;
            if let Some(v) = n.as_i64().filter(|v| in_range(*v)) {
                out.push_str(&v.to_string());
            } else if let Some(v) = n.as_u64().filter(|v| *v <= MAX_SAFE_INTEGER) {
                out.push_str(&v.to_string());
            } else {
                return Err(CanonicalError::UnsupportedNumber);
            }
        }
        // serde_json's string escaping is exactly JCS's: `"` and `\` escaped,
        // the five short control escapes, `\u00XX` for the other controls,
        // everything else literal UTF-8.
        Value::String(s) => {
            out.push_str(&serde_json::to_string(s).expect("string serialization is infallible"));
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Sorted by the UTF-16 code units of the names - NOT by Unicode
            // code points: a supplementary-plane name (surrogates 0xD8xx)
            // sorts BEFORE U+E000..U+FFFF names, and the two orders disagree
            // exactly there.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(
                    &serde_json::to_string(key).expect("string serialization is infallible"),
                );
                out.push(':');
                write_value(out, &map[key.as_str()])?;
            }
            out.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn objects_sort_by_utf16_code_units_not_code_points() {
        // U+10000 (surrogate pair D800 DC00) must sort BEFORE U+E000: by code
        // points it would sort after. This is the one place the two orders
        // disagree, and the RFC picks UTF-16.
        let value = json!({ "\u{E000}": 2, "\u{10000}": 1, "b": 3, "a": 4 });
        assert_eq!(
            to_canonical_json(&value).expect("canonical"),
            "{\"a\":4,\"b\":3,\"\u{10000}\":1,\"\u{E000}\":2}"
        );
    }

    #[test]
    fn output_is_compact_and_nested_values_are_canonicalized() {
        let value = json!({
            "z": [1, { "k": "v", "a": null }, true],
            "a": { "nested": { "y": 1, "x": 2 } }
        });
        assert_eq!(
            to_canonical_json(&value).expect("canonical"),
            r#"{"a":{"nested":{"x":2,"y":1}},"z":[1,{"a":null,"k":"v"},true]}"#
        );
    }

    #[test]
    fn strings_use_the_jcs_escapes() {
        let value = json!({ "s": "a\"b\\c\nd\u{0007}e\u{00E9}" });
        assert_eq!(
            to_canonical_json(&value).expect("canonical"),
            "{\"s\":\"a\\\"b\\\\c\\nd\\u0007e\u{00E9}\"}"
        );
    }

    #[test]
    fn integers_hold_to_the_safe_range_and_floats_are_refused() {
        assert_eq!(
            to_canonical_json(&json!({ "n": 9007199254740991_u64 })).expect("canonical"),
            r#"{"n":9007199254740991}"#
        );
        assert_eq!(
            to_canonical_json(&json!(-9007199254740991_i64)).expect("canonical"),
            "-9007199254740991"
        );
        for bad in [
            json!(9007199254740992_u64),
            json!(-9007199254740992_i64),
            json!(1.5),
            json!(1.0),
            json!({ "deep": [1, 2, 1e60] }),
        ] {
            assert_eq!(
                to_canonical_json(&bad),
                Err(CanonicalError::UnsupportedNumber),
                "should have refused: {bad}"
            );
        }
    }
}
