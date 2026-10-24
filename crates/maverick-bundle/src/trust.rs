//! The bundle public keys this release trusts (spec §14.4 "Signing keys", D49;
//! self-hosted plan §5.3).
//!
//! A replica verifies a bundle against [`release_keys`] and nothing else: no setting adds
//! or replaces a trusted key, so a changed environment file cannot make a replica accept a
//! forged bundle. The table holds the current key, one standby key, and every retiring key
//! that signed a bundle in the last 12 months. The owner makes the key pairs and keeps the
//! private halves; this file holds only public keys the owner provided.
//!
//! **Rotation.** A release adds the new key as `Current` and marks the old one
//! `Retiring`; the origin then signs every bundle with both for 12 months, then stops
//! signing with the old key, and a later release removes its row. **Compromise.** The
//! origin signs with the standby key at once; the next release drops the leaked key, makes
//! the standby `Current`, and adds a new standby. `docs/operations.md` "Bundle signing
//! keys" has the procedures and the openssl commands.

use std::collections::BTreeMap;
use std::fmt;

use ed25519_dalek::VerifyingKey;

/// Where a trusted key is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStatus {
    /// The key the origin signs with.
    Current,
    /// Kept offline; the origin signs with it only after the current key leaks.
    Standby,
    /// Replaced by a newer current key; still trusted while its bundles are in the window.
    Retiring,
}

/// One compiled-in public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedKey {
    /// The `signatures/<kid>` file name: 1 to 64 ASCII letters, digits, `_`, or `-`.
    pub kid: &'static str,
    /// The raw 32-byte Ed25519 public key, as 64 lowercase hex digits.
    pub public_key: &'static str,
    /// The release that added the key.
    pub added_in: &'static str,
    /// The last day the origin signs with it (`YYYY-MM-DD`), once that is decided.
    pub signs_until: Option<&'static str>,
    pub status: KeyStatus,
}

/// The release's trusted keys. Empty until the owner provides the real public keys (bead
/// model-maverick-tgm2): until then a replica imports no bundle and serves cold start.
/// A release build checks this table with [`check_release_table`].
pub const TRUSTED_KEYS: &[TrustedKey] = &[];

/// Why a trust table is not fit for a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustTableError {
    /// A kid outside the allowed alphabet or length.
    Kid(&'static str),
    /// A kid listed twice.
    DuplicateKid(&'static str),
    /// A key that is not 64 hex digits or not a valid Ed25519 public key.
    Key(&'static str),
    /// A test kid (`test-...`) in the release table.
    TestKid(&'static str),
    /// Not exactly one `Current` and one `Standby` key.
    Shape { current: usize, standby: usize },
}

impl fmt::Display for TrustTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kid(kid) => write!(f, "trusted key kid {kid:?} is not 1 to 64 of [A-Za-z0-9_-]"),
            Self::DuplicateKid(kid) => write!(f, "trusted key kid {kid:?} is listed twice"),
            Self::Key(kid) => write!(f, "trusted key {kid:?} is not a valid Ed25519 public key"),
            Self::TestKid(kid) => write!(f, "test key {kid:?} is in the release table"),
            Self::Shape { current, standby } => write!(
                f,
                "a release trusts exactly one current and one standby key, not {current} and {standby}"
            ),
        }
    }
}

impl std::error::Error for TrustTableError {}

fn decode_hex32(hex: &str) -> Option<[u8; 32]> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        out[index] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

fn valid_kid(kid: &str) -> bool {
    !kid.is_empty()
        && kid.len() <= 64
        && kid
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// The decoded keys of `table`, after checking each row: a valid kid, listed once, and a
/// valid Ed25519 public key.
pub fn decode_table(
    table: &[TrustedKey],
) -> Result<BTreeMap<String, VerifyingKey>, TrustTableError> {
    let mut keys = BTreeMap::new();
    for row in table {
        if !valid_kid(row.kid) {
            return Err(TrustTableError::Kid(row.kid));
        }
        let key = decode_hex32(row.public_key)
            .and_then(|bytes| VerifyingKey::from_bytes(&bytes).ok())
            .ok_or(TrustTableError::Key(row.kid))?;
        if keys.insert(row.kid.to_owned(), key).is_some() {
            return Err(TrustTableError::DuplicateKid(row.kid));
        }
    }
    Ok(keys)
}

/// A table fit for a release: every row decodes, no test kid, and exactly one `Current`
/// and one `Standby` key (any number of `Retiring` keys).
pub fn check_release_table(table: &[TrustedKey]) -> Result<(), TrustTableError> {
    decode_table(table)?;
    if let Some(row) = table.iter().find(|row| row.kid.starts_with("test-")) {
        return Err(TrustTableError::TestKid(row.kid));
    }
    let count = |status| table.iter().filter(|row| row.status == status).count();
    let (current, standby) = (count(KeyStatus::Current), count(KeyStatus::Standby));
    if (current, standby) != (1, 1) {
        return Err(TrustTableError::Shape { current, standby });
    }
    Ok(())
}

/// The keys a replica verifies bundles against: [`TRUSTED_KEYS`], decoded. Never a test
/// key, whatever features are on.
#[must_use]
pub fn release_keys() -> BTreeMap<String, VerifyingKey> {
    decode_table(TRUSTED_KEYS)
        .expect("TRUSTED_KEYS is checked by maverick-bundle's trust tests")
        .into_iter()
        .filter(|(kid, _)| !kid.starts_with("test-"))
        .collect()
}

/// Fixed test key pairs for bundle tests in this and other crates (the `test-keys`
/// feature, for dev-dependencies). Their kids start with `test-`, which a release table
/// refuses, and [`release_keys`] never returns them.
#[cfg(any(test, feature = "test-keys"))]
pub mod test_keys {
    use std::collections::BTreeMap;

    use ed25519_dalek::{SigningKey, VerifyingKey};

    use crate::SigningKeys;

    pub const CURRENT: &str = "test-current";
    pub const STANDBY: &str = "test-standby";
    pub const RETIRING: &str = "test-retiring";
    /// A key no release trusts.
    pub const UNKNOWN: &str = "test-unknown";

    fn seed(kid: &str) -> [u8; 32] {
        match kid {
            CURRENT => [0x11; 32],
            STANDBY => [0x22; 32],
            RETIRING => [0x33; 32],
            UNKNOWN => [0x44; 32],
            other => panic!("no test key {other:?}"),
        }
    }

    /// The private key of a test kid.
    #[must_use]
    pub fn signing_key(kid: &str) -> SigningKey {
        SigningKey::from_bytes(&seed(kid))
    }

    /// The signing set an origin with these test kids uses.
    ///
    /// # Panics
    /// When `kids` is empty or names no test key.
    #[must_use]
    pub fn signing(kids: &[&str]) -> SigningKeys {
        SigningKeys::new(
            kids.iter()
                .map(|kid| ((*kid).to_owned(), signing_key(kid)))
                .collect(),
        )
        .expect("test signing keys")
    }

    /// The trust set of a release that carries these test kids.
    #[must_use]
    pub fn trust(kids: &[&str]) -> BTreeMap<String, VerifyingKey> {
        kids.iter()
            .map(|kid| ((*kid).to_owned(), signing_key(kid).verifying_key()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid public key's hex, from a fixed seed.
    fn hex_of(seed: u8) -> &'static str {
        let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]).verifying_key();
        let hex: String = key.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        Box::leak(hex.into_boxed_str())
    }

    fn row(kid: &'static str, seed: u8, status: KeyStatus) -> TrustedKey {
        TrustedKey {
            kid,
            public_key: hex_of(seed),
            added_in: "0.1.0",
            signs_until: None,
            status,
        }
    }

    /// The compiled-in table always decodes, never holds a test kid, and has at most one
    /// current and one standby key. A release build (`MAVERICK_RELEASE_BUILD=1`, set by
    /// `scripts/release-artifacts.sh`) also requires exactly one of each, so a tag cannot
    /// ship before the owner's real keys are in the table.
    #[test]
    fn the_release_table_is_well_formed() {
        decode_table(TRUSTED_KEYS).expect("every row decodes");
        assert!(
            TRUSTED_KEYS.iter().all(|row| !row.kid.starts_with("test-")),
            "a test kid is in TRUSTED_KEYS"
        );
        let count = |status| TRUSTED_KEYS.iter().filter(|r| r.status == status).count();
        assert!(count(KeyStatus::Current) <= 1 && count(KeyStatus::Standby) <= 1);
        if std::env::var_os("MAVERICK_RELEASE_BUILD").is_some() {
            check_release_table(TRUSTED_KEYS).expect("a release needs the owner's keys");
        }
    }

    #[test]
    fn a_release_table_needs_one_current_and_one_standby_key() {
        let current = row("mm-2026-10", 1, KeyStatus::Current);
        let standby = row("mm-standby-2026-10", 2, KeyStatus::Standby);
        let retiring = row("mm-2025-10", 3, KeyStatus::Retiring);
        assert_eq!(check_release_table(&[current, standby]), Ok(()));
        assert_eq!(check_release_table(&[retiring, current, standby]), Ok(()));
        assert_eq!(
            check_release_table(&[]),
            Err(TrustTableError::Shape {
                current: 0,
                standby: 0
            })
        );
        assert_eq!(
            check_release_table(&[current]),
            Err(TrustTableError::Shape {
                current: 1,
                standby: 0
            })
        );
        let second = row("mm-2026-11", 4, KeyStatus::Current);
        assert_eq!(
            check_release_table(&[current, second, standby]),
            Err(TrustTableError::Shape {
                current: 2,
                standby: 1
            })
        );
    }

    #[test]
    fn a_bad_row_is_refused() {
        let standby = row("mm-standby", 2, KeyStatus::Standby);
        let current = row("mm-current", 1, KeyStatus::Current);
        assert_eq!(
            check_release_table(&[current, current, standby]),
            Err(TrustTableError::DuplicateKid("mm-current"))
        );
        for kid in ["", "has space", "dot.kid", &*"k".repeat(65).leak()] {
            let bad = TrustedKey { kid, ..current };
            assert_eq!(
                decode_table(&[bad]),
                Err(TrustTableError::Kid(kid)),
                "{kid:?}"
            );
        }
        for key in ["", "00", &*"g".repeat(64).leak(), &*"A".repeat(64).leak()] {
            let bad = TrustedKey {
                public_key: key,
                ..current
            };
            assert_eq!(
                decode_table(&[bad]),
                Err(TrustTableError::Key("mm-current"))
            );
        }
        let test = TrustedKey {
            kid: "test-current",
            ..current
        };
        assert_eq!(
            check_release_table(&[test, standby]),
            Err(TrustTableError::TestKid("test-current"))
        );
    }

    #[test]
    fn hex_decodes_exactly_32_lowercase_bytes() {
        let mut want = [0u8; 32];
        want[0] = 0xab;
        want[31] = 0x09;
        let hex = format!("ab{}09", "00".repeat(30));
        assert_eq!(decode_hex32(&hex), Some(want));
        assert_eq!(decode_hex32(&hex[..62]), None);
        assert_eq!(decode_hex32(&format!("{hex}00")), None);
        assert_eq!(decode_hex32(&hex.to_uppercase()), None);
    }

    #[test]
    fn release_keys_never_hold_a_test_key() {
        assert!(release_keys().keys().all(|kid| !kid.starts_with("test-")));
        assert_eq!(release_keys().len(), TRUSTED_KEYS.len());
    }
}

/// DIVERGENCE: D49. Rotation and compromise at the verify step: a bundle imports on a
/// release that knows at least one of its signing kids, and only then.
#[cfg(test)]
mod rotation {
    use std::collections::BTreeMap;

    use chrono::{TimeZone, Utc};

    use super::release_keys;
    use super::test_keys::{self, CURRENT, RETIRING, STANDBY, UNKNOWN};
    use crate::{FORMAT, Manifest, PublicMatrix, SchemaRange, build, verify};

    fn bundle(signers: &[&str]) -> Vec<u8> {
        let manifest = Manifest {
            format: FORMAT.to_owned(),
            matrix_version: "public-v1".to_owned(),
            published_at: Utc.with_ymd_and_hms(2026, 10, 24, 3, 0, 0).unwrap(),
            origin_release: "0.1.0".to_owned(),
            schema: SchemaRange { min: 11, max: 12 },
            files: BTreeMap::new(),
        };
        build(
            manifest,
            &PublicMatrix::default(),
            &test_keys::signing(signers),
        )
        .expect("bundle")
    }

    /// The kids that verified, or `None` when the bundle is refused.
    fn imports(bytes: &[u8], release: &[&str]) -> Option<Vec<String>> {
        verify(bytes, &test_keys::trust(release), 12)
            .ok()
            .map(|bundle| bundle.signing_kids)
    }

    #[test]
    fn a_rotation_imports_on_every_release_that_knows_one_signer() {
        // Three releases: before the rotation, during it, and after the old key is gone.
        let before = [RETIRING, STANDBY];
        let during = [RETIRING, CURRENT, STANDBY];
        let after = [CURRENT, STANDBY];

        let old_only = bundle(&[RETIRING]);
        assert_eq!(imports(&old_only, &before), Some(vec![RETIRING.to_owned()]));
        assert_eq!(imports(&old_only, &during), Some(vec![RETIRING.to_owned()]));
        assert_eq!(imports(&old_only, &after), None);

        let new_only = bundle(&[CURRENT]);
        assert_eq!(imports(&new_only, &before), None);
        assert_eq!(imports(&new_only, &during), Some(vec![CURRENT.to_owned()]));
        assert_eq!(imports(&new_only, &after), Some(vec![CURRENT.to_owned()]));

        // The origin signs with both keys for the whole window.
        let both = bundle(&[RETIRING, CURRENT]);
        assert_eq!(imports(&both, &before), Some(vec![RETIRING.to_owned()]));
        let mut verified = imports(&both, &during).expect("during");
        verified.sort();
        assert_eq!(verified, [CURRENT, RETIRING]);
        assert_eq!(imports(&both, &after), Some(vec![CURRENT.to_owned()]));
    }

    #[test]
    fn after_a_leak_the_standby_key_keeps_every_release_importing() {
        let standby_only = bundle(&[STANDBY]);
        for release in [[RETIRING, STANDBY], [CURRENT, STANDBY]] {
            assert_eq!(
                imports(&standby_only, &release),
                Some(vec![STANDBY.to_owned()])
            );
        }
    }

    #[test]
    fn a_bundle_signed_only_by_an_unknown_kid_is_refused() {
        let unknown = bundle(&[UNKNOWN]);
        assert_eq!(imports(&unknown, &[CURRENT, STANDBY, RETIRING]), None);
        // An unknown signature next to a known one is ignored.
        let mixed = bundle(&[UNKNOWN, CURRENT]);
        assert_eq!(
            imports(&mixed, &[CURRENT, STANDBY]),
            Some(vec![CURRENT.to_owned()])
        );
    }

    /// A release build trusts no test key, so a test-signed bundle never imports there.
    #[test]
    fn a_test_signed_bundle_fails_against_the_release_keys() {
        let signed = bundle(&[CURRENT, STANDBY, RETIRING]);
        assert!(verify(&signed, &release_keys(), 12).is_err());
    }
}
