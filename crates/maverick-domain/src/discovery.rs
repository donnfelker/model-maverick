//! Provider model discovery (spec §4.6, D51): snapshot base ids, and the `new` and
//! `withdrawn` sets from the provider lists and the catalog. Pure logic: the list calls
//! live in `maverick-providers`, and the `model_discovery` job and the refresh endpoint
//! read the store and write the alerts.

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;

/// The endpoint whose columns a first-party list can withdraw.
pub const DIRECT_ENDPOINT: &str = "direct";

/// A date stamp at the end of a model id. `year` is 0 for the two forms without a year.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateStamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

/// The stamp forms, longest first: the shape (`d` is a digit) and where the year (if
/// any), the month, and the day start in it.
const STAMP_FORMS: [(&[u8], Option<usize>, usize, usize); 4] = [
    (b"-dddd-dd-dd", Some(1), 6, 9),
    (b"-dddddddd", Some(1), 5, 7),
    (b"-dd-dd", None, 1, 4),
    (b"-dddd", None, 1, 3),
];

/// The number in the ASCII digits `digits`.
fn number(digits: &[u8]) -> u16 {
    digits
        .iter()
        .fold(0, |total, digit| total * 10 + u16::from(digit - b'0'))
}

/// Splits a trailing date stamp off `model_id` (spec §4.6): `-YYYYMMDD`, `-YYYY-MM-DD`,
/// `-MM-DD`, or `-MMDD`, with a month from 01 to 12 and a day from 01 to 31. The id
/// before the stamp must not be empty. Returns the base id and the stamp.
#[must_use]
pub fn split_date_stamp(model_id: &str) -> Option<(&str, DateStamp)> {
    let bytes = model_id.as_bytes();
    STAMP_FORMS.iter().find_map(|&(shape, year, month, day)| {
        let start = bytes.len().checked_sub(shape.len())?;
        let tail = &bytes[start..];
        let fits = tail.iter().zip(shape).all(|(&byte, &want)| {
            if want == b'd' {
                byte.is_ascii_digit()
            } else {
                byte == want
            }
        });
        if start == 0 || !fits {
            return None;
        }
        let stamp = DateStamp {
            year: year.map_or(0, |at| number(&tail[at..at + 4])),
            month: u8::try_from(number(&tail[month..month + 2])).ok()?,
            day: u8::try_from(number(&tail[day..day + 2])).ok()?,
        };
        let valid = (1..=12).contains(&stamp.month) && (1..=31).contains(&stamp.day);
        // The stamp starts with `-`, an ASCII byte, so `start` is a char boundary.
        valid.then(|| (&model_id[..start], stamp))
    })
}

/// The base id of `model_id`: the id without its trailing date stamp, if it has one.
#[must_use]
pub fn base_id(model_id: &str) -> &str {
    split_date_stamp(model_id).map_or(model_id, |(base, _)| base)
}

/// A catalog column, as discovery reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogColumn {
    pub id: String,
    pub provider: String,
    pub model_id: String,
    pub endpoint: String,
    pub status: String,
}

/// One provider's list result. `model_ids` is `None` when the call was skipped (no key)
/// or failed: such a provider reports nothing new and withdraws nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderListing {
    pub provider: String,
    pub model_ids: Option<Vec<String>>,
}

/// A listed model that no catalog column covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewModel {
    pub provider: String,
    pub base_model_id: String,
    /// The newest listed snapshot of the base id: the pinned id to enroll.
    pub model_id: String,
}

/// A `direct` column whose model its provider no longer lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WithdrawnModel {
    pub column_id: String,
    pub provider: String,
    pub model_id: String,
}

/// The two discovery sets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DiscoveryDiff {
    /// By provider in listing order, then by base id.
    pub new: Vec<NewModel>,
    /// By column id.
    pub withdrawn: Vec<WithdrawnModel>,
}

/// The newest of `ids` (all with one base id): the latest date stamp wins, any stamp
/// beats none, and equal stamps fall back to the larger id, so the choice never depends
/// on list order.
fn newest<'a>(ids: &[&'a str]) -> Option<&'a str> {
    ids.iter()
        .copied()
        .max_by_key(|id| (split_date_stamp(id).map(|(_, stamp)| stamp), *id))
}

/// The `new` and `withdrawn` sets (spec §4.6).
///
/// - A listed base id that no catalog column of the same provider has is new. Endpoint,
///   settings, and status do not matter: a retired column still covers its model.
/// - A non-retired `direct` column is withdrawn when its base id is missing from its
///   provider's list. Only a provider whose list call succeeded can withdraw.
#[must_use]
pub fn diff(listings: &[ProviderListing], catalog: &[CatalogColumn]) -> DiscoveryDiff {
    let cataloged: HashSet<(&str, &str)> = catalog
        .iter()
        .map(|column| (column.provider.as_str(), base_id(&column.model_id)))
        .collect();

    let mut new = Vec::new();
    let mut listed: HashSet<(&str, &str)> = HashSet::new();
    for listing in listings {
        let Some(model_ids) = &listing.model_ids else {
            continue;
        };
        let provider = listing.provider.as_str();
        let mut by_base: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for id in model_ids {
            by_base.entry(base_id(id)).or_default().push(id);
        }
        for (base, ids) in by_base {
            listed.insert((provider, base));
            if cataloged.contains(&(provider, base)) {
                continue;
            }
            if let Some(model_id) = newest(&ids) {
                new.push(NewModel {
                    provider: provider.to_owned(),
                    base_model_id: base.to_owned(),
                    model_id: model_id.to_owned(),
                });
            }
        }
    }

    let answered: HashSet<&str> = listings
        .iter()
        .filter(|listing| listing.model_ids.is_some())
        .map(|listing| listing.provider.as_str())
        .collect();
    let mut withdrawn: Vec<WithdrawnModel> = catalog
        .iter()
        .filter(|column| {
            column.status != "retired"
                && column.endpoint == DIRECT_ENDPOINT
                && answered.contains(column.provider.as_str())
                && !listed.contains(&(column.provider.as_str(), base_id(&column.model_id)))
        })
        .map(|column| WithdrawnModel {
            column_id: column.id.clone(),
            provider: column.provider.clone(),
            model_id: column.model_id.clone(),
        })
        .collect();
    withdrawn.sort_by(|a, b| a.column_id.cmp(&b.column_id));

    DiscoveryDiff { new, withdrawn }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(year: u16, month: u8, day: u8) -> DateStamp {
        DateStamp { year, month, day }
    }

    fn column(
        id: &str,
        provider: &str,
        model_id: &str,
        endpoint: &str,
        status: &str,
    ) -> CatalogColumn {
        CatalogColumn {
            id: id.to_owned(),
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
            endpoint: endpoint.to_owned(),
            status: status.to_owned(),
        }
    }

    fn listing(provider: &str, ids: &[&str]) -> ProviderListing {
        ProviderListing {
            provider: provider.to_owned(),
            model_ids: Some(ids.iter().map(|id| (*id).to_owned()).collect()),
        }
    }

    fn unanswered(provider: &str) -> ProviderListing {
        ProviderListing {
            provider: provider.to_owned(),
            model_ids: None,
        }
    }

    fn new_model(provider: &str, base: &str, model_id: &str) -> NewModel {
        NewModel {
            provider: provider.to_owned(),
            base_model_id: base.to_owned(),
            model_id: model_id.to_owned(),
        }
    }

    /// DIVERGENCE: D51. Each stamp form, with its numbers read digit by digit.
    #[test]
    fn every_stamp_form_splits_off() {
        assert_eq!(
            split_date_stamp("claude-haiku-4-5-20251001"),
            Some(("claude-haiku-4-5", stamp(2025, 10, 1)))
        );
        assert_eq!(
            split_date_stamp("gpt-4o-2024-08-06"),
            Some(("gpt-4o", stamp(2024, 8, 6)))
        );
        assert_eq!(
            split_date_stamp("gemini-2.5-pro-preview-06-05"),
            Some(("gemini-2.5-pro-preview", stamp(0, 6, 5)))
        );
        assert_eq!(
            split_date_stamp("grok-4-0709"),
            Some(("grok-4", stamp(0, 7, 9)))
        );
        assert_eq!(
            split_date_stamp("m-19991231"),
            Some(("m", stamp(1999, 12, 31)))
        );
    }

    /// DIVERGENCE: D51. Month 01 to 12 and day 01 to 31, both ends included.
    #[test]
    fn a_stamp_needs_a_real_month_and_day() {
        assert_eq!(split_date_stamp("m-0101"), Some(("m", stamp(0, 1, 1))));
        assert_eq!(split_date_stamp("m-1231"), Some(("m", stamp(0, 12, 31))));
        for kept in [
            "m-0001",
            "m-1301",
            "m-0100",
            "m-0132",
            "m-2024-00-10",
            "m-2024-13-10",
            "m-2024-01-00",
            "m-2024-01-32",
            "m-20240001",
            "m-20241301",
            "m-20240100",
            "m-20240132",
            "m-00-10",
            "m-13-10",
            "m-01-00",
            "m-01-32",
        ] {
            assert_eq!(split_date_stamp(kept), None, "{kept}");
            assert_eq!(base_id(kept), kept, "{kept}");
        }
    }

    /// DIVERGENCE: D51. Only a whole stamp at the very end, after a non-empty id.
    #[test]
    fn other_digit_tails_are_not_stamps() {
        for kept in [
            "-0709",
            "-20250101",
            "0709",
            "gpt-5.5",
            "gemini-2.0-flash-001",
            "gpt-4-1106-preview",
            "m-070",
            "m-2024-0709x",
            "m_0709",
            "m-07a9",
            "m-2024/07/09",
            "claude-sonnet-4-5",
        ] {
            assert_eq!(base_id(kept), kept, "{kept}");
        }
        // The 4-digit tail of a longer number is not a stamp on its own.
        assert_eq!(base_id("m-120709"), "m-120709");
    }

    /// DIVERGENCE: D51. The latest stamp wins, any stamp beats the bare base id, and an
    /// equal stamp falls back to the larger id, whatever the list order.
    #[test]
    fn the_newest_snapshot_is_chosen() {
        assert_eq!(
            newest(&["gpt-4o", "gpt-4o-2024-08-06", "gpt-4o-2024-05-13"]),
            Some("gpt-4o-2024-08-06")
        );
        assert_eq!(
            newest(&["gpt-4o-2024-11-20", "gpt-4o-2024-08-06"]),
            Some("gpt-4o-2024-11-20")
        );
        assert_eq!(newest(&["m-20240101", "m-20231231"]), Some("m-20240101"));
        assert_eq!(
            newest(&["m-2024-01-02", "m-2024-01-01"]),
            Some("m-2024-01-02")
        );
        assert_eq!(newest(&["m-0102", "m"]), Some("m-0102"));
        // Equal stamps: the larger id wins (`0` sorts after `-`), in either order.
        assert_eq!(newest(&["m-2024-01-01", "m-20240101"]), Some("m-20240101"));
        assert_eq!(newest(&["m-20240101", "m-2024-01-01"]), Some("m-20240101"));
        assert_eq!(newest(&["m"]), Some("m"));
        assert_eq!(newest(&[]), None);
    }

    /// DIVERGENCE: D51. A cataloged alias covers its snapshots and the reverse; a retired or
    /// non-direct column still covers its model; providers stay apart.
    #[test]
    fn new_models_are_matched_on_provider_and_base_id() {
        let catalog = [
            column("col_1", "anthropic", "claude-haiku-4-5", "direct", "active"),
            column("col_2", "openai", "gpt-4o-2024-05-13", "direct", "active"),
            column("col_3", "anthropic", "claude-3-opus", "direct", "retired"),
            column("col_4", "google", "gemini-2.5-pro", "vertex", "active"),
        ];
        let listings = [
            listing(
                "anthropic",
                &[
                    "claude-haiku-4-5-20251001",
                    "claude-3-opus-20240229",
                    "claude-sonnet-5-5-20260930",
                    "claude-opus-4-1-20250805",
                ],
            ),
            listing("google", &["gemini-2.5-pro", "gemini-2.5-flash"]),
            listing(
                "openai",
                &["gpt-4o", "gpt-4o-2024-08-06", "claude-haiku-4-5"],
            ),
        ];

        let diff = diff(&listings, &catalog);

        assert_eq!(
            diff.new,
            vec![
                new_model("anthropic", "claude-opus-4-1", "claude-opus-4-1-20250805"),
                new_model(
                    "anthropic",
                    "claude-sonnet-5-5",
                    "claude-sonnet-5-5-20260930"
                ),
                new_model("google", "gemini-2.5-flash", "gemini-2.5-flash"),
                // The same id under another provider is that provider's model.
                new_model("openai", "claude-haiku-4-5", "claude-haiku-4-5"),
            ]
        );
        assert_eq!(diff.withdrawn, vec![]);
    }

    /// DIVERGENCE: D51. Only a non-retired direct column of a provider that answered.
    #[test]
    fn withdrawn_columns_need_an_answering_provider() {
        let catalog = [
            column("col_b", "xai", "grok-3", "direct", "active"),
            column("col_a", "xai", "grok-2-1212", "direct", "deprecated"),
            column("col_c", "xai", "grok-1", "direct", "retired"),
            column("col_d", "xai", "grok-beta", "openrouter", "active"),
            column("col_e", "xai", "grok-4", "direct", "cold_start"),
            column("col_f", "openai", "gpt-4", "direct", "active"),
            column("col_g", "google", "gemini-1.0-pro", "direct", "active"),
        ];
        let listings = [listing("xai", &["grok-4-0709"]), unanswered("openai")];

        let diff = diff(&listings, &catalog);

        assert_eq!(
            diff.withdrawn,
            vec![
                WithdrawnModel {
                    column_id: "col_a".to_owned(),
                    provider: "xai".to_owned(),
                    model_id: "grok-2-1212".to_owned(),
                },
                WithdrawnModel {
                    column_id: "col_b".to_owned(),
                    provider: "xai".to_owned(),
                    model_id: "grok-3".to_owned(),
                },
            ]
        );
        // grok-4 is covered by the cataloged grok-4 column, so nothing is new.
        assert_eq!(diff.new, vec![]);
    }

    /// DIVERGENCE: D51. An unanswered provider reports nothing new either.
    #[test]
    fn an_unanswered_provider_reports_nothing() {
        let diff = diff(&[unanswered("anthropic")], &[]);
        assert_eq!(diff, DiscoveryDiff::default());
    }

    #[test]
    fn an_empty_catalog_makes_every_listed_base_new() {
        let diff = diff(&[listing("xai", &["grok-4-0709", "grok-4"])], &[]);
        assert_eq!(diff.new, vec![new_model("xai", "grok-4", "grok-4-0709")]);
    }
}
