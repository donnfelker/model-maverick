//! OpenRouter execution (openrouter execution spec §3.2 and §4; `parity/DIVERGENCES.md`
//! D15; v1 spec §8.1).
//!
//! In OpenRouter mode every chain entry goes to the one OpenRouter upstream, with the
//! entry's `execution_aliases.openrouter` slug as the model. An entry without that alias
//! cannot run there and is skipped.
//!
//! Zero retention (D15) fails closed. For a zero-retention tenant each attempt carries
//! OpenRouter's per-request Zero Data Retention option ([`ZDR_OBJECT`].[`ZDR_FIELD`] =
//! `true`), and an entry whose alias is not verified `zero` (the engine already applied
//! the expiry rule) is skipped. When nothing remains the answer is
//! `no_compliant_columns`, never a direct-key fallback.
//!
//! The proxy and the SDK cores share these rules.

use crate::recommend::{ChainEntry, RetentionMode};

/// The execution alias key of the OpenRouter slug.
pub const ALIAS_KEY: &str = "openrouter";

/// The `endpoint_used` telemetry value of an OpenRouter attempt.
pub const ENDPOINT_USED: &str = "openrouter";

/// The request member that carries OpenRouter's provider-routing options.
pub const ZDR_OBJECT: &str = "provider";

/// The provider-routing option for Zero Data Retention, sent as `true` on every attempt
/// for a zero-retention tenant (confirmed against OpenRouter's docs, bead 10.6).
pub const ZDR_FIELD: &str = "zdr";

/// Whether one entry can run on OpenRouter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility<'a> {
    /// Run it with this model slug.
    Slug(&'a str),
    /// The entry has no (non-empty) OpenRouter alias.
    NoAlias,
    /// A zero-retention tenant, and the alias is not verified `zero`.
    NotZeroRetention,
}

/// Whether `entry` runs on OpenRouter for a tenant with this zero-retention flag.
#[must_use]
pub fn eligibility(entry: &ChainEntry, zero_retention: bool) -> Eligibility<'_> {
    let Some(slug) = entry
        .execution_aliases
        .get(ALIAS_KEY)
        .filter(|slug| !slug.is_empty())
    else {
        return Eligibility::NoAlias;
    };
    if zero_retention
        && entry.execution_alias_retention.get(ALIAS_KEY) != Some(&RetentionMode::Zero)
    {
        return Eligibility::NotZeroRetention;
    }
    Eligibility::Slug(slug)
}

/// What a whole chain allows before any attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// At least one entry is eligible.
    Executable,
    /// No entry has an OpenRouter alias (a catalog gap, not a fail-open event).
    NoExecutableCandidates,
    /// A zero-retention tenant, and no entry has a verified `zero` alias: fail closed.
    NoCompliantColumns,
}

/// The chain's verdict. For a zero-retention tenant an empty eligible set is always
/// [`Verdict::NoCompliantColumns`].
#[must_use]
pub fn verdict(chain: &[ChainEntry], zero_retention: bool) -> Verdict {
    if chain
        .iter()
        .any(|entry| matches!(eligibility(entry, zero_retention), Eligibility::Slug(_)))
    {
        Verdict::Executable
    } else if zero_retention {
        Verdict::NoCompliantColumns
    } else {
        Verdict::NoExecutableCandidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(alias: Option<&str>, retention: Option<RetentionMode>) -> ChainEntry {
        let mut entry = ChainEntry {
            model: "m".into(),
            provider: "anthropic".into(),
            ..ChainEntry::default()
        };
        if let Some(alias) = alias {
            entry
                .execution_aliases
                .insert(ALIAS_KEY.to_owned(), alias.to_owned());
        }
        if let Some(retention) = retention {
            entry
                .execution_alias_retention
                .insert(ALIAS_KEY.to_owned(), retention);
        }
        entry
    }

    #[test]
    fn entries_need_an_alias_and_for_zero_retention_a_zero_alias() {
        let zero = entry(Some("anthropic/claude"), Some(RetentionMode::Zero));
        let standard = entry(Some("openai/gpt"), Some(RetentionMode::Standard));
        let unknown = entry(Some("google/gemini"), None);
        let no_alias = entry(None, Some(RetentionMode::Zero));
        let empty_alias = entry(Some(""), Some(RetentionMode::Zero));
        assert_eq!(
            eligibility(&zero, true),
            Eligibility::Slug("anthropic/claude")
        );
        assert_eq!(eligibility(&standard, true), Eligibility::NotZeroRetention);
        assert_eq!(eligibility(&unknown, true), Eligibility::NotZeroRetention);
        assert_eq!(eligibility(&no_alias, true), Eligibility::NoAlias);
        assert_eq!(eligibility(&empty_alias, false), Eligibility::NoAlias);
        assert_eq!(
            eligibility(&standard, false),
            Eligibility::Slug("openai/gpt")
        );
        assert_eq!(
            eligibility(&unknown, false),
            Eligibility::Slug("google/gemini")
        );
    }

    #[test]
    fn zero_retention_fails_closed_and_never_reports_a_catalog_gap() {
        let standard = entry(Some("openai/gpt"), Some(RetentionMode::Standard));
        let zero = entry(Some("anthropic/claude"), Some(RetentionMode::Zero));
        let no_alias = entry(None, None);
        assert_eq!(
            verdict(&[standard.clone(), zero.clone()], true),
            Verdict::Executable
        );
        assert_eq!(
            verdict(std::slice::from_ref(&standard), true),
            Verdict::NoCompliantColumns
        );
        assert_eq!(
            verdict(std::slice::from_ref(&no_alias), true),
            Verdict::NoCompliantColumns
        );
        assert_eq!(verdict(&[], true), Verdict::NoCompliantColumns);
        assert_eq!(
            verdict(std::slice::from_ref(&no_alias), false),
            Verdict::NoExecutableCandidates
        );
        assert_eq!(verdict(&[], false), Verdict::NoExecutableCandidates);
        assert_eq!(verdict(&[no_alias, standard], false), Verdict::Executable);
    }
}
