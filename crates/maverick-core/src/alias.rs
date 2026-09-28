//! The alias grammar: parse a `model` string into `{pool, mode, settings_mode}`.
//!
//! Ports `ParseAlias` (reference part 6 §5; master plan §3.5; spec §7.4). The grammar
//! lives here in `maverick-core` so the proxy, the SDK cores, and the engine all parse
//! aliases identically.
//!
//! Grammar (exact, case sensitive):
//! 1. The string must start with the exact prefix `maverick/`, else [`AliasError::NotMaverickAlias`].
//! 2. `rest` is everything after the prefix.
//! 3. If `rest` contains `:`, split at the FIRST `:` into `pool` and the mode word. If
//!    either part is empty, the alias is [`AliasError::MalformedAlias`]. With no `:`,
//!    there is no pool and the whole `rest` is the mode word.
//! 4. The mode word is exact: `auto` is [`Mode::Balanced`], `quality` is [`Mode::Quality`],
//!    `cost` is [`Mode::Cost`]. Anything else (including `balanced`, `AUTO`, ``, ` auto`)
//!    is [`AliasError::MalformedAlias`].
//!
//! A pool is any non-empty string without `:` (spaces and `/` are allowed, nothing is
//! trimmed). A concrete model name (no `maverick/` prefix) parses to `NotMaverickAlias`;
//! the caller decides what to do (the proxy rejects it 400, an SDK passes it through).
//!
//! DIVERGENCE D14: this port also accepts a trailing `?settings=caller` suffix
//! (`maverick/<pool>:<mode>?settings=caller`). It opts the caller out of applying the
//! recommended entry's `settings`, recorded as [`SettingsMode::Caller`]; the default is
//! [`SettingsMode::Apply`]. In Go this whole string is `malformed_alias` because the mode
//! word (for example `cost?settings=caller`) is not recognized. See `parity/DIVERGENCES.md` D14.

/// The engine ranking mode an alias selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `auto`.
    Balanced,
    /// `quality`.
    Quality,
    /// `cost`.
    Cost,
}

/// Whether the recommended entry's settings apply, or the caller's are left alone (D14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsMode {
    /// Apply the recommended entry's `settings` (the default).
    Apply,
    /// Leave the caller's settings alone (`?settings=caller`).
    Caller,
}

/// A parsed Maverick alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    /// The pool name, or `None` when the alias names no pool (`maverick/<mode>`).
    pub pool: Option<String>,
    /// The ranking mode.
    pub mode: Mode,
    /// The settings behavior (D14).
    pub settings_mode: SettingsMode,
}

/// Why a `model` string is not a usable Maverick alias. Both variants surface as the
/// identical HTTP 400 `invalid_model_alias` in the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AliasError {
    /// The string is not a Maverick alias (no `maverick/` prefix).
    #[error("not_maverick_alias")]
    NotMaverickAlias,
    /// The string starts with `maverick/` but the pool or mode word is malformed.
    #[error("malformed_alias")]
    MalformedAlias,
}

/// The exact, case-sensitive alias prefix.
const PREFIX: &str = "maverick/";
/// The D14 caller settings opt-out suffix.
const SETTINGS_CALLER_SUFFIX: &str = "?settings=caller";

/// Parse a `model` string into an [`Alias`], or an [`AliasError`].
///
/// This function is total: it never panics on any input.
pub fn parse_alias(model: &str) -> Result<Alias, AliasError> {
    // D14: strip an optional trailing `?settings=caller` first; the remainder is parsed
    // with the Go grammar. In Go the whole string is `malformed_alias`.
    let (core, settings_mode) = match model.strip_suffix(SETTINGS_CALLER_SUFFIX) {
        Some(head) => (head, SettingsMode::Caller),
        None => (model, SettingsMode::Apply),
    };

    // 1. Exact, case-sensitive `maverick/` prefix.
    let rest = core
        .strip_prefix(PREFIX)
        .ok_or(AliasError::NotMaverickAlias)?;

    // 3. Split on the FIRST `:` into pool and mode word; empty parts are malformed. With
    //    no `:`, there is no pool and the whole rest is the mode word.
    let (pool, mode_word) = match rest.split_once(':') {
        Some((pool, mode_word)) => {
            if pool.is_empty() || mode_word.is_empty() {
                return Err(AliasError::MalformedAlias);
            }
            (Some(pool.to_owned()), mode_word)
        }
        None => (None, rest),
    };

    // 4. Mode word is exact and case sensitive.
    let mode = match mode_word {
        "auto" => Mode::Balanced,
        "quality" => Mode::Quality,
        "cost" => Mode::Cost,
        _ => return Err(AliasError::MalformedAlias),
    };

    Ok(Alias {
        pool,
        mode,
        settings_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `parse_alias` rows recorded in `parity/fixtures/proxy/unit.json`.
    fn parse_alias_rows() -> Vec<serde_json::Value> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../parity/fixtures/proxy/unit.json"
        );
        let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse unit.json");
        doc["parse_alias"]
            .as_array()
            .expect("parse_alias is an array")
            .clone()
    }

    fn mode_name(mode: Mode) -> &'static str {
        match mode {
            Mode::Balanced => "balanced",
            Mode::Quality => "quality",
            Mode::Cost => "cost",
        }
    }

    /// Drives every `parse_alias-*` row from the parity fixture. These are the Go-parity
    /// rows and carry no suffix, so `settings_mode` is always `Apply` here.
    #[test]
    fn parse_alias_parity_rows() {
        let rows = parse_alias_rows();
        assert!(rows.len() >= 23, "expected the recorded parse_alias rows");
        for row in rows {
            let model = row["model"].as_str().expect("row has a model");
            let id = row["id"].as_str().unwrap_or("?");
            match row.get("error").and_then(serde_json::Value::as_str) {
                Some("not_maverick_alias") => assert_eq!(
                    parse_alias(model),
                    Err(AliasError::NotMaverickAlias),
                    "{id}: {model:?}"
                ),
                Some("malformed_alias") => assert_eq!(
                    parse_alias(model),
                    Err(AliasError::MalformedAlias),
                    "{id}: {model:?}"
                ),
                Some(other) => panic!("{id}: unexpected error kind {other:?}"),
                None => {
                    let want_mode = row["mode"].as_str().expect("row has a mode");
                    let want_pool = row["pool"].as_str().expect("row has a pool"); // "" == none
                    let got = parse_alias(model)
                        .unwrap_or_else(|e| panic!("{id}: {model:?} unexpectedly errored: {e}"));
                    assert_eq!(mode_name(got.mode), want_mode, "{id}: mode");
                    assert_eq!(got.pool.as_deref().unwrap_or(""), want_pool, "{id}: pool");
                    assert_eq!(
                        got.settings_mode,
                        SettingsMode::Apply,
                        "{id}: settings_mode"
                    );
                }
            }
        }
    }

    /// DIVERGENCE: D14. Rust accepts a trailing `?settings=caller`; Go treats these as
    /// `malformed_alias`. See `parity/DIVERGENCES.md` D14 and reference part 6 §5.
    #[test]
    fn d14_settings_caller_suffix() {
        // maverick/<pool>:<mode>?settings=caller
        let with_pool =
            parse_alias("maverick/prod-pool:cost?settings=caller").expect("valid D14 alias");
        assert_eq!(
            with_pool,
            Alias {
                pool: Some("prod-pool".to_owned()),
                mode: Mode::Cost,
                settings_mode: SettingsMode::Caller,
            }
        );

        // maverick/<mode>?settings=caller (no pool)
        let no_pool = parse_alias("maverick/auto?settings=caller").expect("valid D14 alias");
        assert_eq!(
            no_pool,
            Alias {
                pool: None,
                mode: Mode::Balanced,
                settings_mode: SettingsMode::Caller,
            }
        );

        // The same base without the suffix defaults to Apply.
        assert_eq!(
            parse_alias("maverick/prod-pool:cost")
                .expect("valid alias")
                .settings_mode,
            SettingsMode::Apply
        );

        // The suffix must be exactly `?settings=caller`; a different query is malformed.
        assert_eq!(
            parse_alias("maverick/auto?settings=apply"),
            Err(AliasError::MalformedAlias)
        );
    }

    /// Property (done-when): `parse_alias` is total and never panics on arbitrary input.
    /// The parser uses only total `&str` operations, so this holds by construction; the
    /// deterministic pseudo-random sweep guards against regressions. maverick-core has no
    /// `proptest` dev-dependency, so the generator is a small std-only xorshift.
    #[test]
    fn parse_never_panics_on_arbitrary_input() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next_u64 = || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };

        // Alias-significant bytes plus a couple of edge bytes; all valid ASCII.
        let alphabet = b"maverick/:?=scallentgqubosto AUTO\0\x7f";
        for _ in 0..5_000 {
            let len = (next_u64() % 24) as usize;
            let mut s = String::with_capacity(len);
            for _ in 0..len {
                let byte = alphabet[(next_u64() as usize) % alphabet.len()];
                s.push(byte as char);
            }
            // Also exercise the prefix and suffix boundaries directly.
            for candidate in [
                s.clone(),
                format!("maverick/{s}"),
                format!("{s}?settings=caller"),
                format!("maverick/{s}?settings=caller"),
            ] {
                let _ = parse_alias(&candidate);
            }
        }
    }
}
