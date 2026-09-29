//! Scoring (spec §6): run quality and pass, the high-stakes overlay, and (in later
//! beads) the variance and ambiguity penalties and cell aggregation.

pub mod overlay;
pub mod run_quality;

pub use overlay::{apply_autofail, autofail, high_stakes, overlay_weights};
pub use run_quality::{RunScore, run_quality, status};
