//! Byte-level helpers for the delivery wire formats (reference Part 3 §4.3).

use chrono::{DateTime, Timelike, Utc};

/// Go `json.Compact` with HTML escaping, as `json.Marshal` applies it to a
/// `json.RawMessage`: whitespace outside strings is dropped, and inside strings `<`,
/// `>`, `&`, U+2028, and U+2029 become `\u` escapes. Everything else, including
/// existing escapes, is copied as is.
pub(crate) fn compact_escaped(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_string {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => {
                    escaped = true;
                    out.push(c);
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                '<' => out.push_str("\\u003c"),
                '>' => out.push_str("\\u003e"),
                '&' => out.push_str("\\u0026"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                _ => out.push(c),
            }
        } else {
            match c {
                ' ' | '\t' | '\n' | '\r' => {}
                '"' => {
                    in_string = true;
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }
    out
}

/// Go `time.RFC3339Nano` in UTC: fractional seconds with trailing zeros trimmed, and no
/// fraction at all for a whole second.
pub(crate) fn rfc3339_nano(at: DateTime<Utc>) -> String {
    let base = at.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = at.nanosecond() % 1_000_000_000;
    if nanos == 0 {
        return format!("{base}Z");
    }
    let fraction = format!("{nanos:09}");
    format!("{base}.{}Z", fraction.trim_end_matches('0'))
}

/// Go `time.RFC1123Z` in UTC, for example `Thu, 08 May 2031 03:30:00 +0000`.
pub(crate) fn rfc1123z(at: DateTime<Utc>) -> String {
    at.format("%a, %d %b %Y %H:%M:%S +0000").to_string()
}

/// Lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Standard base64 with padding, for SMTP AUTH PLAIN.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes `%XX` escapes, as Go `url.User.Username` and `Password` return them.
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn compact_drops_whitespace_and_escapes_html() {
        assert_eq!(
            compact_escaped(r#"{"a": 86.94117647058823, "b": 0.0000001, "c": "x>y", "aa": 2}"#),
            r#"{"a":86.94117647058823,"b":0.0000001,"c":"x\u003ey","aa":2}"#
        );
        assert_eq!(
            compact_escaped(r#"{"s": "a b\" <&"}"#),
            r#"{"s":"a b\" \u003c\u0026"}"#
        );
    }

    #[test]
    fn rfc3339_nano_trims_the_fraction() {
        let at = Utc.with_ymd_and_hms(2031, 5, 8, 3, 30, 0).unwrap();
        assert_eq!(rfc3339_nano(at), "2031-05-08T03:30:00Z");
        let micros = at + chrono::Duration::microseconds(123_456);
        assert_eq!(rfc3339_nano(micros), "2031-05-08T03:30:00.123456Z");
        let tenth = at + chrono::Duration::milliseconds(100);
        assert_eq!(rfc3339_nano(tenth), "2031-05-08T03:30:00.1Z");
    }

    #[test]
    fn rfc1123z_has_a_numeric_zone() {
        let at = Utc.with_ymd_and_hms(2031, 5, 8, 3, 30, 0).unwrap();
        assert_eq!(rfc1123z(at), "Thu, 08 May 2031 03:30:00 +0000");
    }

    #[test]
    fn encodings() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(base64(b"\0user\0pass"), "AHVzZXIAcGFzcw==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(percent_decode("p%40ss%3Aword"), "p@ss:word");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
