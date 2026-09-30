//! History pagination cursors (reference part 4 §6.1; spec §9.6).
//!
//! A cursor is `base64url_nopad("v1|<kind>|<field>|...")`. Kinds and their fields:
//! `rec` [recommendation id], `out` [outcome id], `agg` [as_of `YYYY-MM-DD`, capability,
//! tier], `cal` [batch_id, capability]. Decoding is Go's `base64.RawURLEncoding`: the URL
//! alphabet only, no padding, `\r` and `\n` ignored, and a lone trailing character is an
//! error. The text must split on `|` into exactly `2 + n` parts: `v1`, the endpoint's kind,
//! and `n` non-empty fields. Every failure is the same error, [`INVALID_CURSOR`], with
//! details `{"parameter": "cursor"}` (a 422 `invalid_request` at the API).

use chrono::NaiveDate;

/// The cursor format version.
pub const CURSOR_VERSION: &str = "v1";

/// The separator between cursor parts.
pub const CURSOR_SEPARATOR: char = '|';

/// The exact error message for any bad cursor.
pub const INVALID_CURSOR: &str = "cursor is not a valid pagination token";

/// The `details.parameter` value of the bad-cursor error.
pub const INVALID_CURSOR_PARAMETER: &str = "cursor";

/// Recommendation history: `[recommendation id]`.
pub const KIND_RECOMMENDATION: &str = "rec";
/// Outcome history: `[outcome id]`.
pub const KIND_OUTCOME: &str = "out";
/// Aggregate series: `[as_of, capability, tier]`.
pub const KIND_AGGREGATE: &str = "agg";
/// Calibration: `[batch_id, capability]`.
pub const KIND_CALIBRATION: &str = "cal";

/// A cursor that does not decode for the endpoint that received it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cursor is not a valid pagination token")]
pub struct InvalidCursor;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Mint a cursor for `kind` from its fields.
#[must_use]
pub fn encode_cursor(kind: &str, fields: &[&str]) -> String {
    let mut raw = String::from(CURSOR_VERSION);
    raw.push(CURSOR_SEPARATOR);
    raw.push_str(kind);
    for field in fields {
        raw.push(CURSOR_SEPARATOR);
        raw.push_str(field);
    }
    base64url_encode(raw.as_bytes())
}

/// Decode a cursor minted for `kind` with exactly `field_count` fields.
pub fn decode_cursor(
    token: &str,
    kind: &str,
    field_count: usize,
) -> Result<Vec<String>, InvalidCursor> {
    let bytes = base64url_decode(token).ok_or(InvalidCursor)?;
    let text = String::from_utf8(bytes).map_err(|_| InvalidCursor)?;
    let parts: Vec<&str> = text.split(CURSOR_SEPARATOR).collect();
    let valid = parts.len() == field_count + 2
        && parts[0] == CURSOR_VERSION
        && parts[1] == kind
        && parts[2..].iter().all(|field| !field.is_empty());
    if !valid {
        return Err(InvalidCursor);
    }
    Ok(parts[2..].iter().map(|field| (*field).to_owned()).collect())
}

/// An aggregate-series cursor position: `(as_of, capability, tier)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregatePosition {
    /// The `as_of` date.
    pub as_of: NaiveDate,
    /// The capability.
    pub capability: String,
    /// The difficulty tier.
    pub tier: i16,
}

/// Mint an aggregate-series cursor.
#[must_use]
pub fn encode_aggregate_cursor(position: &AggregatePosition) -> String {
    let as_of = position.as_of.format("%Y-%m-%d").to_string();
    let tier = position.tier.to_string();
    encode_cursor(KIND_AGGREGATE, &[&as_of, &position.capability, &tier])
}

/// Decode an aggregate-series cursor: Go also parses the date (layout `2006-01-02`) and the
/// tier (`int16`), and either failure is the same error.
pub fn decode_aggregate_cursor(token: &str) -> Result<AggregatePosition, InvalidCursor> {
    let fields = decode_cursor(token, KIND_AGGREGATE, 3)?;
    Ok(AggregatePosition {
        as_of: parse_go_date(&fields[0]).ok_or(InvalidCursor)?,
        capability: fields[1].clone(),
        tier: fields[2].parse().map_err(|_| InvalidCursor)?,
    })
}

/// Go `time.Parse("2006-01-02", s)`: four-digit year, two-digit month and day, a real date.
fn parse_go_date(text: &str) -> Option<NaiveDate> {
    let bytes = text.as_bytes();
    let shape = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit());
    if !shape {
        return None;
    }
    let year = text[..4].parse().ok()?;
    let month = text[5..7].parse().ok()?;
    let day = text[8..].parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// `base64.RawURLEncoding.EncodeToString`. The capture list cursor is this encoding of
/// the last returned capture id.
pub fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // `+`, not `|`: the shifted bytes never overlap, so `|` and `^` agree and a
        // mutant swapping them could not be caught. A mutated `+` changes the value.
        let group = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |group, (index, byte)| {
                group + (u32::from(*byte) << (16 - 8 * index))
            });
        for index in 0..=chunk.len() {
            let sextet = (group >> (18 - 6 * index)) & 63;
            out.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    out
}

/// `base64.RawURLEncoding.DecodeString` (not `Strict`): URL alphabet only, no padding,
/// `\r` and `\n` skipped, unused trailing bits ignored, and a lone trailing character is an
/// error.
pub fn base64url_decode(token: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(token.len() * 3 / 4);
    let mut group = 0_u32;
    let mut bits = 0_u32;
    let mut count = 0_usize;
    for byte in token.bytes() {
        if byte == b'\r' || byte == b'\n' {
            continue;
        }
        let sextet = ALPHABET.iter().position(|symbol| *symbol == byte)?;
        // `+`, not `|`, for the same reason as in `base64url_encode`.
        group = (group << 6) + sextet as u32;
        bits += 6;
        count += 1;
        if bits >= 8 {
            bits -= 8;
            out.push((group >> bits) as u8);
            group &= (1 << bits) - 1;
        }
    }
    (count % 4 != 1).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_every_length_and_byte() {
        for length in 0..=12_usize {
            for seed in 0..=255_u8 {
                let bytes: Vec<u8> = (0..length)
                    .map(|index| {
                        seed.wrapping_mul(31)
                            .wrapping_add((index as u8).wrapping_mul(97))
                    })
                    .collect();
                let encoded = base64url_encode(&bytes);
                assert_eq!(encoded.len(), (length * 4).div_ceil(3), "no padding");
                assert_eq!(base64url_decode(&encoded), Some(bytes), "{encoded}");
            }
        }
    }

    #[test]
    fn base64url_matches_known_vectors() {
        // RFC 4648 §10 vectors in the URL alphabet without padding.
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url_encode(plain.as_bytes()), encoded);
            assert_eq!(base64url_decode(encoded).as_deref(), Some(plain.as_bytes()));
        }
        // `-` and `_` are 62 and 63.
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url_decode("-_8"), Some(vec![0xfb, 0xff]));
    }

    #[test]
    fn base64url_decode_follows_go_raw_url_rules() {
        for bad in [
            "=", "Zg==", "Zm9v=", "+w", "/w", "Z", "Zm9vY", "Zm 9v", "Zm.9",
        ] {
            assert_eq!(base64url_decode(bad), None, "{bad:?}");
        }
        // Newlines are skipped; unused trailing bits are ignored (not Strict).
        assert_eq!(base64url_decode("Zm\r\n9v\n").as_deref(), Some(&b"foo"[..]));
        assert_eq!(base64url_decode("Zh").as_deref(), Some(&b"f"[..]));
        assert_eq!(base64url_decode("Zm9"), base64url_decode("Zm8"));
        // Alphabet edges.
        assert_eq!(base64url_decode("AA"), Some(vec![0]));
        assert_eq!(base64url_decode("__8"), Some(vec![0xff, 0xff]));
        for edge in ["@A", "[A", "`A", "{A", ":A", ",A"] {
            assert_eq!(base64url_decode(edge), None, "{edge:?}");
        }
    }

    #[test]
    fn go_dates_are_strict() {
        assert_eq!(
            parse_go_date("2026-07-01"),
            NaiveDate::from_ymd_opt(2026, 7, 1)
        );
        assert_eq!(
            parse_go_date("0001-01-01"),
            NaiveDate::from_ymd_opt(1, 1, 1)
        );
        for bad in [
            "2026-7-01",
            "2026-07-1",
            "26-07-01",
            "2026/07/01",
            "2026-02-30",
            "2026-13-01",
            "2026-07-01 ",
            "+026-07-01",
            "2026-0a-01",
        ] {
            assert_eq!(parse_go_date(bad), None, "{bad:?}");
        }
    }
}
