//! The hidden `maverick spend preview` report (bead 9.15).
//!
//! It prints what a batch would project and what the G8 spend guard would decide, from the
//! batch's own composition, projection, and guard check
//! ([`maverick_jobs::batch::preview_spend`]). It is read only: no batch lease, no alert,
//! no run. Operators run it before the first nights, when every column is `cold_start`
//! and runs the whole corpus, to size `MAVERICK_SPEND_NIGHTLY_CAP_USD` and
//! `MAVERICK_SPEND_WEEKLY_CAP_USD`. Like the `corpus` commands, config and database
//! failures go through the JSON logger to stdout, and the report is plain text.

use std::io::Write as _;

use chrono::NaiveDate;
use maverick_domain::spend::{SpendCaps, SpendDecision};
use maverick_jobs::JobKind;
use maverick_jobs::batch::{SpendPreview, preview_spend};
use sqlx::postgres::PgPoolOptions;

use crate::cli::{block_on, load_config};
use crate::corpus::tabwrite;
use crate::logging::{Output, with_logger};

const EXIT_SUCCESS: i32 = 0;
const EXIT_FAILURE: i32 = 1;

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// `spend preview` for `job` and `night`, with the configured repeats and caps.
pub(crate) fn preview(
    job: JobKind,
    night: NaiveDate,
    lookup: Lookup<'_>,
    stdout: Output,
    stderr: Output,
) -> i32 {
    let mut plain_out = stdout.clone();
    let mut plain_err = stderr;
    with_logger(stdout, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        let caps = SpendCaps {
            nightly_usd: config.spend_nightly_cap_usd,
            weekly_usd: config.spend_weekly_cap_usd,
        };
        let outcome = block_on(async {
            let pool = match PgPoolOptions::new()
                .max_connections(2)
                .connect(&config.database_url)
                .await
            {
                Ok(pool) => pool,
                Err(error) => {
                    tracing::error!(error = %error, "connecting to database");
                    return Err(None);
                }
            };
            let preview = preview_spend(&pool, job, night, config.runner_repeats, caps).await;
            pool.close().await;
            preview.map_err(|error| Some(error.to_string()))
        });
        match outcome {
            Ok(Ok(preview)) => {
                let _ = plain_out.write_all(render(&preview).as_bytes());
                EXIT_SUCCESS
            }
            Ok(Err(Some(message))) => {
                let _ = writeln!(plain_err, "maverick spend preview: {message}");
                EXIT_FAILURE
            }
            // Connect failure (already logged) or the runtime failed to build.
            Ok(Err(None)) | Err(_) => EXIT_FAILURE,
        }
    })
}

fn usd(amount: f64) -> String {
    // An empty f64 sum is negative zero; a group with no columns costs zero dollars.
    let amount = if amount == 0.0 { 0.0 } else { amount };
    format!("{amount:.4}")
}

/// The report text.
pub(crate) fn render(preview: &SpendPreview) -> String {
    let mut out = format!(
        "{} for {}: corpus {}, {} repeats\n\n",
        preview.kind.as_str(),
        preview.night.format("%Y-%m-%d"),
        if preview.matrix_version.is_empty() {
            "(none active)"
        } else {
            &preview.matrix_version
        },
        preview.repeats,
    );

    let mut groups = vec![vec![
        "GROUP".to_owned(),
        "TASKS".to_owned(),
        "COLUMNS".to_owned(),
        "RUNS".to_owned(),
        "PROJECTED_USD".to_owned(),
    ]];
    let mut columns = vec![vec![
        "COLUMN".to_owned(),
        "MODEL".to_owned(),
        "STATUS".to_owned(),
        "GROUP".to_owned(),
        "RUNS".to_owned(),
        "PROJECTED_USD".to_owned(),
    ]];
    for group in &preview.groups {
        let runs: i64 = group.columns.iter().map(|c| c.runs).sum();
        let projected: f64 = group.columns.iter().map(|c| c.projected_usd).sum();
        groups.push(vec![
            group.name.to_owned(),
            group.tasks.to_string(),
            group.columns.len().to_string(),
            runs.to_string(),
            usd(projected),
        ]);
        for column in &group.columns {
            columns.push(vec![
                column.id.clone(),
                format!(
                    "{}/{}/{}",
                    column.provider, column.model_id, column.endpoint
                ),
                column.status.clone(),
                group.name.to_owned(),
                column.runs.to_string(),
                usd(column.projected_usd),
            ]);
        }
    }
    out.push_str(&tabwrite(&groups, ""));
    if columns.len() > 1 {
        out.push('\n');
        out.push_str(&tabwrite(&columns, ""));
    }

    let projected = preview.remaining.projected_usd;
    let night = if preview.apply_nightly_cap {
        format!(
            "${} spent + ${} projected = ${} of the ${:.2} nightly cap",
            usd(preview.night_spent_usd),
            usd(projected),
            usd(preview.night_spent_usd + projected),
            preview.caps.nightly_usd
        )
    } else {
        "not checked: weekly_sweep has only the weekly cap".to_owned()
    };
    let week = format!(
        "${} spent + ${} projected = ${} of the ${:.2} weekly cap",
        usd(preview.week_spent_usd),
        usd(projected),
        usd(preview.week_spent_usd + projected),
        preview.caps.weekly_usd
    );
    let verdict = if preview.remaining.runs == 0 {
        "nothing to run: the batch would skip the spend guard"
    } else {
        match preview.decision {
            SpendDecision::Allow => "allow: the batch would run",
            SpendDecision::Warn => {
                "warn: the batch would run and raise a spend_warning (a total at 80% of its cap or more)"
            }
            SpendDecision::Refuse => {
                "refuse: the batch would not run, and it would raise a critical spend_refusal"
            }
        }
    };
    let mut summary = vec![vec![
        "projected".to_owned(),
        format!(
            "${} for {} runs",
            usd(preview.projection.projected_usd),
            preview.projection.runs
        ),
    ]];
    if preview.done_runs > 0 {
        summary.push(vec![
            "recorded".to_owned(),
            format!("{} runs already recorded for this batch", preview.done_runs),
        ]);
        summary.push(vec![
            "remaining".to_owned(),
            format!("${} for {} runs", usd(projected), preview.remaining.runs),
        ]);
    }
    summary.extend([
        vec!["night".to_owned(), night],
        vec!["week".to_owned(), week],
        vec!["verdict".to_owned(), verdict.to_owned()],
    ]);
    out.push('\n');
    out.push_str(&tabwrite(&summary, ""));
    out
}

#[cfg(test)]
mod tests {
    use maverick_domain::spend::Projection;
    use maverick_jobs::batch::{PreviewColumn, PreviewGroup};

    use super::*;

    fn column(id: &str, model: &str, status: &str, runs: i64, usd: f64) -> PreviewColumn {
        PreviewColumn {
            id: id.to_owned(),
            provider: "anthropic".to_owned(),
            model_id: model.to_owned(),
            endpoint: "direct".to_owned(),
            status: status.to_owned(),
            runs,
            projected_usd: usd,
        }
    }

    fn preview(decision: SpendDecision) -> SpendPreview {
        SpendPreview {
            kind: JobKind::NightlySmoke,
            night: NaiveDate::from_ymd_opt(2030, 10, 1).expect("date"),
            matrix_version: "2030-09-30".to_owned(),
            repeats: 3,
            groups: vec![
                PreviewGroup {
                    name: "smoke",
                    tasks: 2,
                    columns: vec![column("col_a", "m-a", "active", 6, 0.25)],
                },
                PreviewGroup {
                    name: "cold_start",
                    tasks: 3,
                    columns: vec![
                        column("col_b", "m-b", "cold_start", 9, 1.5),
                        column("col_c", "m-c", "cold_start", 9, 2.0),
                    ],
                },
            ],
            projection: Projection {
                projected_usd: 3.75,
                runs: 24,
            },
            done_runs: 0,
            remaining: Projection {
                projected_usd: 3.75,
                runs: 24,
            },
            night_spent_usd: 1.0,
            week_spent_usd: 4.0,
            caps: SpendCaps {
                nightly_usd: 100.0,
                weekly_usd: 500.0,
            },
            apply_nightly_cap: true,
            decision,
        }
    }

    #[test]
    fn renders_groups_columns_windows_and_the_verdict() {
        assert_eq!(
            render(&preview(SpendDecision::Allow)),
            "nightly_smoke for 2030-10-01: corpus 2030-09-30, 3 repeats

GROUP       TASKS  COLUMNS  RUNS  PROJECTED_USD
smoke       2      1        6     0.2500
cold_start  3      2        18    3.5000

COLUMN  MODEL                 STATUS      GROUP       RUNS  PROJECTED_USD
col_a   anthropic/m-a/direct  active      smoke       6     0.2500
col_b   anthropic/m-b/direct  cold_start  cold_start  9     1.5000
col_c   anthropic/m-c/direct  cold_start  cold_start  9     2.0000

projected  $3.7500 for 24 runs
night      $1.0000 spent + $3.7500 projected = $4.7500 of the $100.00 nightly cap
week       $4.0000 spent + $3.7500 projected = $7.7500 of the $500.00 weekly cap
verdict    allow: the batch would run
"
        );
    }

    #[test]
    fn names_each_verdict() {
        let last = |p: &SpendPreview| render(p).lines().last().expect("line").to_owned();
        assert_eq!(
            last(&preview(SpendDecision::Warn)),
            "verdict    warn: the batch would run and raise a spend_warning (a total at 80% of its cap or more)"
        );
        assert_eq!(
            last(&preview(SpendDecision::Refuse)),
            "verdict    refuse: the batch would not run, and it would raise a critical spend_refusal"
        );
        let mut empty = preview(SpendDecision::Refuse);
        empty.projection.runs = 0;
        empty.remaining.runs = 0;
        assert_eq!(
            last(&empty),
            "verdict    nothing to run: the batch would skip the spend guard"
        );
    }

    #[test]
    fn resumed_report_adds_only_remaining_cost_to_actual_spend() {
        let mut resumed = preview(SpendDecision::Warn);
        resumed.done_runs = 6;
        resumed.remaining = Projection {
            projected_usd: 3.5,
            runs: 18,
        };
        let report = render(&resumed);
        assert!(
            report.contains("projected  $3.7500 for 24 runs\n"),
            "{report}"
        );
        assert!(
            report.contains("recorded   6 runs already recorded for this batch\n"),
            "{report}"
        );
        assert!(
            report.contains("remaining  $3.5000 for 18 runs\n"),
            "{report}"
        );
        assert!(
            report.contains("$1.0000 spent + $3.5000 projected = $4.5000"),
            "{report}"
        );
        assert!(
            report.contains("$4.0000 spent + $3.5000 projected = $7.5000"),
            "{report}"
        );
        resumed.remaining = Projection::default();
        assert!(render(&resumed).contains("nothing to run: the batch would skip the spend guard"));
    }

    #[test]
    fn weekly_sweep_skips_the_nightly_cap_and_no_corpus_is_named() {
        let mut sweep = preview(SpendDecision::Allow);
        sweep.kind = JobKind::WeeklySweep;
        sweep.apply_nightly_cap = false;
        sweep.matrix_version.clear();
        sweep.groups.clear();
        let text = render(&sweep);
        assert!(
            text.starts_with("weekly_sweep for 2030-10-01: corpus (none active), 3 repeats\n"),
            "{text}"
        );
        assert!(
            text.contains("\nnight      not checked: weekly_sweep has only the weekly cap\n"),
            "{text}"
        );
        assert!(
            !text.lines().any(|line| line.starts_with("COLUMN ")),
            "{text}"
        );
    }
}
