//! Prompt fingerprint, debug-capture crypto (with the D18 key ring), and the pure
//! capture decision.
//!
//! This module is pure. The DB-backed capture flow (insert, replay, reads, access log)
//! lives in the API package. Reference Part 4 §4.1, §4.2, §4.4, §4.5; spec §10.1 and
//! §10.4. Parity fixtures: `parity/fixtures/decision/tracing_fingerprint.json`,
//! `tracing_capture_crypto.json`, and `tracing_capture_decide.json`.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use chrono::{DateTime, Duration, Utc};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Size of the capture master key and every derived data key.
pub const MASTER_KEY_SIZE: usize = 32;

/// Size of the AES-256-GCM nonce.
pub const NONCE_SIZE: usize = 12;

/// Prefix on every prompt fingerprint.
pub const FINGERPRINT_PREFIX: &str = "hmac_sha256:";

/// The prompt fingerprint (G5, G6):
/// `"hmac_sha256:" + lowercase_hex(HMAC-SHA256(key, prompt))`.
///
/// The message is the prompt's exact bytes. There is no Unicode normalization and no
/// trimming, so the NFC and NFD forms of the same text differ. The same value is the
/// classification cache key, the response `prompt_hash`, and the trace column.
pub fn fingerprint(prompt_hmac_key: &[u8], prompt: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(prompt_hmac_key)
        .expect("HMAC-SHA256 accepts a key of any length");
    mac.update(prompt);
    let code = mac.finalize().into_bytes();

    let mut out = String::with_capacity(FINGERPRINT_PREFIX.len() + code.len() * 2);
    out.push_str(FINGERPRINT_PREFIX);
    out.push_str(&lower_hex(&code));
    out
}

/// Why parsing `MAVERICK_CAPTURE_MASTER_KEY` failed. The config loader maps these to its
/// issue strings; the `Display` text is the exact string it reports.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Error)]
pub enum MasterKeyError {
    /// The value is unset or empty.
    #[error("required but not set")]
    Missing,
    /// The value is set but is not 64 hex characters encoding 32 bytes.
    #[error("must be 64 hex characters encoding a 32-byte key")]
    Invalid,
}

/// Parse a capture master key: exactly 64 hex characters (upper or lower case) encoding
/// 32 bytes. Empty input is [`MasterKeyError::Missing`]; any decode error or a decoded
/// length other than 32 is [`MasterKeyError::Invalid`].
pub fn parse_master_key(value: &str) -> Result<[u8; MASTER_KEY_SIZE], MasterKeyError> {
    if value.is_empty() {
        return Err(MasterKeyError::Missing);
    }
    let bytes = decode_hex(value).ok_or(MasterKeyError::Invalid)?;
    bytes.try_into().map_err(|_| MasterKeyError::Invalid)
}

/// Something went wrong sealing or opening a capture.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum CaptureError {
    /// A key that is not 32 bytes reached the cipher.
    #[error("tracing: capture master key must be 32 bytes, got {0}")]
    MasterKeySize(usize),
    /// Opening failed (tamper, wrong key, or wrong capture id). The prefix is fixed
    /// (D36); the detail is the underlying AEAD error text and is not compared.
    #[error("tracing: decrypting capture {capture_id}: {detail}")]
    Decrypt { capture_id: String, detail: String },
    /// The stored capture key version is neither the current nor the previous key.
    #[error("unknown capture key version {0}")]
    UnknownKeyVersion(u32),
}

/// Derive the per-capture data key: HKDF-SHA256 with IKM = master key, salt = the capture
/// id bytes, empty info, 32-byte output.
fn derive_data_key(master_key: &[u8], capture_id: &str) -> [u8; MASTER_KEY_SIZE] {
    let hkdf = Hkdf::<Sha256>::new(Some(capture_id.as_bytes()), master_key);
    let mut data_key = [0u8; MASTER_KEY_SIZE];
    hkdf.expand(&[], &mut data_key)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    data_key
}

/// Seal a prompt with a caller-supplied nonce. Deterministic, so the parity fixtures
/// drive it directly. Returns `ciphertext || 16-byte tag`.
pub fn seal_with_nonce(
    master_key: &[u8],
    capture_id: &str,
    nonce: &[u8; NONCE_SIZE],
    plaintext: &[u8],
) -> Result<Vec<u8>, CaptureError> {
    if master_key.len() != MASTER_KEY_SIZE {
        return Err(CaptureError::MasterKeySize(master_key.len()));
    }
    let data_key = derive_data_key(master_key, capture_id);
    let cipher = Aes256Gcm::new_from_slice(&data_key).expect("data key is 32 bytes");
    Ok(cipher
        .encrypt(&Nonce::from(*nonce), plaintext)
        .expect("AES-256-GCM encryption of a bounded prompt does not fail"))
}

/// Seal a prompt with a fresh CSPRNG nonce. Returns the nonce and `ciphertext || tag`.
pub fn seal(
    master_key: &[u8],
    capture_id: &str,
    plaintext: &[u8],
) -> Result<([u8; NONCE_SIZE], Vec<u8>), CaptureError> {
    use rand::Rng;
    let mut nonce = [0u8; NONCE_SIZE];
    rand::rng().fill_bytes(&mut nonce);
    let ciphertext = seal_with_nonce(master_key, capture_id, &nonce, plaintext)?;
    Ok((nonce, ciphertext))
}

/// Open a sealed capture. On any failure the error keeps the fixed
/// `tracing: decrypting capture <id>: ` prefix (D36).
pub fn open(
    master_key: &[u8],
    capture_id: &str,
    nonce: &[u8; NONCE_SIZE],
    ciphertext_with_tag: &[u8],
) -> Result<Vec<u8>, CaptureError> {
    if master_key.len() != MASTER_KEY_SIZE {
        return Err(CaptureError::MasterKeySize(master_key.len()));
    }
    let data_key = derive_data_key(master_key, capture_id);
    let cipher = Aes256Gcm::new_from_slice(&data_key).expect("data key is 32 bytes");
    cipher
        .decrypt(&Nonce::from(*nonce), ciphertext_with_tag)
        .map_err(|err| CaptureError::Decrypt {
            capture_id: capture_id.to_string(),
            detail: err.to_string(),
        })
}

/// A capture master key ring (D18): the current key plus at most one previous key.
/// Every capture stores the version it was sealed with. New captures use the current
/// key; opening selects the key by the stored version.
#[derive(Debug, Clone)]
pub struct KeyRing {
    current_version: u32,
    current_key: [u8; MASTER_KEY_SIZE],
    previous: Option<(u32, [u8; MASTER_KEY_SIZE])>,
}

impl KeyRing {
    /// Build a ring from the current key (with its version) and an optional previous key.
    pub fn new(
        current_version: u32,
        current_key: [u8; MASTER_KEY_SIZE],
        previous: Option<(u32, [u8; MASTER_KEY_SIZE])>,
    ) -> Self {
        Self {
            current_version,
            current_key,
            previous,
        }
    }

    /// The version new captures are sealed with.
    pub fn current_version(&self) -> u32 {
        self.current_version
    }

    fn key_for_version(&self, version: u32) -> Option<&[u8; MASTER_KEY_SIZE]> {
        if version == self.current_version {
            return Some(&self.current_key);
        }
        match &self.previous {
            Some((prev_version, prev_key)) if *prev_version == version => Some(prev_key),
            _ => None,
        }
    }

    /// Seal with the current key and a caller-supplied nonce. Returns the key version and
    /// `ciphertext || tag`.
    pub fn seal_with_nonce(
        &self,
        capture_id: &str,
        nonce: &[u8; NONCE_SIZE],
        plaintext: &[u8],
    ) -> Result<(u32, Vec<u8>), CaptureError> {
        let ciphertext = seal_with_nonce(&self.current_key, capture_id, nonce, plaintext)?;
        Ok((self.current_version, ciphertext))
    }

    /// Seal with the current key and a fresh nonce.
    pub fn seal(
        &self,
        capture_id: &str,
        plaintext: &[u8],
    ) -> Result<(u32, [u8; NONCE_SIZE], Vec<u8>), CaptureError> {
        let (nonce, ciphertext) = seal(&self.current_key, capture_id, plaintext)?;
        Ok((self.current_version, nonce, ciphertext))
    }

    /// Open a capture sealed under `version`. An unknown version is rejected before any
    /// decryption is attempted.
    pub fn open(
        &self,
        version: u32,
        capture_id: &str,
        nonce: &[u8; NONCE_SIZE],
        ciphertext_with_tag: &[u8],
    ) -> Result<Vec<u8>, CaptureError> {
        let key = self
            .key_for_version(version)
            .ok_or(CaptureError::UnknownKeyVersion(version))?;
        open(key, capture_id, nonce, ciphertext_with_tag)
    }
}

/// Whether a retired previous key can be dropped: true once
/// `max_window_minutes + retention_hours` have elapsed since rotation, the point after
/// which no live capture can still reference it.
pub fn previous_key_expired(
    rotated_at: DateTime<Utc>,
    now: DateTime<Utc>,
    max_window_minutes: i64,
    retention_hours: i64,
) -> bool {
    let ttl = Duration::minutes(max_window_minutes) + Duration::hours(retention_hours);
    now - rotated_at >= ttl
}

/// An active-capture window as the decision sees it.
#[derive(Debug, Clone)]
pub struct CaptureWindow {
    pub id: String,
    pub api_key_id: Option<String>,
    pub confidence_below: Option<f32>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

/// The capture decision: whether to capture and, if so, which window matched.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CaptureDecision {
    pub captured: bool,
    pub window_id: Option<String>,
}

/// Decide whether to capture a prompt.
///
/// Zero retention never captures (this fails closed, G5). Otherwise the first window in
/// list order wins that is active (`starts_at <= now < ends_at`), scoped to this key
/// (`api_key_id` null or equal), and under the confidence bar (`confidence_below` null or
/// `confidence < confidence_below`, strict). `confidence_below` is stored as float32, so
/// the comparison quantizes `confidence` to float32 too: a window created with 0.95 does
/// not capture a confidence of 0.95, but does capture 0.9499999.
pub fn decide(
    windows: &[CaptureWindow],
    zero_retention: bool,
    api_key_id: &str,
    confidence: f64,
    now: DateTime<Utc>,
) -> CaptureDecision {
    if zero_retention {
        return CaptureDecision {
            captured: false,
            window_id: None,
        };
    }

    let confidence = confidence as f32;
    for window in windows {
        let active = window.starts_at <= now && now < window.ends_at;
        let key_ok = window
            .api_key_id
            .as_deref()
            .is_none_or(|scoped| scoped == api_key_id);
        let confidence_ok = window.confidence_below.is_none_or(|bar| confidence < bar);
        if active && key_ok && confidence_ok {
            return CaptureDecision {
                captured: true,
                window_id: Some(window.id.clone()),
            };
        }
    }

    CaptureDecision {
        captured: false,
        window_id: None,
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    // `chunks_exact(2)` is a bounded iterator over byte pairs, so there is no manual
    // index to advance (and no way to mutate the step into a non-terminating loop).
    bytes
        .chunks_exact(2)
        .map(|pair| Some((hex_digit(pair[0])? << 4) + hex_digit(pair[1])?))
        .collect()
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const FINGERPRINT_FIXTURE: &str =
        include_str!("../../../parity/fixtures/decision/tracing_fingerprint.json");
    const CRYPTO_FIXTURE: &str =
        include_str!("../../../parity/fixtures/decision/tracing_capture_crypto.json");
    const DECIDE_FIXTURE: &str =
        include_str!("../../../parity/fixtures/decision/tracing_capture_decide.json");

    fn hex(s: &str) -> Vec<u8> {
        decode_hex(s).expect("test hex decodes")
    }

    fn nonce12(s: &str) -> [u8; NONCE_SIZE] {
        hex(s).try_into().expect("nonce is 12 bytes")
    }

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("rfc3339 timestamp")
            .with_timezone(&Utc)
    }

    // parity: tracing.TestFingerprintGoldenVector
    // parity: tracing.TestFingerprintFormat
    // parity: tracing.TestFingerprintRFC4231Vector
    // parity: tracing.TestFingerprintOneCharEditDiverges
    // parity: tracing.TestFingerprintCrossTenantDiverges
    // parity: tracing.TestFingerprintEmptyPrompt
    // parity: tracing.TestFingerprintNoUnicodeNormalization
    #[test]
    fn fingerprint_matches_fixture_rows() {
        let fixture: Value = serde_json::from_str(FINGERPRINT_FIXTURE).unwrap();
        assert_eq!(fixture["prefix"].as_str().unwrap(), FINGERPRINT_PREFIX);
        for case in fixture["cases"].as_array().unwrap() {
            let id = case["id"].as_str().unwrap();
            let key = hex(case["input"]["key_hex"].as_str().unwrap());
            let prompt = hex(case["input"]["prompt_utf8_hex"].as_str().unwrap());
            let got = fingerprint(&key, &prompt);
            assert_eq!(got, case["expected"].as_str().unwrap(), "case {id}");
        }
    }

    // parity: tracing.TestSealOpenRoundTrip
    #[test]
    fn capture_crypto_matches_fixture_rows() {
        let fixture: Value = serde_json::from_str(CRYPTO_FIXTURE).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let id = case["id"].as_str().unwrap();
            let input = &case["input"];
            let expected = &case["expected"];

            let master_key = hex(input["master_key_hex"].as_str().unwrap());
            let capture_id = input["capture_id"].as_str().unwrap();
            let nonce = nonce12(input["nonce_hex"].as_str().unwrap());
            let plaintext = hex(input["plaintext_utf8_hex"].as_str().unwrap());

            // HKDF data key.
            let data_key = derive_data_key(&master_key, capture_id);
            assert_eq!(
                data_key.to_vec(),
                hex(expected["data_key_hex"].as_str().unwrap()),
                "case {id}: data key"
            );

            // Seal reproduces the fixed-nonce vector.
            let sealed = seal_with_nonce(&master_key, capture_id, &nonce, &plaintext).unwrap();
            assert_eq!(
                sealed,
                hex(expected["ciphertext_with_tag_hex"].as_str().unwrap()),
                "case {id}: ciphertext"
            );
            assert_eq!(
                sealed.len(),
                expected["ciphertext_len"].as_u64().unwrap() as usize,
                "case {id}: length"
            );
            let tag = &sealed[sealed.len() - 16..];
            assert_eq!(
                tag,
                hex(expected["tag_hex"].as_str().unwrap()),
                "case {id}: tag"
            );

            // Open round-trips.
            let opened = open(&master_key, capture_id, &nonce, &sealed).unwrap();
            assert_eq!(opened, plaintext, "case {id}: round trip");
        }
    }

    // parity: tracing.TestOpenRejectsTamperedCiphertext
    // parity: tracing.TestOpenRejectsWrongCaptureID
    #[test]
    fn open_rejects_tampered_ciphertext() {
        let fixture: Value = serde_json::from_str(CRYPTO_FIXTURE).unwrap();
        let case = &fixture["cases"][0];
        let input = &case["input"];
        let master_key = hex(input["master_key_hex"].as_str().unwrap());
        let capture_id = input["capture_id"].as_str().unwrap();
        let nonce = nonce12(input["nonce_hex"].as_str().unwrap());
        let mut sealed = seal_with_nonce(
            &master_key,
            capture_id,
            &nonce,
            &hex(input["plaintext_utf8_hex"].as_str().unwrap()),
        )
        .unwrap();

        sealed[0] ^= 0x01;
        let err = open(&master_key, capture_id, &nonce, &sealed).unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with(&format!("tracing: decrypting capture {capture_id}: ")),
            "prefix preserved (D36): {message}"
        );

        // A different capture id cannot open it either.
        let wrong = open(&master_key, "cap_other", &nonce, &sealed).unwrap_err();
        assert!(
            wrong
                .to_string()
                .starts_with("tracing: decrypting capture cap_other: ")
        );
    }

    // parity: tracing.TestSealNoncesAndCiphertextsDiffer
    #[test]
    fn seal_uses_a_fresh_nonce_each_time() {
        let master_key = [0x11u8; MASTER_KEY_SIZE];
        let (nonce_a, ct_a) = seal(&master_key, "cap_x", b"prompt").unwrap();
        let (nonce_b, ct_b) = seal(&master_key, "cap_x", b"prompt").unwrap();
        assert_ne!(nonce_a, nonce_b);
        assert_ne!(ct_a, ct_b);
        // Both still open under the same key and id.
        assert_eq!(
            open(&master_key, "cap_x", &nonce_a, &ct_a).unwrap(),
            b"prompt"
        );
        assert_eq!(
            open(&master_key, "cap_x", &nonce_b, &ct_b).unwrap(),
            b"prompt"
        );
    }

    #[test]
    fn parse_master_key_validates() {
        // 64 hex chars encoding 32 bytes.
        let valid = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let key = parse_master_key(valid).unwrap();
        assert_eq!(key.len(), MASTER_KEY_SIZE);
        assert_eq!(key[0], 0x00);
        assert_eq!(key[31], 0x1f);
        // Upper case is accepted.
        assert_eq!(parse_master_key(&valid.to_uppercase()).unwrap(), key);

        assert_eq!(parse_master_key(""), Err(MasterKeyError::Missing));
        assert_eq!(parse_master_key("00").unwrap_err(), MasterKeyError::Invalid); // 1 byte
        assert_eq!(parse_master_key("0g").unwrap_err(), MasterKeyError::Invalid); // non-hex
        assert_eq!(
            parse_master_key("000").unwrap_err(),
            MasterKeyError::Invalid
        ); // odd length
        // 33 bytes.
        let long = format!("{valid}00");
        assert_eq!(
            parse_master_key(&long).unwrap_err(),
            MasterKeyError::Invalid
        );
    }

    // parity: tracing.TestSealRejectsShortMasterKey
    #[test]
    fn master_key_size_is_checked_at_use() {
        let short = [0u8; 31];
        let err = seal_with_nonce(&short, "cap_x", &[0u8; NONCE_SIZE], b"p").unwrap_err();
        assert_eq!(err, CaptureError::MasterKeySize(31));
        assert_eq!(
            err.to_string(),
            "tracing: capture master key must be 32 bytes, got 31"
        );
        assert_eq!(
            open(&short, "cap_x", &[0u8; NONCE_SIZE], b"").unwrap_err(),
            CaptureError::MasterKeySize(31)
        );
    }

    #[test]
    fn error_messages_are_exact() {
        assert_eq!(MasterKeyError::Missing.to_string(), "required but not set");
        assert_eq!(
            MasterKeyError::Invalid.to_string(),
            "must be 64 hex characters encoding a 32-byte key"
        );
        assert_eq!(
            CaptureError::UnknownKeyVersion(7).to_string(),
            "unknown capture key version 7"
        );
    }

    // parity: tracing.TestOpenRejectsWrongMasterKey
    #[test]
    fn key_ring_seals_with_current_and_opens_by_version() {
        let v1 = [0x11u8; MASTER_KEY_SIZE];
        let v2 = [0x22u8; MASTER_KEY_SIZE];
        let ring = KeyRing::new(2, v2, Some((1, v1)));
        assert_eq!(ring.current_version(), 2);

        // Seal with the current key (v2).
        let nonce = [0u8; NONCE_SIZE];
        let (version, sealed) = ring.seal_with_nonce("cap_z", &nonce, b"secret").unwrap();
        assert_eq!(version, 2);
        assert_eq!(ring.open(2, "cap_z", &nonce, &sealed).unwrap(), b"secret");

        // A v1 capture (sealed under the previous key directly) opens via the ring.
        let v1_capture = seal_with_nonce(&v1, "cap_old", &nonce, b"legacy").unwrap();
        assert_eq!(
            ring.open(1, "cap_old", &nonce, &v1_capture).unwrap(),
            b"legacy"
        );

        // An unknown version is rejected before decryption.
        assert_eq!(
            ring.open(9, "cap_z", &nonce, &sealed).unwrap_err(),
            CaptureError::UnknownKeyVersion(9)
        );

        // Opening a v2 capture as v1 fails to decrypt (wrong key), not with a version error.
        let err = ring.open(1, "cap_z", &nonce, &sealed).unwrap_err();
        assert!(matches!(err, CaptureError::Decrypt { .. }));
    }

    #[test]
    fn key_ring_seal_uses_fresh_nonce_and_round_trips() {
        let ring = KeyRing::new(5, [0x44u8; MASTER_KEY_SIZE], None);
        let (v1, n1, c1) = ring.seal("cap_r", b"hello").unwrap();
        let (v2, n2, c2) = ring.seal("cap_r", b"hello").unwrap();
        assert_eq!(v1, 5, "seal reports the current version");
        assert_eq!(v2, 5);
        assert_ne!(n1, n2, "each seal draws a fresh nonce");
        assert_ne!(c1, c2);
        assert_eq!(ring.open(v1, "cap_r", &n1, &c1).unwrap(), b"hello");
        assert_eq!(ring.open(v2, "cap_r", &n2, &c2).unwrap(), b"hello");
    }

    #[test]
    fn key_ring_without_previous_rejects_old_versions() {
        let ring = KeyRing::new(3, [0x33u8; MASTER_KEY_SIZE], None);
        assert_eq!(
            ring.open(1, "cap_z", &[0u8; NONCE_SIZE], b"").unwrap_err(),
            CaptureError::UnknownKeyVersion(1)
        );
    }

    #[test]
    fn previous_key_drop_rule() {
        let rotated = ts("2026-07-07T00:00:00Z");
        // Defaults: max_window_minutes 1440 (24h) + retention_hours 72 = 96h total.
        assert!(!previous_key_expired(
            rotated,
            ts("2026-07-10T23:59:00Z"),
            1440,
            72
        )); // 95h59m
        assert!(previous_key_expired(
            rotated,
            ts("2026-07-11T00:00:00Z"),
            1440,
            72
        )); // exactly 96h
        assert!(previous_key_expired(
            rotated,
            ts("2026-07-12T00:00:00Z"),
            1440,
            72
        )); // later
    }

    // parity: tracing.TestDecide
    #[test]
    fn decide_matches_fixture_rows() {
        let fixture: Value = serde_json::from_str(DECIDE_FIXTURE).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let id = case["id"].as_str().unwrap();
            let input = &case["input"];

            let windows: Vec<CaptureWindow> = input["windows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| CaptureWindow {
                    id: w["id"].as_str().unwrap().to_string(),
                    api_key_id: w["api_key_id"].as_str().map(str::to_string),
                    confidence_below: w["confidence_below"].as_f64().map(|v| v as f32),
                    starts_at: ts(w["starts_at"].as_str().unwrap()),
                    ends_at: ts(w["ends_at"].as_str().unwrap()),
                })
                .collect();

            let got = decide(
                &windows,
                input["zero_retention"].as_bool().unwrap(),
                input["api_key_id"].as_str().unwrap(),
                input["confidence"].as_f64().unwrap(),
                ts(input["now"].as_str().unwrap()),
            );

            let expected = &case["expected"];
            assert_eq!(
                got.captured,
                expected["captured"].as_bool().unwrap(),
                "case {id}: captured"
            );
            assert_eq!(
                got.window_id.as_deref(),
                expected["window_id"].as_str(),
                "case {id}: window_id"
            );
        }
    }
}
