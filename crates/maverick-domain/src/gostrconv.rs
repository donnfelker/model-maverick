//! Go `strconv` parsing that engine behavior depends on.

use crate::gofmt;

/// Why a number did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumErrorKind {
    /// Not a Go float literal.
    Syntax,
    /// Well formed, but too large for a `float64` (Go returns ±Inf with this error).
    Range,
}

/// A `strconv.ParseFloat` error. The text is Go's:
/// `strconv.ParseFloat: parsing "x": invalid syntax`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFloatError {
    pub input: String,
    pub kind: NumErrorKind,
}

impl std::fmt::Display for ParseFloatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.kind {
            NumErrorKind::Syntax => "invalid syntax",
            NumErrorKind::Range => "value out of range",
        };
        write!(
            f,
            "strconv.ParseFloat: parsing {}: {reason}",
            gofmt::quote(&self.input)
        )
    }
}

impl std::error::Error for ParseFloatError {}

/// Go `strconv.ParseFloat(s, 64)`.
///
/// Accepts decimal literals (`42`, `-3.25`, `1.`, `.5`, `1e3`, `+5`), underscores
/// between digits (`1_000`), hexadecimal floats with a `p` exponent (`0x1p-2`), and
/// `inf` / `infinity` (optionally signed) and `nan` (unsigned), in any case. Rejects
/// surrounding space, `0x10` (no exponent), non-ASCII digits, and values too large for
/// a `float64` (`1e400`, a range error). Underflow rounds to zero without an error.
pub fn parse_float(s: &str) -> Result<f64, ParseFloatError> {
    let error = |kind| ParseFloatError {
        input: s.to_owned(),
        kind,
    };
    if let Some(value) = special(s) {
        return Ok(value);
    }
    let (negative, body) = match s.as_bytes().first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    let cleaned;
    let body = if body.contains('_') {
        if !underscore_ok(body) {
            return Err(error(NumErrorKind::Syntax));
        }
        cleaned = body.replace('_', "");
        cleaned.as_str()
    } else {
        body
    };
    let magnitude = match body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        Some(hex) => parse_hex(hex).ok_or(error(NumErrorKind::Syntax))?,
        None if is_decimal_literal(body) => body
            .parse::<f64>()
            .map_err(|_| error(NumErrorKind::Syntax))?,
        None => return Err(error(NumErrorKind::Syntax)),
    };
    if magnitude.is_infinite() {
        return Err(error(NumErrorKind::Range));
    }
    Ok(if negative { -magnitude } else { magnitude })
}

/// `inf`, `infinity` (with an optional sign) and `nan` (no sign), ignoring case. The
/// whole string must be the special value.
fn special(s: &str) -> Option<f64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1.0, &s[1..]),
        b'-' => (-1.0, &s[1..]),
        _ => (1.0, s),
    };
    if rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity") {
        return Some(sign * f64::INFINITY);
    }
    if rest.len() == s.len() && rest.eq_ignore_ascii_case("nan") {
        return Some(f64::NAN);
    }
    None
}

/// `digits* ('.' digits*)? ([eE] [+-]? digits+)?` with at least one mantissa digit.
fn is_decimal_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    // Counting digits with an iterator keeps every scan bounded: a `while` that steps
    // `i += 1` hangs under a mutated `i *= 1`, and a cargo-mutants timeout fails CI.
    let digits_from = |i: usize| bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
    let mut i = digits_from(0);
    let mut mantissa_digits = i;
    if bytes.get(i) == Some(&b'.') {
        let fraction = digits_from(i + 1);
        i += 1 + fraction;
        mantissa_digits += fraction;
    }
    if mantissa_digits == 0 {
        return false;
    }
    if let Some(b'e' | b'E') = bytes.get(i) {
        i += 1;
        if let Some(b'+' | b'-') = bytes.get(i) {
            i += 1;
        }
        let exponent = digits_from(i);
        if exponent == 0 {
            return false;
        }
        i += exponent;
    }
    i == bytes.len()
}

/// A hexadecimal float after `0x`: hex digits with at most one `.`, then a mandatory
/// `p` exponent (decimal, optionally signed).
fn parse_hex(s: &str) -> Option<f64> {
    let (mantissa, exponent) = s.split_once(['p', 'P'])?;
    let mut value: u64 = 0;
    let mut binary_exponent: i64 = 0;
    let mut seen_dot = false;
    let mut saw_digit = false;
    for c in mantissa.chars() {
        if c == '.' {
            if seen_dot {
                return None;
            }
            seen_dot = true;
            continue;
        }
        let digit = u64::from(c.to_digit(16)?);
        saw_digit = true;
        if value >> 60 == 0 {
            value = value * 16 + digit;
            if seen_dot {
                binary_exponent -= 4;
            }
        } else if !seen_dot {
            // Past 60 bits of precision: drop the digit and keep its scale.
            binary_exponent += 4;
        }
    }
    if !saw_digit {
        return None;
    }
    let (exponent_negative, exponent_digits) = match exponent.as_bytes().first()? {
        b'+' => (false, &exponent[1..]),
        b'-' => (true, &exponent[1..]),
        _ => (false, exponent),
    };
    if exponent_digits.is_empty() || !exponent_digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Saturate: anything past a few thousand is already 0 or infinity.
    let magnitude = exponent_digits
        .parse::<i64>()
        .unwrap_or(i64::MAX)
        .min(100_000);
    let exponent = if exponent_negative {
        -magnitude
    } else {
        magnitude
    };
    let scale = i32::try_from((binary_exponent + exponent).clamp(-2_000, 2_000))
        .expect("clamped into i32 range");
    // Scale in two steps so a subnormal result is not flushed early.
    let half = scale / 2;
    Some(value as f64 * 2f64.powi(half) * 2f64.powi(scale - half))
}

/// Go's `underscoreOK`: underscores only between digits, or between the base prefix
/// and a digit.
fn underscore_ok(s: &str) -> bool {
    let bytes = s.as_bytes();
    // `^` start, `0` digit or base prefix, `_` underscore, `!` anything else.
    let mut saw = b'^';
    let mut start = 0;
    let mut hex = false;
    if let [b'0', base, ..] = bytes {
        if matches!(base.to_ascii_lowercase(), b'b' | b'o' | b'x') {
            start = 2;
            saw = b'0';
            hex = base.eq_ignore_ascii_case(&b'x');
        }
    }
    for &c in &bytes[start..] {
        if c.is_ascii_digit() || (hex && c.is_ascii_hexdigit()) {
            saw = b'0';
        } else if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
        } else {
            if saw == b'_' {
                return false;
            }
            saw = b'!';
        }
    }
    saw != b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_forms() {
        for (s, want) in [
            ("42", 42.0),
            ("-3.25", -3.25),
            ("+5", 5.0),
            ("1.", 1.0),
            (".5", 0.5),
            ("1e3", 1000.0),
            ("1E+3", 1000.0),
            ("2.5e-1", 0.25),
            ("0.1", 0.1),
            ("007", 7.0),
            ("-0", -0.0),
            ("1e-400", 0.0),
        ] {
            assert_eq!(parse_float(s), Ok(want), "{s}");
        }
    }

    #[test]
    fn specials() {
        for s in ["Inf", "inf", "+INF", "infinity", "+Infinity"] {
            assert_eq!(parse_float(s), Ok(f64::INFINITY), "{s}");
        }
        for s in ["-inf", "-Infinity"] {
            assert_eq!(parse_float(s), Ok(f64::NEG_INFINITY), "{s}");
        }
        for s in ["NaN", "nan", "nAn"] {
            assert!(parse_float(s).expect(s).is_nan(), "{s}");
        }
        for s in ["+nan", "-NaN", "infx", "infinit", "nanx", "in"] {
            assert_eq!(
                parse_float(s).unwrap_err().kind,
                NumErrorKind::Syntax,
                "{s}"
            );
        }
    }

    #[test]
    fn underscores_follow_go_rules() {
        assert_eq!(parse_float("1_000"), Ok(1000.0));
        assert_eq!(parse_float("1_000.5_0e1_0"), Ok(1000.5e10));
        assert_eq!(parse_float("-1_0"), Ok(-10.0));
        assert_eq!(parse_float("0x_1p0"), Ok(1.0));
        assert_eq!(parse_float("0x1_0p0"), Ok(16.0));
        for s in ["_1", "1_", "1__0", "1_.5", "1._5", "1e_5", "+_1", "1_e5"] {
            assert_eq!(
                parse_float(s).unwrap_err().kind,
                NumErrorKind::Syntax,
                "{s}"
            );
        }
    }

    #[test]
    fn hex_floats_need_an_exponent() {
        assert_eq!(parse_float("0x1p-2"), Ok(0.25));
        assert_eq!(parse_float("0X1P+2"), Ok(4.0));
        assert_eq!(parse_float("0x1.8p1"), Ok(3.0));
        assert_eq!(parse_float("0x.8p0"), Ok(0.5));
        assert_eq!(parse_float("-0xAp0"), Ok(-10.0));
        assert_eq!(parse_float("0x1p-1074"), Ok(f64::from_bits(1)));
        assert_eq!(parse_float("0x1p-1080"), Ok(0.0));
        assert_eq!(
            parse_float("0x1p99999999999"),
            Err(ParseFloatError {
                input: "0x1p99999999999".to_owned(),
                kind: NumErrorKind::Range,
            })
        );
        // 17 hex digits: the last is past 60 bits of precision and only scales.
        assert_eq!(parse_float("0x10000000000000000p0"), Ok(2f64.powi(64)));
        assert_eq!(parse_float("0x1.00000000000000001p0"), Ok(1.0));
        for s in [
            "0x10",
            "0x",
            "0xp1",
            "0x1p",
            "0x1p+",
            "0x1.2.3p0",
            "0xgp0",
            "0x1pa",
        ] {
            assert_eq!(
                parse_float(s).unwrap_err().kind,
                NumErrorKind::Syntax,
                "{s}"
            );
        }
    }

    #[test]
    fn rejects_what_go_rejects() {
        for s in [
            "", " 1", "1 ", "4 2", ".", "e5", "1e", "1e+", "+-1", "٤٢", "１２", "1,2", "0b1",
            "1.2.3",
        ] {
            assert_eq!(
                parse_float(s).unwrap_err().kind,
                NumErrorKind::Syntax,
                "{s:?}"
            );
        }
        let error = parse_float("1e400").unwrap_err();
        assert_eq!(error.kind, NumErrorKind::Range);
        assert_eq!(
            error.to_string(),
            "strconv.ParseFloat: parsing \"1e400\": value out of range"
        );
        assert_eq!(
            parse_float("x").unwrap_err().to_string(),
            "strconv.ParseFloat: parsing \"x\": invalid syntax"
        );
    }
}
