//! Grader spec types: the envelope and spec validation (spec §5.4).
//!
//! The grading framework (the grader trait, the grading registry, the failure
//! counter, `MapDimensions`) and the five grader kinds build on these types.

pub mod envelope;
pub mod spec;

pub use envelope::{
    Envelope, EnvelopeError, EnvelopeErrorKind, GraderKind, TEST_SUITE, VERSION, is_known_type,
    parse_envelope, parse_envelope_value, raw_type,
};
pub use spec::{SpecValidator, ValidationRegistry, check_schema};
