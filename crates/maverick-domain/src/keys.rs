//! Auth primitives: API key minting and hashing, and internal token parsing with a
//! constant-time match.
//!
//! This module is pure. It has no I/O and no HTTP. The axum middleware, the store
//! lookup by `key_hash`, and the rate limiter live in the API package and call these
//! functions. Reference Part 4 §5.1 and §5.3; spec §9.1 and §13. Parity fixtures live
//! in `parity/fixtures/decision/auth_keys.json`.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

/// Prefix on every raw API key. The key hash covers the prefix too.
pub const KEY_PREFIX: &str = "mk_live_";

/// base62 alphabet, most significant digit first. Order is exact and matters.
pub const BASE62_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Number of CSPRNG bytes behind a raw key.
pub const RANDOM_BYTES: usize = 32;

/// base62 length of a raw key after the prefix. Always this many digits.
pub const ENCODED_LEN: usize = 43;

/// A freshly minted API key. The raw key is shown once and never stored. Only the hash
/// and the last four characters go to the store.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MintedKey {
    /// `mk_live_` + 43 base62 characters (51 characters total).
    pub raw_key: String,
    /// `SHA-256(raw_key bytes)`, the `bytea` stored at rest.
    pub key_hash: [u8; 32],
    /// Last four characters of the raw key.
    pub last4: String,
}

/// `SHA-256` of the raw key string bytes, including the prefix.
pub fn key_hash(raw_key: &str) -> [u8; 32] {
    sha256(raw_key.as_bytes())
}

/// Encode 32 bytes as exactly 43 base62 digits.
///
/// The bytes are one big-endian unsigned integer. Divide by 62 repeatedly; each
/// remainder is a digit, least significant first. Left-pad with `0` to 43 digits and
/// emit most significant first. `62^43 > 2^256`, so 43 digits always hold a 32 byte
/// value and never overflow to 44.
pub fn encode_base62(bytes: &[u8; RANDOM_BYTES]) -> String {
    let mut num = *bytes;
    let mut digits: Vec<u8> = Vec::with_capacity(ENCODED_LEN);

    loop {
        let mut remainder: u32 = 0;
        let mut quotient_is_zero = true;
        for byte in num.iter_mut() {
            let acc = (remainder << 8) | u32::from(*byte);
            *byte = (acc / 62) as u8;
            remainder = acc % 62;
            if *byte != 0 {
                quotient_is_zero = false;
            }
        }
        digits.push(BASE62_ALPHABET[remainder as usize]);
        if quotient_is_zero {
            break;
        }
    }

    while digits.len() < ENCODED_LEN {
        digits.push(b'0');
    }
    digits.reverse();
    String::from_utf8(digits).expect("base62 alphabet is ASCII")
}

/// Mint a key from caller-supplied bytes. Deterministic, so the parity fixtures drive
/// it directly.
pub fn mint_key_from_bytes(bytes: &[u8; RANDOM_BYTES]) -> MintedKey {
    let raw_key = format!("{KEY_PREFIX}{}", encode_base62(bytes));
    let key_hash = key_hash(&raw_key);
    let last4 = raw_key[raw_key.len() - 4..].to_string();
    MintedKey {
        raw_key,
        key_hash,
        last4,
    }
}

/// Mint a key from the thread CSPRNG (ChaCha based). Use this in production; use
/// [`mint_key_from_bytes`] in tests.
pub fn mint_key() -> MintedKey {
    use rand::Rng;
    let mut bytes = [0u8; RANDOM_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    mint_key_from_bytes(&bytes)
}

/// One internal token entry. The secret is not kept; only its digest and the label are.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InternalToken {
    label: String,
    digest: [u8; 32],
}

impl InternalToken {
    /// Build an entry from a label and a secret. The digest is `SHA-256(label + ":" +
    /// secret)`.
    fn new(label: &str, secret: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(label.as_bytes());
        hasher.update(b":");
        hasher.update(secret.as_bytes());
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&hasher.finalize());
        Self {
            label: label.to_string(),
            digest,
        }
    }

    /// The acting party recorded when this entry matches.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// `SHA-256(label + ":" + secret)`, compared against a bearer token digest.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

/// Why parsing `MAVERICK_INTERNAL_TOKENS` failed. The config loader maps these to its
/// issue strings; the `Display` text is the exact string it reports.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Error)]
pub enum InternalTokenError {
    /// The value is unset or blank.
    #[error("required but not set")]
    Missing,
    /// The value is set but an entry is not a non-empty `label:secret` pair.
    #[error("must be a comma-separated list of \"label:secret\" pairs")]
    Malformed,
}

/// Parse `MAVERICK_INTERNAL_TOKENS`: a comma-separated list of `label:secret` pairs.
///
/// Blank input is [`InternalTokenError::Missing`]. Otherwise split on `,`; trim each
/// entry at both ends; cut at the first `:`; the left side is the label and the right
/// side is the secret, neither trimmed further. If any entry has no `:`, an empty
/// label, or an empty secret, the whole list is [`InternalTokenError::Malformed`] (one
/// issue, even if several entries are bad). Duplicate labels are allowed. A secret may
/// contain `:`.
pub fn parse_internal_tokens(raw: &str) -> Result<Vec<InternalToken>, InternalTokenError> {
    if raw.trim().is_empty() {
        return Err(InternalTokenError::Missing);
    }

    let mut tokens = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        let (label, secret) = entry.split_once(':').ok_or(InternalTokenError::Malformed)?;
        if label.is_empty() || secret.is_empty() {
            return Err(InternalTokenError::Malformed);
        }
        tokens.push(InternalToken::new(label, secret));
    }
    Ok(tokens)
}

/// Match an `Authorization` header against the internal token list.
///
/// The header must start with exactly `Bearer ` (case sensitive, one space). The
/// remainder is hashed and compared with every entry using a constant-time compare over
/// the 32 byte digests. Every entry is always compared and the first match's label wins.
/// An empty or wrong-scheme header matches nothing. `None` means no match; the API layer
/// turns that into a 401.
pub fn authenticate_internal<'a>(
    authorization: &str,
    tokens: &'a [InternalToken],
) -> Option<&'a str> {
    let token = parse_bearer(authorization)?;
    let digest = sha256(token.as_bytes());

    let mut matched: Option<&'a str> = None;
    for entry in tokens {
        if bool::from(digest[..].ct_eq(&entry.digest[..])) {
            matched.get_or_insert(entry.label.as_str());
        }
    }
    matched
}

/// The token after a strict `Bearer ` prefix, or `None` if the scheme is wrong. The
/// remainder is not trimmed, so `Bearer  x` keeps its leading space and `Bearer ` yields
/// an empty token that no entry can match.
fn parse_bearer(authorization: &str) -> Option<&str> {
    authorization.strip_prefix("Bearer ")
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let out = Sha256::digest(data);
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fmt::Write as _;

    // The parity fixture. Path is relative to this source file: up to the repo root,
    // then into parity/. Loaded at compile time so the test needs no fixture crate.
    const FIXTURE: &str = include_str!("../../../parity/fixtures/decision/auth_keys.json");

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("auth_keys.json parses")
    }

    fn hex_decode(hex: &str) -> Vec<u8> {
        assert!(hex.len() % 2 == 0, "hex has even length");
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex digit"))
            .collect()
    }

    fn hex_encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(out, "{byte:02x}").expect("write to String");
        }
        out
    }

    fn bytes32(hex: &str) -> [u8; 32] {
        let decoded = hex_decode(hex);
        assert_eq!(decoded.len(), 32, "input_hex is 32 bytes");
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&decoded);
        arr
    }

    #[test]
    fn fixture_constants_match_module() {
        let f = fixture();
        assert_eq!(
            f["base62_alphabet"].as_str().unwrap().as_bytes(),
            &BASE62_ALPHABET[..]
        );
        assert_eq!(f["key_prefix"].as_str().unwrap(), KEY_PREFIX);
        assert_eq!(f["encoded_length"].as_u64().unwrap() as usize, ENCODED_LEN);
        assert_eq!(f["random_bytes"].as_u64().unwrap() as usize, RANDOM_BYTES);
    }

    #[test]
    fn base62_and_mint_match_fixture_rows() {
        let f = fixture();
        for case in f["base62_cases"].as_array().unwrap() {
            let id = case["id"].as_str().unwrap();
            let bytes = bytes32(case["input_hex"].as_str().unwrap());

            let encoded = encode_base62(&bytes);
            assert_eq!(encoded.len(), ENCODED_LEN, "case {id}: length");
            assert_eq!(
                encoded,
                case["expected_base62"].as_str().unwrap(),
                "case {id}: base62"
            );

            let minted = mint_key_from_bytes(&bytes);
            assert_eq!(
                minted.raw_key,
                case["raw_key"].as_str().unwrap(),
                "case {id}: raw_key"
            );
            assert_eq!(
                hex_encode(&minted.key_hash),
                case["key_hash_hex"].as_str().unwrap(),
                "case {id}: key_hash"
            );
            assert_eq!(
                minted.last4,
                case["last4"].as_str().unwrap(),
                "case {id}: last4"
            );
        }
    }

    #[test]
    fn mint_key_has_the_expected_format() {
        let key = mint_key();
        assert!(key.raw_key.starts_with(KEY_PREFIX));
        assert_eq!(key.raw_key.len(), KEY_PREFIX.len() + ENCODED_LEN);
        let encoded = &key.raw_key[KEY_PREFIX.len()..];
        assert_eq!(encoded.len(), ENCODED_LEN);
        assert!(
            encoded.bytes().all(|b| BASE62_ALPHABET.contains(&b)),
            "base62 chars only"
        );
        assert_eq!(key.last4, key.raw_key[key.raw_key.len() - 4..]);
        assert_eq!(key.key_hash, sha256(key.raw_key.as_bytes()));

        // Two mints must differ: a 256 bit CSPRNG collision has probability 2^-256.
        // This also fails if minting stops drawing fresh random bytes.
        assert_ne!(mint_key().raw_key, mint_key().raw_key);
    }

    #[test]
    fn internal_digest_example_matches() {
        let f = fixture();
        let example = &f["internal_digest_example"];
        let input = example["input"].as_str().unwrap();
        assert_eq!(
            hex_encode(&sha256(input.as_bytes())),
            example["sha256_hex"].as_str().unwrap()
        );
    }

    #[test]
    fn internal_auth_cases_match_fixture() {
        let f = fixture();
        let config = f["internal_tokens_config"].as_str().unwrap();
        let tokens = parse_internal_tokens(config).unwrap();
        assert_eq!(tokens.len(), 2);

        for case in f["internal_auth_cases"].as_array().unwrap() {
            let authorization = case["authorization"].as_str().unwrap();
            let ok = case["ok"].as_bool().unwrap();
            let want_label = case["label"].as_str().unwrap();

            match authenticate_internal(authorization, &tokens) {
                Some(label) => {
                    assert!(ok, "expected reject for {authorization:?}");
                    assert_eq!(label, want_label, "label for {authorization:?}");
                }
                None => {
                    assert!(!ok, "expected accept for {authorization:?}");
                    assert_eq!(want_label, "", "reject rows carry an empty label");
                }
            }
        }
    }

    #[test]
    fn parse_internal_tokens_validates_entries() {
        // Hand-computed from the reference (Part 1 config table, internalTokens).
        assert_eq!(parse_internal_tokens(""), Err(InternalTokenError::Missing));
        assert_eq!(
            parse_internal_tokens("   "),
            Err(InternalTokenError::Missing)
        );
        assert_eq!(
            parse_internal_tokens("ops"),
            Err(InternalTokenError::Malformed)
        );
        assert_eq!(
            parse_internal_tokens(":alpha"),
            Err(InternalTokenError::Malformed)
        );
        assert_eq!(
            parse_internal_tokens("ops:"),
            Err(InternalTokenError::Malformed)
        );
        assert_eq!(
            parse_internal_tokens("ops:alpha,"),
            Err(InternalTokenError::Malformed)
        );

        // Duplicate labels are accepted.
        assert_eq!(parse_internal_tokens("ops:a,ops:b").unwrap().len(), 2);

        // A secret may contain ':'.
        let with_colon = parse_internal_tokens("db:pa:ss").unwrap();
        assert_eq!(with_colon.len(), 1);
        assert_eq!(with_colon[0].label(), "db");
        assert_eq!(with_colon[0].digest(), &sha256(b"db:pa:ss"));

        // Each entry is trimmed at both ends before the split.
        let trimmed = parse_internal_tokens(" ops:alpha , rails:beta ").unwrap();
        assert_eq!(trimmed[0].label(), "ops");
        assert_eq!(trimmed[1].label(), "rails");
        assert_eq!(trimmed[0].digest(), &sha256(b"ops:alpha"));
    }

    #[test]
    fn internal_token_error_messages_are_exact() {
        assert_eq!(
            InternalTokenError::Missing.to_string(),
            "required but not set"
        );
        assert_eq!(
            InternalTokenError::Malformed.to_string(),
            "must be a comma-separated list of \"label:secret\" pairs"
        );
    }
}
