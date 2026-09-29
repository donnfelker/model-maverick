//! Graders (spec §5.2, §5.4): the spec envelope, spec validation, and the grading
//! framework. The five grader kinds build on these types.

pub mod envelope;
pub mod golden;
pub mod goregex;
pub mod json_schema;
pub mod keyfacts;
pub mod registry;
pub mod rulescan;
pub mod spec;

pub use envelope::{
    Envelope, EnvelopeError, EnvelopeErrorKind, GraderKind, TEST_SUITE, VERSION, is_known_type,
    parse_envelope, parse_envelope_value, raw_type,
};
pub use golden::GoldenAnswer;
pub use json_schema::JsonSchema;
pub use keyfacts::KeyFacts;
pub use registry::{
    BoxError, BoxFuture, FailureCounter, GradeError, GradeInput, GradeResult, Grader, GraderError,
    RegisterError, Registry, RunMeta,
};
pub use rulescan::RuleScan;
pub use spec::{SpecValidator, ValidationRegistry, check_schema};
