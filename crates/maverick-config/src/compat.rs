//! Configuration compatibility (spec §14.5 "Configuration", D50; self-hosted plan §7.5).
//!
//! Self-hosted installs upgrade late and keep their old environment files, so a release
//! must neither stop on an extra setting nor lose a renamed one:
//!
//! - An unknown `MAVERICK_*` variable gets one warning with its name, never its value (a
//!   value can be a secret), and startup goes on. "Known" is every name the loader reads
//!   ([`known_settings`] records them), so the check and the loader cannot drift.
//! - A renamed variable keeps working under its old name for 12 months after the rename
//!   ([`RENAMED`]). The old name alone is used, with a warning that names the new one. With
//!   both set, the new name wins and the old one is reported as ignored.
//! - A new required setting needs a safe default or ships only in a major release
//!   (`docs/operations.md` "Configuration compatibility").

use std::cell::RefCell;
use std::collections::BTreeSet;

/// One renamed setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rename {
    /// The name before the rename.
    pub old: &'static str,
    /// The name after it.
    pub new: &'static str,
    /// The release date that renamed it, `YYYY-MM-DD`.
    pub renamed_on: &'static str,
}

/// Every rename still inside its window. Empty until a setting is renamed. A test fails
/// once a row is older than [`RENAME_WINDOW_DAYS`], so it is removed on purpose.
pub const RENAMED: &[Rename] = &[];

/// How long an old name keeps working: 12 months.
pub const RENAME_WINDOW_DAYS: i64 = 365;

/// `MAVERICK_*` names that are not engine settings but can share its environment: the
/// release stamp (read at build time), and the proxy's two unprefixed settings, since a
/// self-hosted install can keep the engine and the proxy in one environment file.
const OTHER_NAMES: &[&str] = &[
    "MAVERICK_RELEASE_DATE",
    "MAVERICK_RELEASE_COMMIT",
    "MAVERICK_ENGINE_URL",
    "MAVERICK_API_KEY",
];

/// Prefixes of other components' settings: the proxy's, and the test harness's.
const OTHER_PREFIXES: &[&str] = &["MAVERICK_PROXY_", "MAVERICK_UPSTREAM_", "MAVERICK_TEST_"];

/// One finding about the environment. It names variables only, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatWarning {
    /// A `MAVERICK_*` variable that no setting reads.
    Unknown { name: String },
    /// A renamed variable set only under its old name, which is used.
    Renamed {
        old: &'static str,
        new: &'static str,
    },
    /// A renamed variable set under both names: the new one wins.
    OldIgnored {
        old: &'static str,
        new: &'static str,
    },
}

/// `lookup` with the renames applied: a new name that is unset or empty falls back to its
/// old name when that one is set. Every other name reads as it is.
pub(crate) fn with_renames<F>(
    lookup: F,
    renames: &'static [Rename],
) -> impl Fn(&str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    move |name| {
        let value = lookup(name);
        if value.as_deref().is_some_and(|value| !value.is_empty()) {
            return value;
        }
        renames
            .iter()
            .find(|rename| rename.new == name)
            .and_then(|rename| lookup(rename.old))
            .filter(|old| !old.is_empty())
            .or(value)
    }
}

/// Every `MAVERICK_*` name the loader reads, for both roles. The loader runs four times
/// with a recording lookup: every value unset, then every value set, so a setting that is
/// read only when another one is set is recorded too. Old names of renames are included.
#[must_use]
pub fn known_settings() -> BTreeSet<String> {
    let read = RefCell::new(BTreeSet::new());
    for role in ["origin", "replica"] {
        for set in [false, true] {
            let _ = crate::load_with(|name| {
                read.borrow_mut().insert(name.to_owned());
                if name == "MAVERICK_ROLE" {
                    Some(role.to_owned())
                } else {
                    set.then(|| "1".to_owned())
                }
            });
        }
    }
    let mut known = read.into_inner();
    known.extend(RENAMED.iter().map(|rename| rename.old.to_owned()));
    known
}

/// The findings for an environment whose variable names are `names`, read through
/// `lookup`. Each name is reported once.
#[must_use]
pub fn compatibility_warnings<F>(names: &[String], lookup: F) -> Vec<CompatWarning>
where
    F: Fn(&str) -> Option<String>,
{
    warnings_with(names, lookup, &known_settings(), RENAMED)
}

fn warnings_with<F>(
    names: &[String],
    lookup: F,
    known: &BTreeSet<String>,
    renames: &[Rename],
) -> Vec<CompatWarning>
where
    F: Fn(&str) -> Option<String>,
{
    let set = |name: &str| lookup(name).is_some_and(|value| !value.is_empty());
    let mut out = Vec::new();
    for rename in renames {
        match (set(rename.old), set(rename.new)) {
            (true, false) => out.push(CompatWarning::Renamed {
                old: rename.old,
                new: rename.new,
            }),
            (true, true) => out.push(CompatWarning::OldIgnored {
                old: rename.old,
                new: rename.new,
            }),
            _ => {}
        }
    }
    let unknown: BTreeSet<&String> = names
        .iter()
        .filter(|name| name.starts_with("MAVERICK_"))
        .filter(|name| !known.contains(name.as_str()))
        .filter(|name| !OTHER_NAMES.contains(&name.as_str()))
        .filter(|name| !OTHER_PREFIXES.iter().any(|prefix| name.starts_with(prefix)))
        .collect();
    out.extend(
        unknown
            .into_iter()
            .map(|name| CompatWarning::Unknown { name: name.clone() }),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Days since 1970-01-01 for a `YYYY-MM-DD` date (H. Hinnant's days_from_civil).
    fn days_from_civil(date: &str) -> Option<i64> {
        let mut parts = date.splitn(3, '-');
        let year: i64 = parts.next()?.parse().ok()?;
        let month: i64 = parts.next()?.parse().ok()?;
        let day: i64 = parts.next()?.parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let year = if month <= 2 { year - 1 } else { year };
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        Some(era * 146_097 + day_of_era - 719_468)
    }

    /// The renames whose window has ended by `today` (days since 1970-01-01). A date that
    /// does not parse counts as ended, so a bad row fails the window test too.
    fn expired(renames: &[Rename], today: i64) -> Vec<Rename> {
        renames
            .iter()
            .filter(|rename| {
                days_from_civil(rename.renamed_on)
                    .is_none_or(|renamed| today > renamed + RENAME_WINDOW_DAYS)
            })
            .copied()
            .collect()
    }

    const RENAMES: &[Rename] = &[Rename {
        old: "MAVERICK_OLD_SETTING",
        new: "MAVERICK_NEW_SETTING",
        renamed_on: "2026-10-24",
    }];

    fn env(vars: &[(&str, &str)]) -> (Vec<String>, impl Fn(&str) -> Option<String>) {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let names = vars.iter().map(|(k, _)| k.clone()).collect();
        let lookup = move |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        (names, lookup)
    }

    #[test]
    fn the_loader_reads_every_known_setting() {
        let known = known_settings();
        for name in [
            "MAVERICK_ROLE",
            "MAVERICK_DATABASE_URL",
            "MAVERICK_INTERNAL_TOKENS",
            "MAVERICK_CAPTURE_MASTER_KEY",
            "MAVERICK_RATELIMIT_RPM",
            "MAVERICK_CRON_NIGHTLY",
            "MAVERICK_CRON_MODEL_DISCOVERY",
            "MAVERICK_JUDGE_POOL",
        ] {
            assert!(known.contains(name), "{name} is not known: {known:?}");
        }
        assert!(
            known.iter().all(|name| name.starts_with("MAVERICK_")),
            "{known:?}"
        );
    }

    /// DIVERGENCE D50: an unknown `MAVERICK_*` name is reported once, by name; known
    /// names, other components' names, and non-Maverick names are not.
    #[test]
    fn unknown_names_are_reported_by_name() {
        let (names, lookup) = env(&[
            ("MAVERICK_FOO", "secret-value"),
            ("MAVERICK_DATABASE_URL", "postgres://x"),
            ("MAVERICK_PROXY_MODE", "hosted"),
            ("MAVERICK_UPSTREAM_OPENAI", "http://x"),
            ("MAVERICK_RELEASE_DATE", "2026-10-24"),
            ("MAVERICK_API_KEY", "mk_live_x"),
            ("MAVERICK_TEST_REQUIRE_DB", "1"),
            ("PATH", "/bin"),
        ]);
        let mut names = names;
        names.push("MAVERICK_FOO".to_owned());
        assert_eq!(
            compatibility_warnings(&names, lookup),
            [CompatWarning::Unknown {
                name: "MAVERICK_FOO".to_owned()
            }]
        );
    }

    /// D50: the old name alone works and warns with the new name; with both set the new
    /// one wins and the old one is reported as ignored.
    #[test]
    fn renamed_settings_keep_their_old_name() {
        let (_, lookup) = env(&[("MAVERICK_OLD_SETTING", "old")]);
        let renamed = with_renames(&lookup, RENAMES);
        assert_eq!(renamed("MAVERICK_NEW_SETTING").as_deref(), Some("old"));
        let known: BTreeSet<String> = ["MAVERICK_NEW_SETTING", "MAVERICK_OLD_SETTING"]
            .map(str::to_owned)
            .into();
        let names = vec!["MAVERICK_OLD_SETTING".to_owned()];
        assert_eq!(
            warnings_with(&names, &lookup, &known, RENAMES),
            [CompatWarning::Renamed {
                old: "MAVERICK_OLD_SETTING",
                new: "MAVERICK_NEW_SETTING"
            }]
        );

        let (names, both) = env(&[
            ("MAVERICK_OLD_SETTING", "old"),
            ("MAVERICK_NEW_SETTING", "new"),
        ]);
        let renamed = with_renames(&both, RENAMES);
        assert_eq!(renamed("MAVERICK_NEW_SETTING").as_deref(), Some("new"));
        assert_eq!(
            warnings_with(&names, &both, &known, RENAMES),
            [CompatWarning::OldIgnored {
                old: "MAVERICK_OLD_SETTING",
                new: "MAVERICK_NEW_SETTING"
            }]
        );

        // An empty new name counts as unset.
        let (_, empty) = env(&[
            ("MAVERICK_OLD_SETTING", "old"),
            ("MAVERICK_NEW_SETTING", ""),
        ]);
        assert_eq!(
            with_renames(&empty, RENAMES)("MAVERICK_NEW_SETTING").as_deref(),
            Some("old")
        );
        let (_, neither) = env(&[]);
        assert_eq!(
            with_renames(&neither, RENAMES)("MAVERICK_NEW_SETTING"),
            None
        );
        // Without a usable old value, the new name reads as it is, empty included.
        let (_, blank) = env(&[("MAVERICK_OLD_SETTING", ""), ("MAVERICK_NEW_SETTING", "")]);
        let blank = with_renames(&blank, RENAMES);
        assert_eq!(blank("MAVERICK_NEW_SETTING").as_deref(), Some(""));
        assert_eq!(blank("MAVERICK_OLD_SETTING").as_deref(), Some(""));
    }

    /// D50: every rename row stays for 12 months, and a row past its window fails here,
    /// so it is removed on purpose.
    #[test]
    fn no_rename_row_outlives_its_window() {
        let today = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            .div_euclid(86_400);
        let today = i64::try_from(today).expect("days");
        assert_eq!(
            expired(RENAMED, today),
            Vec::<Rename>::new(),
            "remove these rows from RENAMED"
        );
        let renamed = days_from_civil("2026-10-24").expect("date");
        assert_eq!(
            expired(RENAMES, renamed + RENAME_WINDOW_DAYS),
            Vec::<Rename>::new()
        );
        assert_eq!(expired(RENAMES, renamed + RENAME_WINDOW_DAYS + 1), RENAMES);
        let bad = [Rename {
            renamed_on: "2026-13-01",
            ..RENAMES[0]
        }];
        assert_eq!(expired(&bad, 0), bad);
    }

    #[test]
    fn civil_dates_count_days_from_the_epoch() {
        assert_eq!(days_from_civil("1970-01-01"), Some(0));
        assert_eq!(days_from_civil("1970-03-01"), Some(59));
        assert_eq!(days_from_civil("2000-03-01"), Some(11_017));
        assert_eq!(days_from_civil("2026-10-24"), Some(20_750));
        assert_eq!(days_from_civil("2026-10"), None);
        assert_eq!(days_from_civil("x-1-1"), None);
    }
}
