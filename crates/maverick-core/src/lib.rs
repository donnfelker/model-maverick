//! The shared SDK core (Rust copy).
//!
//! The recommend client, the chain executor, the fail-open ladder, the telemetry
//! batcher, the alias grammar, and workload projection. This is the logic the SDKs
//! share; the trial proxy links it directly. It never executes inference on its own
//! credentials. Package 2d fills this in. The skeleton is intentionally empty.
