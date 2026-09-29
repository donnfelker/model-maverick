//! ID scheme: `prefix + "_" + ULID`.
//!
//! Ports `internal/store/ids`. [`new`] returns `"{prefix}_{ULID}"`, where the ULID is
//! the 26-character Crockford base32 string (upper case, alphabet
//! `0123456789ABCDEFGHJKMNPQRSTVWXYZ`). One process-wide monotonic generator mints
//! every ULID, so IDs minted in one process sort by creation time as text. Keyset
//! pagination relies on that ordering (reference part 1, "Ordering reliance").
//!
//! The generator draws entropy from a CSPRNG. The Go engine seeded `math/rand` with
//! the process start time (QUIRK 15); parity does not require matching the entropy
//! source, only the format and the in-process ordering.
//!
//! Any string is accepted as a prefix and it is not validated. Task IDs are authored
//! slugs from corpus YAML (for example `task_sx_017`), not minted here; the [`prefix::TASK`]
//! constant records the convention only.

use std::sync::{Mutex, PoisonError};

use ulid::{Generator, Ulid};

/// The exact ID prefixes. See reference part 1, "Prefix table (exact)".
pub mod prefix {
    /// `tenants.id`
    pub const TENANT: &str = "ten";
    /// `api_keys.id`
    pub const API_KEY: &str = "key";
    /// `model_columns.id`
    pub const MODEL_COLUMN: &str = "col";
    /// `pools.id`
    pub const POOL: &str = "pool";
    /// `tasks.id`. Authored slug from corpus YAML, not minted by [`super::new`].
    pub const TASK: &str = "task";
    /// `runs.id`
    pub const RUN: &str = "run";
    /// `recommendations.id`
    pub const RECOMMENDATION: &str = "rec";
    /// `outcomes.id`
    pub const OUTCOME: &str = "out";
    /// `capture_windows.id`
    pub const CAPTURE_WINDOW: &str = "cw";
    /// `debug_captures.id`
    pub const DEBUG_CAPTURE: &str = "cap";
    /// `capture_access_log.id`
    pub const CAPTURE_ACCESS: &str = "cal";
    /// `alerts.id`
    pub const ALERT: &str = "alr";
    /// `runs.batch_id` (one per batch execution).
    pub const BATCH: &str = "bat";
}

/// One process-wide monotonic ULID generator, matching the Go engine's single
/// mutex-guarded source. Both `Mutex::new` and `Generator::new` are `const`.
static GENERATOR: Mutex<Generator> = Mutex::new(Generator::new());

/// Mint an ID: `"{prefix}_{ULID}"`.
///
/// The prefix is used verbatim and is not validated. Pass one of the [`prefix`]
/// constants for engine IDs, or any string for ad-hoc IDs.
#[must_use]
pub fn new(prefix: &str) -> String {
    format!("{prefix}_{}", mint_ulid())
}

/// Draw the next monotonic ULID from the process-wide generator.
fn mint_ulid() -> Ulid {
    // Only this module touches GENERATOR, and neither branch below panics while the
    // lock is held, so the mutex cannot actually be poisoned; recover if it ever is.
    let mut generator = GENERATOR.lock().unwrap_or_else(PoisonError::into_inner);
    match generator.generate() {
        Ok(ulid) => ulid,
        // Random-bit overflow inside one millisecond is practically unreachable (80
        // bits of entropy). Step into the next millisecond rather than panic; the
        // strict ordering guarantee still holds.
        Err(overflow) => overflow.commit_overflow_increment(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Crockford base32, upper case, no I/L/O/U. Matches the fixture's `ulid_alphabet`
    /// and the regex char class `[0-9A-HJKMNP-TV-Z]`.
    const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    /// Load the parity fixture `parity/fixtures/store/ids.json` relative to this crate.
    fn ids_fixture() -> serde_json::Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../parity/fixtures/store/ids.json"
        );
        let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&raw).expect("parse ids.json")
    }

    /// Our prefix constants and alphabet equal the recorded parity fixture.
    #[test]
    fn prefix_constants_match_fixture() {
        let fx = ids_fixture();
        let prefixes = &fx["prefixes"];
        let expected = [
            ("PrefixTenant", prefix::TENANT),
            ("PrefixAPIKey", prefix::API_KEY),
            ("PrefixModelColumn", prefix::MODEL_COLUMN),
            ("PrefixPool", prefix::POOL),
            ("PrefixTask", prefix::TASK),
            ("PrefixRun", prefix::RUN),
            ("PrefixRecommendation", prefix::RECOMMENDATION),
            ("PrefixOutcome", prefix::OUTCOME),
            ("PrefixCaptureWindow", prefix::CAPTURE_WINDOW),
            ("PrefixDebugCapture", prefix::DEBUG_CAPTURE),
            ("PrefixCaptureAccess", prefix::CAPTURE_ACCESS),
            ("PrefixAlert", prefix::ALERT),
            ("PrefixBatch", prefix::BATCH),
        ];
        for (key, want) in expected {
            assert_eq!(prefixes[key].as_str(), Some(want), "prefix {key}");
        }
        assert_eq!(fx["ulid_alphabet"].as_str(), Some(CROCKFORD));
    }

    /// ids.TestNewFormat: `new("run")` is `run_` + 26 upper-case Crockford chars.
    // parity: store/ids.TestNewFormat
    #[test]
    fn new_format() {
        let id = new(prefix::RUN);
        let (p, rest) = id.split_once('_').expect("id has an underscore");
        assert_eq!(p, "run");
        assert_eq!(rest.len(), 26, "ULID body is 26 chars");
        assert!(
            rest.chars().all(|c| CROCKFORD.contains(c)),
            "ULID body is upper-case Crockford base32: {rest}"
        );
    }

    /// ids.TestNewUnique: 8 threads * 200 mints are all distinct (1600 total).
    // parity: store/ids.TestNewUnique
    #[test]
    fn new_unique_concurrent() {
        let threads = 8usize;
        let per_thread = 200usize;
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::spawn(move || {
                    (0..per_thread)
                        .map(|_| new(prefix::BATCH))
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        let mut seen = HashSet::new();
        for handle in handles {
            for id in handle.join().expect("mint thread joined") {
                assert!(seen.insert(id), "minted a duplicate id");
            }
        }
        assert_eq!(
            seen.len(),
            threads * per_thread,
            "expected 1600 distinct ids"
        );
    }

    /// Reference part 1, "Ordering reliance": ids minted in one process sort by
    /// creation time as text. The monotonic generator makes mint order == lexical order.
    #[test]
    fn ids_sort_by_creation_order() {
        let minted: Vec<String> = (0..1000).map(|_| new(prefix::RECOMMENDATION)).collect();
        let mut sorted = minted.clone();
        sorted.sort();
        assert_eq!(minted, sorted, "mint order must equal lexical order");
    }

    /// Any prefix is accepted and is not validated (the Go tests mint ad-hoc `cv`, `m`).
    #[test]
    fn arbitrary_prefixes_are_accepted() {
        for p in ["cv", "m"] {
            let id = new(p);
            let (got, rest) = id.split_once('_').expect("id has an underscore");
            assert_eq!(got, p);
            assert_eq!(rest.len(), 26);
        }
    }
}
