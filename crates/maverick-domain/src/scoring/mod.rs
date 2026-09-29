//! Scoring (spec §6): run quality and pass, the high-stakes overlay, the variance and
//! ambiguity penalties, and (in later beads) cell aggregation.

pub mod overlay;
pub mod penalties;
pub mod run_quality;

pub use overlay::{apply_autofail, autofail, high_stakes, overlay_weights};
pub use penalties::{
    LAMBDA_AMB, LAMBDA_VAR, Variance, ambiguity, ambiguity_penalty, grader_observations,
    order_swap_observations, shared_dimension_spread, variance_penalty,
};
pub use run_quality::{RunScore, run_quality, status};
