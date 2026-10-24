//! `maverick matrix status`: the active matrix bundle (spec §14.4, D49). Both roles run
//! it. The result is plain text on stdout. Logs and errors go to stderr.

use std::io::Write;

use chrono::{DateTime, Utc};
use maverick_store::matrix::{ActiveBundle, active_bundle};
use sqlx::postgres::PgPoolOptions;

use crate::cli::{block_on, load_config};
use crate::corpus::tabwrite;
use crate::logging::{Output, with_logger};

const EXIT_SUCCESS: i32 = 0;
const EXIT_FAILURE: i32 = 1;

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// What `matrix status` prints when no bundle has been imported (a replica serves cold
/// start until one is, G1).
pub const NO_ACTIVE_BUNDLE: &str = "no matrix bundle is active\n";

/// `matrix status`. Exit 0 with the status on stdout, or 1 with the reason on stderr.
pub(crate) fn status(lookup: Lookup<'_>, stdout: Output, stderr: Output) -> i32 {
    let mut out = stdout;
    let mut plain_err = stderr.clone();
    with_logger(stderr, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        let read = block_on(async {
            let pool = match PgPoolOptions::new()
                .max_connections(1)
                .connect(&config.database_url)
                .await
            {
                Ok(pool) => pool,
                Err(error) => {
                    tracing::error!(error = %error, "connecting to database");
                    return None;
                }
            };
            let bundle = active_bundle(&pool).await;
            pool.close().await;
            Some(bundle)
        });
        match read {
            Ok(Some(Ok(bundle))) => {
                let _ = out.write_all(render(bundle.as_ref(), Utc::now()).as_bytes());
                EXIT_SUCCESS
            }
            Ok(Some(Err(error))) => {
                let _ = writeln!(plain_err, "maverick matrix status: {error}");
                EXIT_FAILURE
            }
            // A connect failure (already logged), or the runtime failed to build.
            Ok(None) | Err(_) => EXIT_FAILURE,
        }
    })
}

/// The status of `bundle` at `now`: one table row, or [`NO_ACTIVE_BUNDLE`].
fn render(bundle: Option<&ActiveBundle>, now: DateTime<Utc>) -> String {
    let Some(bundle) = bundle else {
        return NO_ACTIVE_BUNDLE.to_owned();
    };
    let rows = vec![
        vec![
            "MATRIX_VERSION".to_owned(),
            "PUBLISHED".to_owned(),
            "IMPORTED".to_owned(),
            "DAYS_BEHIND".to_owned(),
        ],
        vec![
            bundle.matrix_version.clone(),
            utc_seconds(bundle.published_at),
            utc_seconds(bundle.created_at),
            days_behind(bundle.published_at, now).to_string(),
        ],
    ];
    tabwrite(&rows, "")
}

fn utc_seconds(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Whole days from `published_at` to `now`, and 0 for a bundle published after `now`.
fn days_behind(published_at: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    (now - published_at).num_days().max(0)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn days_behind_counts_whole_days_and_never_goes_negative() {
        assert_eq!(days_behind(at(20, 3), at(20, 3)), 0);
        assert_eq!(days_behind(at(20, 3), at(21, 2)), 0);
        assert_eq!(days_behind(at(20, 3), at(21, 3)), 1);
        assert_eq!(days_behind(at(20, 3), at(24, 2)), 3);
        assert_eq!(days_behind(at(21, 3), at(20, 3)), 0);
    }

    #[test]
    fn render_prints_one_row_or_the_no_bundle_line() {
        assert_eq!(render(None, at(24, 0)), NO_ACTIVE_BUNDLE);
        let bundle = ActiveBundle {
            matrix_version: "2026-10-20".to_owned(),
            published_at: at(20, 3),
            created_at: at(21, 8),
        };
        assert_eq!(
            render(Some(&bundle), at(24, 4)),
            "MATRIX_VERSION  PUBLISHED             IMPORTED              DAYS_BEHIND\n\
             2026-10-20      2026-10-20T03:00:00Z  2026-10-21T08:00:00Z  4\n"
        );
    }
}
