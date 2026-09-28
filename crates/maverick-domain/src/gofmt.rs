//! Go `fmt` formatting that engine error strings depend on.
//!
//! Many Go error messages embed values with `%v` (floats) and `%q` (strings). The texts
//! are part of the parity contract, so the Rust port formats those values the same way.

/// Go `%v` of a `float64`: `strconv.FormatFloat(x, 'g', -1, 64)`. The digits are the
/// shortest that round-trip. The exponent form is used when the decimal exponent is
/// `< -4` or `>= 6` (Go's threshold for the shortest precision), with at least two
/// exponent digits: `1e+06`, `1.5e-05`, but `123456` and `0.0001`.
pub fn fmt_v_f64(x: f64) -> String {
    if let Some(special) = special(x) {
        return special.to_owned();
    }
    let (mantissa, exp) = split_exp(&format!("{x:e}"));
    format_g(x.is_sign_negative(), &mantissa, exp, || format!("{x}"))
}

/// Go `%v` of a `float32`: the same rules as [`fmt_v_f64`] with the shortest `float32`
/// digits (`0.7`, not `0.699999988079071`).
pub fn fmt_v_f32(x: f32) -> String {
    if let Some(special) = special(f64::from(x)) {
        return special.to_owned();
    }
    let (mantissa, exp) = split_exp(&format!("{x:e}"));
    format_g(x.is_sign_negative(), &mantissa, exp, || format!("{x}"))
}

fn special(x: f64) -> Option<&'static str> {
    if x.is_nan() {
        Some("NaN")
    } else if x == f64::INFINITY {
        Some("+Inf")
    } else if x == f64::NEG_INFINITY {
        Some("-Inf")
    } else {
        None
    }
}

/// Splits Rust `{:e}` output (`-1.5e-7`) into the mantissa without sign (`1.5`) and the
/// decimal exponent (`-7`).
fn split_exp(s: &str) -> (String, i32) {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exp) = s.split_once('e').expect("{:e} output has an exponent");
    (
        mantissa.to_owned(),
        exp.parse().expect("{:e} exponent is an integer"),
    )
}

fn format_g(negative: bool, mantissa: &str, exp: i32, plain: impl FnOnce() -> String) -> String {
    if !(-4..6).contains(&exp) {
        let sign = if negative { "-" } else { "" };
        // A signed exponent with at least two digits: `e+06`, `e-05`, `e-100`.
        format!("{sign}{mantissa}e{exp:+03}")
    } else {
        // Rust's `Display` prints the shortest round-trip digits without an exponent,
        // which is Go's `%f` with the shortest precision.
        plain()
    }
}

/// Go `%q` of a string (`strconv.Quote`): double-quoted, with `\"`, `\\`, the C escapes,
/// and `\x`/`\u`/`\U` escapes for non-printable characters. Printable non-ASCII
/// characters stay as they are.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            c if is_go_printable(c) => out.push(c),
            // Only ASCII controls and DEL reach here below U+0080.
            c if c.is_ascii() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c <= '\u{ffff}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    out
}

/// An approximation of Go `strconv.IsPrint`: letters, marks, numbers, punctuation,
/// symbols, and the ASCII space. Controls, other spaces, the common format characters,
/// private-use code points, and noncharacters are not printable. (Unassigned code
/// points would need Unicode tables; they are treated as printable.)
fn is_go_printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    if c.is_control() || c.is_whitespace() {
        return false;
    }
    let code = c as u32;
    let noncharacter = (0xfdd0..=0xfdef).contains(&code) || code & 0xfffe == 0xfffe;
    !noncharacter
        && !matches!(
            code,
            0xad | 0x600..=0x605
                | 0x61c
                | 0x6dd
                | 0x70f
                | 0x890..=0x891
                | 0x8e2
                | 0x180e
                | 0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x2064
                | 0x2066..=0x206f
                | 0xe000..=0xf8ff
                | 0xfeff
                | 0xfff9..=0xfffb
                | 0x110bd
                | 0x110cd
                | 0x13430..=0x1343f
                | 0x1bca0..=0x1bca3
                | 0x1d173..=0x1d17a
                | 0xe0001
                | 0xe0020..=0xe007f
                | 0xf0000..=0x10ffff
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_v_f64_matches_go() {
        for (x, want) in [
            (0.0, "0"),
            (-0.0, "-0"),
            (1.0, "1"),
            (-1.0, "-1"),
            (0.9, "0.9"),
            (1.5, "1.5"),
            (0.1 + 0.2, "0.30000000000000004"),
            (123_456.0, "123456"),
            (1_000_000.0, "1e+06"),
            (1_234_567.0, "1.234567e+06"),
            (0.0001, "0.0001"),
            (0.000_01, "1e-05"),
            (0.000_015, "1.5e-05"),
            (1e21, "1e+21"),
            (1e-100, "1e-100"),
            (9_007_199_254_740_992.0, "9.007199254740992e+15"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "+Inf"),
            (f64::NEG_INFINITY, "-Inf"),
        ] {
            assert_eq!(fmt_v_f64(x), want, "{x:?}");
        }
    }

    #[test]
    fn fmt_v_f32_uses_shortest_f32_digits() {
        assert_eq!(fmt_v_f32(0.7), "0.7");
        assert_eq!(fmt_v_f64(f64::from(0.7f32)), "0.699999988079071");
        assert_eq!(fmt_v_f32(-1.0), "-1");
        assert_eq!(fmt_v_f32(f32::NAN), "NaN");
        assert_eq!(fmt_v_f32(3e7), "3e+07");
    }

    #[test]
    fn quote_matches_strconv_quote() {
        assert_eq!(quote("fluency"), "\"fluency\"");
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(quote("tab\there\nnl"), "\"tab\\there\\nnl\"");
        assert_eq!(quote("\u{1}"), "\"\\x01\"");
        assert_eq!(quote("\u{7f}"), "\"\\x7f\"");
        assert_eq!(quote("\u{85}"), "\"\\u0085\"");
        assert_eq!(quote("zürich 東京"), "\"zürich 東京\"");
        assert_eq!(quote("\u{200b}"), "\"\\u200b\"");
        // C1 controls, other spaces, format, private use, noncharacters.
        assert_eq!(quote("\u{80}"), "\"\\u0080\"");
        assert_eq!(quote("\u{a0}\u{3000}"), "\"\\u00a0\\u3000\"");
        assert_eq!(quote("\u{2028}"), "\"\\u2028\"");
        assert_eq!(quote("\u{e000}"), "\"\\ue000\"");
        assert_eq!(quote("\u{fdd0}\u{ffff}"), "\"\\ufdd0\\uffff\"");
        assert_eq!(quote("\u{e0001}"), "\"\\U000e0001\"");
        assert_eq!(quote("\u{1fffe}"), "\"\\U0001fffe\"");
        assert_eq!(quote("\u{10ffff}"), "\"\\U0010ffff\"");
        // Printable supplementary-plane characters stay as they are.
        assert_eq!(quote("\u{10000}😀"), "\"\u{10000}😀\"");
        assert_eq!(quote(""), "\"\"");
    }
}
