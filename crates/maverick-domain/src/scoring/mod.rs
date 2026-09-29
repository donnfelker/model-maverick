//! Scoring (spec §6): run quality and pass, the high-stakes overlay, the variance and
//! ambiguity penalties, cell observation, the EWMA fold, and regression detection.

pub mod ewma;
pub mod observe;
pub mod overlay;
pub mod penalties;
pub mod regression;
pub mod run_quality;

pub use ewma::{ALPHA, MIN_NIGHTS, StoredAggregate, Stream, f32_round, fold_observation};
pub use observe::{RunStats, TaskOutcome, dimension, observe, percentile};
pub use overlay::{apply_autofail, autofail, high_stakes, overlay_weights};
pub use penalties::{
    LAMBDA_AMB, LAMBDA_VAR, Variance, ambiguity, ambiguity_penalty, grader_observations,
    order_swap_observations, shared_dimension_spread, variance_penalty,
};
pub use regression::{
    AlertMark, DEFAULT_QUALITY_BAR, Finding, PARSE_FAILURE_RATE_FLOOR,
    PARSE_FAILURE_TRAILING_MULTIPLE, RegressionKind, RegressionParams, SeriesPoint,
    detect_regressions, filter_alertable, parse_failure_alert, parse_failure_rate,
    regression_severity, severity,
};
pub use run_quality::{RunScore, run_quality, status};
