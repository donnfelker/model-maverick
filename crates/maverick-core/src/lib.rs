//! The shared SDK core (Rust copy).
//!
//! The recommend client, the chain executor, the fail-open ladder, the telemetry
//! batcher, the alias grammar, and workload projection. This is the logic the SDKs
//! share; the trial proxy links it directly. It never executes inference on its own
//! credentials. Packages 2a and 2d fill this in, one module at a time.

pub mod alias;
pub mod chain;
pub mod failopen;
pub mod gojson;
pub mod normalize;
pub mod openrouter;
pub mod projection;
pub mod recommend;
pub mod settings;
pub mod telemetry;
