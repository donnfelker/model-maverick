//! The G8 spend guard's pure parts (spec §11 "Spend guard", reference Part 3 §2.6).
//!
//! A batch projects its cost before it runs and is refused when the projection would
//! take the night or the ISO week over its cap. The job reads the spent totals, writes
//! the alert, and skips the batch.

use chrono::NaiveDate;

use crate::gojson::{self, JsonValue};
use crate::pricing::parse_pricing;
use crate::tasks::{parse_attachments, prompt_text};

/// `MAVERICK_SPEND_NIGHTLY_CAP_USD` default.
pub const NIGHTLY_CAP_USD: f64 = 100.0;
/// `MAVERICK_SPEND_WEEKLY_CAP_USD` default.
pub const WEEKLY_CAP_USD: f64 = 500.0;
/// A projection at or above this fraction of a cap warns.
pub const WARN_FRACTION: f64 = 0.8;
/// The input tokens the projection adds to every run for the provider's framing.
pub const INPUT_TOKEN_OVERHEAD: i64 = 600;

/// The input tokens the projection assumes for an assembled prompt:
/// `ceil(chars / 4) + 600`, where chars are Unicode code points, not bytes.
pub fn estimate_input_tokens(prompt_text: &str) -> i64 {
    prompt_text.chars().count().div_ceil(4) as i64 + INPUT_TOKEN_OVERHEAD
}

/// The fields of a `tasks` row that the projection reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendTask<'a> {
    pub id: &'a str,
    pub prompt: &'a str,
    /// The raw `attachments` JSON.
    pub attachments: &'a str,
    pub max_tokens: i64,
}

/// The fields of a `model_columns` row that the projection reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendColumn<'a> {
    pub id: &'a str,
    /// The raw pricing snapshot JSON.
    pub pricing: &'a str,
}

/// A projected batch.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Projection {
    pub projected_usd: f64,
    pub runs: i64,
}

/// `ProjectSpend(tasks, columns, repeats)`: for each column (outer) and each task
/// (inner), a run costs `(est_in * input_per_mtok + max_tokens * output_per_mtok) / 1e6`
/// with `est_in` from [`estimate_input_tokens`] over the assembled prompt, times
/// `repeats`.
///
/// A column's pricing error is `jobs: projecting spend: column <id>: <error>`. The
/// reference fixes only that form; a task's attachments error uses the same shape,
/// `jobs: projecting spend: task <id>: parsing attachments: <json error>`.
pub fn project_spend(
    tasks: &[SpendTask<'_>],
    columns: &[SpendColumn<'_>],
    repeats: i64,
) -> Result<Projection, String> {
    let mut projection = Projection::default();
    for column in columns {
        let pricing = parse_pricing(column.pricing)
            .map_err(|e| format!("jobs: projecting spend: column {}: {e}", column.id))?;
        for task in tasks {
            let attachments = parse_attachments(task.attachments)
                .map_err(|e| format!("jobs: projecting spend: task {}: {e}", task.id))?;
            let est_in = estimate_input_tokens(&prompt_text(task.prompt, &attachments));
            let per_run = (est_in as f64 * pricing.input_per_mtok
                + task.max_tokens as f64 * pricing.output_per_mtok)
                / 1e6;
            projection.projected_usd += per_run * repeats as f64;
            projection.runs += repeats;
        }
    }
    Ok(projection)
}

/// The spend caps in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpendCaps {
    pub nightly_usd: f64,
    pub weekly_usd: f64,
}

impl Default for SpendCaps {
    fn default() -> Self {
        SpendCaps {
            nightly_usd: NIGHTLY_CAP_USD,
            weekly_usd: WEEKLY_CAP_USD,
        }
    }
}

/// The guard's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendDecision {
    Allow,
    /// Runs, and raises a `spend_warning` alert.
    Warn,
    /// Does not run, and raises a critical `spend_refusal` alert.
    Refuse,
}

/// One guard check of a batch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpendCheck<'a> {
    /// The job kind, for example `nightly_smoke`.
    pub job: &'a str,
    pub night: NaiveDate,
    pub projected_usd: f64,
    pub planned_runs: i64,
    /// The cost of the runs already made on the night.
    pub night_spent_usd: f64,
    /// The cost of the runs already made from 6 days before the night through the night.
    pub week_spent_usd: f64,
    pub caps: SpendCaps,
    /// False for `weekly_sweep`: only the weekly cap applies.
    pub apply_nightly_cap: bool,
}

impl SpendCheck<'_> {
    pub fn night_total_usd(&self) -> f64 {
        self.night_spent_usd + self.projected_usd
    }

    pub fn week_total_usd(&self) -> f64 {
        self.week_spent_usd + self.projected_usd
    }

    /// Refuse when a total is above its cap (strict). Else warn when a total is at or
    /// above 80% of its cap (a zero cap with a zero projection warns, QUIRK J7).
    pub fn decision(&self) -> SpendDecision {
        let night = self.night_total_usd();
        let week = self.week_total_usd();
        if (self.apply_nightly_cap && night > self.caps.nightly_usd) || week > self.caps.weekly_usd
        {
            SpendDecision::Refuse
        } else if (self.apply_nightly_cap && night >= WARN_FRACTION * self.caps.nightly_usd)
            || week >= WARN_FRACTION * self.caps.weekly_usd
        {
            SpendDecision::Warn
        } else {
            SpendDecision::Allow
        }
    }

    fn night_clause(&self) -> String {
        if self.apply_nightly_cap {
            format!(
                " night to ${:.4} (cap ${:.2}) and",
                self.night_total_usd(),
                self.caps.nightly_usd
            )
        } else {
            String::new()
        }
    }

    /// The `spend_refusal` alert message (exact Go format).
    pub fn refusal_message(&self) -> String {
        format!(
            "{} refused for {}: projected ${:.4} would bring{} week to ${:.4} (cap ${:.2})",
            self.job,
            self.night.format("%Y-%m-%d"),
            self.projected_usd,
            self.night_clause(),
            self.week_total_usd(),
            self.caps.weekly_usd
        )
    }

    /// The `spend_warning` alert message (exact Go format).
    pub fn warning_message(&self) -> String {
        format!(
            "{} for {} projects ${:.4}, bringing{} week to ${:.4} of ${:.2} cap (\u{2265}80%)",
            self.job,
            self.night.format("%Y-%m-%d"),
            self.projected_usd,
            self.night_clause(),
            self.week_total_usd(),
            self.caps.weekly_usd
        )
    }

    /// The alert subject as Go's `json.Marshal` of the map writes it (keys sorted
    /// bytewise, shortest float form). Fails on a non-finite amount.
    pub fn subject(&self) -> Result<String, String> {
        let number = |x: f64| gojson::encode_f64(x).map(JsonValue::Number);
        let string = |s: String| JsonValue::String(s);
        let members = vec![
            ("job".to_owned(), string(self.job.to_owned())),
            (
                "night".to_owned(),
                string(self.night.format("%Y-%m-%d").to_string()),
            ),
            ("night_spent_usd".to_owned(), number(self.night_spent_usd)?),
            ("nightly_cap_usd".to_owned(), number(self.caps.nightly_usd)?),
            (
                "planned_runs".to_owned(),
                JsonValue::Number(self.planned_runs.to_string()),
            ),
            ("projected_usd".to_owned(), number(self.projected_usd)?),
            ("week_spent_usd".to_owned(), number(self.week_spent_usd)?),
            ("weekly_cap_usd".to_owned(), number(self.caps.weekly_usd)?),
        ];
        Ok(gojson::marshal(&JsonValue::Object(members)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(
        night_spent: f64,
        week_spent: f64,
        projected: f64,
        apply: bool,
    ) -> SpendCheck<'static> {
        SpendCheck {
            job: "nightly_smoke",
            night: NaiveDate::from_ymd_opt(2030, 11, 10).expect("date"),
            projected_usd: projected,
            planned_runs: 1,
            night_spent_usd: night_spent,
            week_spent_usd: week_spent,
            caps: SpendCaps {
                nightly_usd: 10.0,
                weekly_usd: 100.0,
            },
            apply_nightly_cap: apply,
        }
    }

    #[test]
    fn decision_thresholds() {
        use SpendDecision::{Allow, Refuse, Warn};
        // Nightly: above the cap refuses, at the cap and at 80% warn, below allows.
        assert_eq!(check(6.0, 6.0, 4.5, true).decision(), Refuse);
        assert_eq!(check(6.0, 6.0, 4.0, true).decision(), Warn);
        assert_eq!(check(4.0, 4.0, 4.0, true).decision(), Warn);
        assert_eq!(check(3.5, 3.5, 4.0, true).decision(), Allow);
        // Weekly sweep: the nightly cap does not apply.
        assert_eq!(check(6.0, 6.0, 4.5, false).decision(), Allow);
        assert_eq!(check(6.0, 6.0, 2.0, false).decision(), Allow);
        // Weekly: above refuses, at the cap and at 80% warn.
        assert_eq!(check(0.0, 96.0, 4.5, false).decision(), Refuse);
        assert_eq!(check(0.0, 96.0, 4.0, false).decision(), Warn);
        assert_eq!(check(0.0, 76.0, 4.0, false).decision(), Warn);
        assert_eq!(check(0.0, 75.0, 4.0, false).decision(), Allow);
        assert_eq!(check(0.0, 96.0, 4.5, true).decision(), Refuse);
        assert_eq!(check(0.0, 76.0, 4.0, true).decision(), Warn);
    }

    #[test]
    fn zero_caps_and_zero_projection_warn() {
        let zero = SpendCheck {
            caps: SpendCaps {
                nightly_usd: 0.0,
                weekly_usd: 0.0,
            },
            ..check(0.0, 0.0, 0.0, false)
        };
        assert_eq!(zero.decision(), SpendDecision::Warn);
        assert_eq!(SpendCaps::default().nightly_usd, 100.0);
        assert_eq!(SpendCaps::default().weekly_usd, 500.0);
    }

    #[test]
    fn subject_rejects_non_finite_amounts() {
        assert!(check(f64::NAN, 0.0, 0.0, true).subject().is_err());
        assert_eq!(
            check(0.25, 1.5, 1e-7, true).subject(),
            Ok(r#"{"job":"nightly_smoke","night":"2030-11-10","night_spent_usd":0.25,"nightly_cap_usd":10,"planned_runs":1,"projected_usd":1e-7,"week_spent_usd":1.5,"weekly_cap_usd":100}"#.to_owned())
        );
    }

    #[test]
    fn estimate_counts_code_points() {
        assert_eq!(
            estimate_input_tokens("\u{2192}\u{2192}\u{2192}\u{2192}\u{2192}"),
            602
        );
    }

    #[test]
    fn attachment_and_pricing_errors_name_the_row() {
        let task = SpendTask {
            id: "task_a",
            prompt: "p",
            attachments: "{",
            max_tokens: 1,
        };
        let column = SpendColumn {
            id: "col_a",
            pricing: r#"{"input_per_mtok": -1}"#,
        };
        assert_eq!(
            project_spend(&[task], &[column], 1),
            Err(
                "jobs: projecting spend: column col_a: pricing snapshot has a negative price"
                    .to_owned()
            )
        );
        let priced = SpendColumn {
            pricing: "{}",
            ..column
        };
        let err = project_spend(&[task], &[priced], 1).expect_err("attachments");
        assert!(
            err.starts_with("jobs: projecting spend: task task_a: parsing attachments: "),
            "{err}"
        );
        // With no columns nothing is parsed.
        assert_eq!(project_spend(&[task], &[], 3), Ok(Projection::default()));
    }
}
