//! Background jobs on graphile_worker.
//!
//! Holds the `batches` lease and status per job kind and window, the spend guard, the
//! batch runners, the rebuild chain, alert jobs, and catalog jobs. Postgres holds the
//! whole queue; there is no Redis. Package 2e fills this in. The skeleton is
//! intentionally empty.
