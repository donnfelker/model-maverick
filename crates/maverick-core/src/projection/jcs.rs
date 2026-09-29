//! RFC 8785 JSON Canonicalization Scheme (JCS) for JSON values embedded in a projection.
//!
//! Projection `p1` embeds JSON values (a response-format schema, tool-call arguments) in
//! its text. Every SDK core must produce the same bytes from the same request, so those
//! values are written with JCS, which has a library in every SDK language:
//!
//! - object members sorted by the UTF-16 code units of their names;
//! - no insignificant whitespace;
//! - strings escaped as ECMAScript `JSON.stringify` escapes them;
//! - numbers written as ECMAScript `Number.prototype.toString` writes an IEEE-754 double.

use std::fmt::Write as _;

use serde_json::Value;

/// Serialize `value` as RFC 8785 canonical JSON.
pub(crate) fn canonicalize(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => match number.as_f64() {
            Some(double) => write_number(out, double),
            None => out.push_str("null"),
        },
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(members) => {
            let mut sorted: Vec<(&String, &Value)> = members.iter().collect();
            sorted.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            out.push('{');
            for (index, (name, member)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(out, name);
                out.push(':');
                write_value(out, member);
            }
            out.push('}');
        }
    }
}

/// ECMAScript `JSON.stringify` string escaping.
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            control if control < ' ' => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// ECMAScript `Number::toString(x)` (radix 10) for a finite double. Non-finite values,
/// which JSON cannot carry, are written as `null`, as `JSON.stringify` writes them.
fn write_number(out: &mut String, value: f64) {
    if !value.is_finite() {
        out.push_str("null");
        return;
    }
    if value == 0.0 {
        // Both +0 and -0.
        out.push('0');
        return;
    }
    if value < 0.0 {
        out.push('-');
    }
    let (digits, exponent) = shortest_digits(value.abs());
    // ECMAScript names: `k` significant digits, value = 0.digits * 10^n.
    let k = digits.len() as i32;
    let n = exponent + 1;
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        let (whole, fraction) = digits.split_at(n as usize);
        out.push_str(whole);
        out.push('.');
        out.push_str(fraction);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        let (first, rest) = digits.split_at(1);
        out.push_str(first);
        if !rest.is_empty() {
            out.push('.');
            out.push_str(rest);
        }
        let power = n - 1;
        out.push('e');
        out.push(if power < 0 { '-' } else { '+' });
        let _ = write!(out, "{}", power.abs());
    }
}

/// Split `{:e}` output (`d[.ddd]e<exp>`) into its digits and decimal exponent.
fn split_scientific(scientific: &str) -> (String, i32) {
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("LowerExp output has an exponent");
    let exponent = exponent.parse().expect("LowerExp exponent is an integer");
    (mantissa.chars().filter(|ch| *ch != '.').collect(), exponent)
}

/// The shortest round-trip significant digits of a positive finite double and the decimal
/// exponent of the first digit, with ECMAScript's tie rule.
///
/// `{:e}` gives the shortest digits that round-trip. When two such digit strings are
/// equally close to the exact value, Rust takes the larger one, but ECMAScript
/// (`Number::toString`, note 2) takes the one whose last digit is even. That tie happens
/// only when the exact value has exactly one more significant digit than the shortest
/// form and that digit is 5.
fn shortest_digits(value: f64) -> (String, i32) {
    let (digits, exponent) = split_scientific(&format!("{value:e}"));
    // 800 digits is more than the 767 significant digits a double can need, so this is
    // the exact value followed by zero padding.
    let (exact, exact_exponent) = split_scientific(&format!("{value:.800e}"));
    let exact = exact.trim_end_matches('0');
    if exact_exponent != exponent || exact.len() != digits.len() + 1 || !exact.ends_with('5') {
        return (digits, exponent);
    }
    // The candidates are `lower` and `lower + 1`. Neither ends in 0: a 0 would make a
    // shorter form round-trip, and `{:e}` returned the shortest.
    let lower = &exact[..digits.len()];
    let last = lower.as_bytes()[lower.len() - 1] - b'0';
    if last % 2 == 0 {
        (lower.to_owned(), exponent)
    } else {
        let mut upper = lower[..lower.len() - 1].to_owned();
        upper.push(char::from(b'0' + last + 1));
        (upper, exponent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn number(bits: u64) -> String {
        let mut out = String::new();
        write_number(&mut out, f64::from_bits(bits));
        out
    }

    /// RFC 8785 Appendix B: IEEE-754 bit patterns and their canonical text.
    #[test]
    fn rfc8785_number_vectors() {
        let vectors: [(u64, &str); 24] = [
            (0x0000_0000_0000_0000, "0"),
            (0x8000_0000_0000_0000, "0"),
            (0x0000_0000_0000_0001, "5e-324"),
            (0x8000_0000_0000_0001, "-5e-324"),
            (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"),
            (0xffef_ffff_ffff_ffff, "-1.7976931348623157e+308"),
            (0x4340_0000_0000_0000, "9007199254740992"),
            (0xc340_0000_0000_0000, "-9007199254740992"),
            (0x4430_0000_0000_0000, "295147905179352830000"),
            (0x44b5_2d02_c7e1_4af5, "9.999999999999997e+22"),
            (0x44b5_2d02_c7e1_4af6, "1e+23"),
            (0x44b5_2d02_c7e1_4af7, "1.0000000000000001e+23"),
            (0x444b_1ae4_d6e2_ef4e, "999999999999999700000"),
            (0x444b_1ae4_d6e2_ef4f, "999999999999999900000"),
            (0x444b_1ae4_d6e2_ef50, "1e+21"),
            (0x3eb0_c6f7_a0b5_ed8c, "9.999999999999997e-7"),
            (0x3eb0_c6f7_a0b5_ed8d, "0.000001"),
            (0x41b3_de43_5555_5553, "333333333.3333332"),
            (0x41b3_de43_5555_5554, "333333333.33333325"),
            (0x41b3_de43_5555_5555, "333333333.3333333"),
            (0x41b3_de43_5555_5556, "333333333.3333334"),
            (0x41b3_de43_5555_5557, "333333333.33333343"),
            (0xbecb_f647_612f_3696, "-0.0000033333333333333333"),
            (0x4314_3ff3_c1cb_0959, "1424953923781206.2"),
        ];
        for (bits, want) in vectors {
            assert_eq!(number(bits), want, "bits {bits:#018x}");
        }
    }

    #[test]
    fn number_formats_at_the_notation_boundaries() {
        for (value, want) in [
            (1.0, "1"),
            (-1.5, "-1.5"),
            (0.1, "0.1"),
            (100.0, "100"),
            (123.456, "123.456"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (1e-6, "0.000001"),
            (1.25e-6, "0.00000125"),
            (1e-7, "1e-7"),
            (1.25e-7, "1.25e-7"),
            (f64::NAN, "null"),
            (f64::INFINITY, "null"),
        ] {
            let mut out = String::new();
            write_number(&mut out, value);
            assert_eq!(out, want, "value {value:?}");
        }
    }

    #[test]
    fn strings_escape_like_json_stringify() {
        let mut out = String::new();
        write_string(
            &mut out,
            "q\"b\\s\u{8}t\tn\nf\u{c}r\r\u{1}\u{1f} \u{7f}é€😀\u{2028}",
        );
        assert_eq!(
            out,
            "\"q\\\"b\\\\s\\bt\\tn\\nf\\fr\\r\\u0001\\u001f \u{7f}é€😀\u{2028}\""
        );
    }

    #[test]
    fn objects_sort_by_utf16_code_units_and_nest() {
        // U+10000 is D800 DC00 in UTF-16, so it sorts before U+FB33 (FB33), although
        // it sorts after it by code point. RFC 8785 §3.2.3 sorts by UTF-16 units.
        let value = json!({
            "b": [1, 2.5, "x", null, true, false, {"z": {}, "a": []}],
            "a": 1e3,
            "\u{fb33}": 1,
            "\u{10000}": 2,
            "A": -0.0
        });
        assert_eq!(
            canonicalize(&value),
            "{\"A\":0,\"a\":1000,\"b\":[1,2.5,\"x\",null,true,false,{\"a\":[],\"z\":{}}],\"\u{10000}\":2,\"\u{fb33}\":1}"
        );
    }

    #[test]
    fn large_integers_are_written_as_doubles() {
        // JS JSON.parse reads 2^53 + 1 as 2^53.
        let value: Value = serde_json::from_str("[9007199254740993, 12, -7]").expect("json");
        assert_eq!(canonicalize(&value), "[9007199254740992,12,-7]");
    }
}
