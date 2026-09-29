//! Regression detection and the parse-failure watch (spec §6.5 and §11, reference
//! Part 3 §1.9 and §2.10).
//!
//! The thresholds are in display points: the stored quality scale is [0, 1] and the rules
//! compare values multiplied by 100. The `regression_scan` job reads the series, keeps the
//! findings of its night, and writes the alerts.

use chrono::{Days, NaiveDate};

use super::ewma::ALPHA;
use super::observe::dimension;

/// The detection and dedup parameters. `Default` has the spec's values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegressionParams {
    /// The EWMA weight the series was folded with. The shock rule runs only when it is
    /// above 0.
    pub alpha: f64,
    /// The drop rule's trailing window, in calendar days.
    pub trailing_days: u32,
    /// The drop rule's threshold, in points.
    pub drop_points: f64,
    /// The shock band's width, in standard deviations of the previous night.
    pub shock_sigma: f64,
    /// The shock band's floor, in points.
    pub shock_floor_points: f64,
    /// A finding less than this many days after the last alert is a duplicate.
    pub dedup_days: u32,
    /// A duplicate still alerts when it is at least this many points deeper.
    pub deepen_points: f64,
}

impl Default for RegressionParams {
    fn default() -> Self {
        RegressionParams {
            alpha: ALPHA,
            trailing_days: 7,
            drop_points: 3.0,
            shock_sigma: 2.0,
            shock_floor_points: 3.0,
            dedup_days: 7,
            deepen_points: 2.0,
        }
    }
}

/// One stored aggregate of the series, on the stored [0, 1] scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeriesPoint {
    pub as_of: NaiveDate,
    pub ewma: f64,
    pub variance: f64,
}

/// Which rule flagged a night.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegressionKind {
    /// The smoothed value fell below the trailing maximum.
    Drop,
    /// The night's raw observation fell below the previous smoothed value.
    Shock,
}

impl RegressionKind {
    /// The `regression_kind` subject value.
    pub fn as_str(self) -> &'static str {
        match self {
            RegressionKind::Drop => "drop",
            RegressionKind::Shock => "shock",
        }
    }
}

/// One flagged night. `baseline`, `current`, and `delta` are in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Finding {
    pub as_of: NaiveDate,
    /// Always `quality`.
    pub dimension: &'static str,
    pub kind: RegressionKind,
    pub baseline: f64,
    pub current: f64,
    pub delta: f64,
}

impl Finding {
    /// `CrossesBar(bar)`: the regression went from at or above the quality bar to below
    /// it. `bar` is on the [0, 1] scale.
    pub fn crosses_bar(&self, bar: f64) -> bool {
        let bar_points = bar * 100.0;
        self.baseline >= bar_points && self.current < bar_points
    }
}

/// `DetectRegressions(series, params)`: at most one finding per night of an ascending
/// series. The first night can never flag.
///
/// - Drop: `baseline` is the maximum smoothed value over the nights in
///   `[as_of - trailing_days, as_of)`. With no such night there is no drop check. The
///   night flags when `smoothed < baseline - drop_points`.
/// - Shock (only when `alpha > 0`): the fold is inverted against the previous night to
///   recover the raw observation, even across a gap in the series (QUIRK Q6). The band is
///   `max(shock_sigma * sqrt(previous variance) * 100, shock_floor_points)`. The night
///   flags when `raw < previous smoothed - band`.
/// - When both flag, the shock replaces the drop only when it is strictly deeper.
pub fn detect_regressions(series: &[SeriesPoint], params: &RegressionParams) -> Vec<Finding> {
    let mut findings = Vec::new();
    for t in 1..series.len() {
        let point = series[t];
        let prev = series[t - 1];
        let smoothed = point.ewma * 100.0;
        let window_start = point.as_of - Days::new(params.trailing_days.into());
        let baseline = series[..t]
            .iter()
            .filter(|row| row.as_of >= window_start && row.as_of < point.as_of)
            .map(|row| row.ewma * 100.0)
            .reduce(f64::max);
        let mut finding = baseline
            .filter(|&baseline| smoothed < baseline - params.drop_points)
            .map(|baseline| Finding {
                as_of: point.as_of,
                dimension: dimension::QUALITY,
                kind: RegressionKind::Drop,
                baseline,
                current: smoothed,
                delta: baseline - smoothed,
            });
        if params.alpha > 0.0 {
            let prev_smoothed = prev.ewma * 100.0;
            let raw = (point.ewma - (1.0 - params.alpha) * prev.ewma) / params.alpha * 100.0;
            let band =
                (params.shock_sigma * prev.variance.sqrt() * 100.0).max(params.shock_floor_points);
            let delta = prev_smoothed - raw;
            if raw < prev_smoothed - band && finding.is_none_or(|drop| delta > drop.delta) {
                finding = Some(Finding {
                    as_of: point.as_of,
                    dimension: dimension::QUALITY,
                    kind: RegressionKind::Shock,
                    baseline: prev_smoothed,
                    current: raw,
                    delta,
                });
            }
        }
        findings.extend(finding);
    }
    findings
}

/// The last alert raised for a cell: the dedup state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlertMark {
    pub as_of: NaiveDate,
    /// In points.
    pub current: f64,
}

/// `FilterAlertable(findings, last, params)`: drops the duplicates of the ascending
/// findings. A finding is a duplicate when it is less than `dedup_days` after the last
/// alert (exactly `dedup_days` re-arms) and not at least `deepen_points` deeper than it.
/// Each finding that alerts becomes the new last alert.
pub fn filter_alertable(
    findings: &[Finding],
    last: Option<AlertMark>,
    params: &RegressionParams,
) -> Vec<Finding> {
    let mut last = last;
    let mut alertable = Vec::new();
    for finding in findings {
        let duplicate = last.is_some_and(|mark| {
            (finding.as_of - mark.as_of).num_days() < i64::from(params.dedup_days)
                && finding.current > mark.current - params.deepen_points
        });
        if !duplicate {
            alertable.push(*finding);
            last = Some(AlertMark {
                as_of: finding.as_of,
                current: finding.current,
            });
        }
    }
    alertable
}

/// Alert severities (exact strings).
pub mod severity {
    pub const WARNING: &str = "warning";
    pub const CRITICAL: &str = "critical";
}

/// The quality bar of a tenant that set none for the capability.
pub const DEFAULT_QUALITY_BAR: f64 = 0.80;

/// A regression's severity: `critical` when it crosses the quality bar of any tenant
/// whose pool holds the column, else `warning`. `bars` has one bar per such tenant.
pub fn regression_severity(finding: &Finding, bars: &[f64]) -> &'static str {
    if bars.iter().any(|&bar| finding.crosses_bar(bar)) {
        severity::CRITICAL
    } else {
        severity::WARNING
    }
}

/// The parse-failure watch alerts above this batch rate (strict).
pub const PARSE_FAILURE_RATE_FLOOR: f64 = 0.05;
/// The parse-failure watch alerts above this multiple of the trailing 7-day rate
/// (strict).
pub const PARSE_FAILURE_TRAILING_MULTIPLE: f64 = 2.0;

/// `parse_failures / total`, or 0 when there are no runs.
pub fn parse_failure_rate(parse_failures: i64, total: i64) -> f64 {
    if total > 0 {
        parse_failures as f64 / total as f64
    } else {
        0.0
    }
}

/// The parse-failure watch (spec §11): `Some(rate)` when a column's batch rate is above
/// 5% and above twice its trailing 7-day rate, else `None`. A column with no runs never
/// alerts.
pub fn parse_failure_alert(parse_failures: i64, total: i64, trailing_rate: f64) -> Option<f64> {
    let rate = parse_failure_rate(parse_failures, total);
    (rate > PARSE_FAILURE_RATE_FLOOR && rate > PARSE_FAILURE_TRAILING_MULTIPLE * trailing_rate)
        .then_some(rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2031, 5, d).expect("date")
    }

    fn point(d: u32, ewma: f64, variance: f64) -> SeriesPoint {
        SeriesPoint {
            as_of: day(d),
            ewma,
            variance,
        }
    }

    fn half_alpha() -> RegressionParams {
        RegressionParams {
            alpha: 0.5,
            ..RegressionParams::default()
        }
    }

    #[test]
    fn drop_threshold_is_strict() {
        let params = RegressionParams {
            drop_points: 3.125,
            alpha: 0.0,
            ..RegressionParams::default()
        };
        let at = detect_regressions(&[point(1, 0.5, 0.0), point(2, 0.46875, 0.0)], &params);
        assert!(at.is_empty());
        let below = detect_regressions(
            &[point(1, 0.5, 0.0), point(2, 0.468_75 - 1e-9, 0.0)],
            &params,
        );
        assert_eq!(below.len(), 1);
        assert_eq!(below[0].kind, RegressionKind::Drop);
    }

    #[test]
    fn zero_alpha_skips_the_shock_rule() {
        let params = RegressionParams {
            alpha: 0.0,
            ..RegressionParams::default()
        };
        let findings = detect_regressions(&[point(1, 0.9, 0.0), point(2, 0.5, 0.0)], &params);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, RegressionKind::Drop);
        assert_eq!(findings[0].delta, 40.0);
    }

    #[test]
    fn a_tie_keeps_the_drop() {
        // Night 3: drop 75 -> 50 (25 points); raw 37.5 against 62.5 (25 points).
        let series = [
            point(1, 0.75, 0.0),
            point(2, 0.625, 0.0),
            point(3, 0.5, 0.0),
        ];
        let findings = detect_regressions(&series, &half_alpha());
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[1].kind, RegressionKind::Drop);
        assert_eq!(findings[1].delta, 25.0);
        assert_eq!(findings[1].baseline, 75.0);
    }

    #[test]
    fn shock_band_is_strict_and_uses_the_previous_variance() {
        // Band = 2 * sqrt(1/64) * 100 = 25; raw = 50 = 75 - 25: no shock.
        let at = detect_regressions(
            &[point(1, 0.75, 0.015_625), point(2, 0.625, 0.0)],
            &half_alpha(),
        );
        assert_eq!(at.len(), 1);
        assert_eq!(at[0].kind, RegressionKind::Drop);
        let below = detect_regressions(
            &[point(1, 0.75, 0.015_625), point(2, 0.625 - 1e-9, 0.0)],
            &half_alpha(),
        );
        assert_eq!(below[0].kind, RegressionKind::Shock);
    }

    #[test]
    fn a_same_night_row_is_outside_the_trailing_window() {
        // The earlier row of night 2 is not in night 2's window: the baseline is 50.
        let series = [point(1, 0.5, 0.0), point(2, 0.95, 0.0), point(2, 0.9, 0.0)];
        let params = RegressionParams {
            alpha: 0.0,
            ..RegressionParams::default()
        };
        assert!(detect_regressions(&series, &params).is_empty());
    }

    #[test]
    fn severity_needs_a_crossed_bar() {
        let finding = Finding {
            as_of: day(1),
            dimension: dimension::QUALITY,
            kind: RegressionKind::Drop,
            baseline: 90.0,
            current: 70.0,
            delta: 20.0,
        };
        assert_eq!(regression_severity(&finding, &[]), severity::WARNING);
        assert_eq!(
            regression_severity(&finding, &[0.95, 0.5]),
            severity::WARNING
        );
        assert_eq!(
            regression_severity(&finding, &[0.95, DEFAULT_QUALITY_BAR]),
            severity::CRITICAL
        );
        assert_eq!(RegressionKind::Shock.as_str(), "shock");
        assert_eq!(RegressionKind::Drop.as_str(), "drop");
    }

    #[test]
    fn parse_failure_watch_thresholds() {
        assert_eq!(parse_failure_rate(0, 0), 0.0);
        assert_eq!(parse_failure_rate(3, 10), 0.3);
        // Fresh column, no trailing history.
        assert_eq!(parse_failure_alert(2, 10, 0.0), Some(0.2));
        // Noisy column: 20% against a trailing 30%.
        assert_eq!(parse_failure_alert(2, 10, 0.3), None);
        // Exactly 5% does not alert.
        assert_eq!(parse_failure_alert(1, 20, 0.0), None);
        // Exactly twice the trailing rate does not alert.
        assert_eq!(parse_failure_alert(2, 10, 0.1), None);
        assert_eq!(parse_failure_alert(0, 0, 0.0), None);
    }
}
