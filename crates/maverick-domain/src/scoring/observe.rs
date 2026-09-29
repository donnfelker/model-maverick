//! Cell observation (spec §6.4, reference Part 3 §1.7).
//!
//! A cell is (model column, capability, difficulty tier). One batch observes each cell
//! once: a value per dimension, which the EWMA stream then folds. A dimension absent from
//! the observation is not observed this batch and folds nothing.

use std::collections::BTreeMap;

use super::run_quality::status;

/// Dimension names (exact).
pub mod dimension {
    pub const QUALITY: &str = "quality";
    pub const PASS_RATE: &str = "pass_rate";
    pub const AMBIGUITY: &str = "ambiguity";
    pub const LATENCY_P50_MS: &str = "latency_p50_ms";
    pub const LATENCY_P95_MS: &str = "latency_p95_ms";
    pub const COST_PER_RUN_USD: &str = "cost_per_run_usd";
    /// New in the Rust engine (D6).
    pub const AVAILABILITY: &str = "availability";
}

/// One (task, column) pair of the cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskOutcome {
    /// The task score (after the variance and ambiguity penalties).
    pub score: f64,
    pub ambiguity: f64,
    /// False when the pair had no gradable repeat: it enters no quality mean.
    pub gradable: bool,
}

/// One executed run of the cell (every run, including provider errors and timeouts).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunStats {
    pub pass: bool,
    pub latency_ms: Option<i32>,
    pub cost_usd: Option<f64>,
    /// False for a provider error or timeout: the run returned no output (D6).
    pub returned_output: bool,
}

impl RunStats {
    /// The stats of a run row. A timeout's latency is censored at `timeout_ms` (D7: Go
    /// left timeouts out of the latency percentiles).
    pub fn from_run(
        run_status: &str,
        pass: bool,
        latency_ms: Option<i32>,
        cost_usd: Option<f64>,
        timeout_ms: i32,
    ) -> Self {
        let timed_out = run_status == status::TIMEOUT;
        RunStats {
            pass,
            latency_ms: if timed_out {
                Some(timeout_ms)
            } else {
                latency_ms
            },
            cost_usd,
            returned_output: !timed_out && run_status != status::PROVIDER_ERROR,
        }
    }
}

/// `Percentile(sorted, p)`: nearest rank, no interpolation. `rank = ceil(p·n)` clamped to
/// `[1, n]`; the value is `sorted[rank - 1]`. `sorted` must be ascending and non-empty.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    // The cast saturates (a negative or NaN rank becomes 0), then the clamp applies.
    let rank = ((p * n as f64).ceil() as usize).clamp(1, n);
    sorted[rank - 1]
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// `Observe(tasks, runs)`: the cell's observation for one batch.
///
/// - `quality`, `ambiguity`: means over the gradable pairs.
/// - `pass_rate`: passing runs over all runs (errors and timeouts are failures).
/// - `latency_p50_ms`, `latency_p95_ms`: nearest-rank percentiles over the runs with a
///   latency (timeouts censored, D7).
/// - `cost_per_run_usd`: mean over the runs with a cost.
/// - `availability`: runs that returned output over all runs (D6).
pub fn observe(tasks: &[TaskOutcome], runs: &[RunStats]) -> BTreeMap<&'static str, f64> {
    let mut out = BTreeMap::new();
    let gradable: Vec<&TaskOutcome> = tasks.iter().filter(|t| t.gradable).collect();
    let scores: Vec<f64> = gradable.iter().map(|t| t.score).collect();
    let ambiguities: Vec<f64> = gradable.iter().map(|t| t.ambiguity).collect();
    if let (Some(quality), Some(ambiguity)) = (mean(&scores), mean(&ambiguities)) {
        out.insert(dimension::QUALITY, quality);
        out.insert(dimension::AMBIGUITY, ambiguity);
    }
    if !runs.is_empty() {
        let n = runs.len() as f64;
        let passed = runs.iter().filter(|r| r.pass).count() as f64;
        out.insert(dimension::PASS_RATE, passed / n);
        let returned = runs.iter().filter(|r| r.returned_output).count() as f64;
        out.insert(dimension::AVAILABILITY, returned / n);
    }
    let mut latencies: Vec<f64> = runs
        .iter()
        .filter_map(|r| r.latency_ms.map(f64::from))
        .collect();
    if !latencies.is_empty() {
        latencies.sort_by(f64::total_cmp);
        out.insert(dimension::LATENCY_P50_MS, percentile(&latencies, 0.50));
        out.insert(dimension::LATENCY_P95_MS, percentile(&latencies, 0.95));
    }
    let costs: Vec<f64> = runs.iter().filter_map(|r| r.cost_usd).collect();
    if let Some(cost) = mean(&costs) {
        out.insert(dimension::COST_PER_RUN_USD, cost);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_run_censors_timeouts_and_marks_output() {
        let timeout = RunStats::from_run(status::TIMEOUT, false, None, None, 120_000);
        assert_eq!(timeout.latency_ms, Some(120_000));
        assert!(!timeout.returned_output);
        let error = RunStats::from_run(status::PROVIDER_ERROR, false, Some(30), None, 120_000);
        assert_eq!(error.latency_ms, Some(30));
        assert!(!error.returned_output);
        let ok = RunStats::from_run(status::OK, true, Some(900), Some(0.01), 120_000);
        assert_eq!(ok.latency_ms, Some(900));
        assert!(ok.returned_output && ok.pass);
        let parse = RunStats::from_run(status::PARSE_FAILURE, false, None, None, 120_000);
        assert!(parse.returned_output);
        assert_eq!(parse.latency_ms, None);
    }

    #[test]
    fn censored_timeouts_raise_the_percentiles_and_lower_availability() {
        let runs = [
            RunStats::from_run(status::OK, true, Some(100), None, 1_000),
            RunStats::from_run(status::OK, true, Some(200), None, 1_000),
            RunStats::from_run(status::TIMEOUT, false, None, None, 1_000),
            RunStats::from_run(status::PROVIDER_ERROR, false, None, None, 1_000),
        ];
        let observed = observe(&[], &runs);
        assert_eq!(observed[dimension::LATENCY_P95_MS], 1_000.0);
        assert_eq!(observed[dimension::LATENCY_P50_MS], 200.0);
        assert_eq!(observed[dimension::AVAILABILITY], 0.5);
        assert_eq!(observed[dimension::PASS_RATE], 0.5);
    }

    #[test]
    fn percentile_rank_bounds() {
        assert_eq!(percentile(&[1.0, 2.0], -1.0), 1.0);
        assert_eq!(percentile(&[1.0, 2.0], 2.0), 2.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 0.34), 2.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 0.33), 1.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], f64::NAN), 1.0);
    }
}
