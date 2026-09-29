# Package 2e parity closeout

- Bead: `model-maverick-rust-port-3hk.7.14`
- Date: 2026-09-29
- Basis: committed `main` plus the tag and test changes in this same commit.
- Reproduce: `cargo run -p parity-support --bin parity_coverage -- --list-missing`

## What this is

Package 2e is the jobs crate `maverick-jobs`: the batches, the aggregate rebuild, the
regression scan, the prediction lock and calibration score, the latency probes, the catalog
and price jobs, the retention jobs, and the worker wiring. Its parity area is
`measurement-core` (`parity/manifest/measurement-core.json`), together with the domain
scoring and prediction code that the jobs call. This report checks each item of the 7.14
checklist.

## Coverage summary

| area | total | tagged | divergence | missing |
|------|-------|--------|------------|---------|
| api | 256 | 220 | 0 | 36 |
| decision | 105 | 97 | 0 | 8 |
| measurement-core | 173 | 173 | 0 | 0 |
| measurement-io | 144 | 144 | 0 | 0 |
| proxy | 66 | 66 | 0 | 0 |
| store | 92 | 82 | 0 | 10 |
| TOTAL | 836 | 782 | 0 | 54 |

Every `measurement-core` row has a Rust test. The other areas belong to other packages; the
counts are a snapshot at this commit.

## Fixes applied in this closeout

- **Six cron rows were untested by tag only.** The unit tests in `crates/maverick-jobs/src/lib.rs` already checked `cron.json` exactly. They now carry their tags: `TestParseCronAcceptsConfigDefaults`, `TestParseCronRejectsInvalid`, `TestCronNext`, `TestCronFiresOn` (with the invalid-expression error), `TestIsoWeek`, and `TestNewClientRejectsBadNightlyCron`.
- **Three catalog rows had no Rust test.** `crates/maverick-jobs/tests/catalog_lookup.rs` adds them: `catalog.TestClientsFor` (the provider client for a column), `catalog.TestColumnLookupByIDAndLifecycleState` (by id, runnable, and by status), and `catalog.TestMain` (the Postgres harness). Go's `Clients{openai: nil}` case cannot be built in Rust, because a registered client is an `Arc` and never null. A provider with no registered client gives the same error.
- **The cron trigger tasks now run under the fast timeout.** The `cron.<kind>` triggers (bead 3.12) only enqueue. They now run under `FAST_JOB_TIMEOUT` like every other handler.

## The checklist

1. **`parity/fixtures/measurement-core/` passes bit-exact under float32 storage.** `cargo test -p maverick-domain -p maverick-jobs` passes. The fold tests compare the stored float32 EWMA and variance bit for bit (bead 7.5).
2. **Every `measurement-core` row has a Rust test or a divergence row.** 173 of 173 are tagged.
   - The two corpus-dependent rows (`TestRealCorpusSimulatedNight`, `TestFullMatrixWeeklySweep`) are in `tests/corpus_gates.rs` (bead 7.13).
   - Both pass against the private corpus checkout (`MAVERICK_CORPUS_DIR`) and skip without it.
3. **The six crash and restart scenarios pass reliably.** `tests/crash_restart.rs` (bead 7.12) covers kill -9 then restart, kill -9 with the lease held live, SIGTERM, an enqueue while running, an enqueue after completion, and a handler timeout.
   - The interruptions do not depend on timing: the child hangs one chosen provider call.
   - Twenty consecutive runs passed, four of them under a concurrent full jobs suite.
4. **No handler uses graphile_worker's default `max_attempts`.**
   - Every enqueue goes through `JobKind::job_spec` or `rebuild::enqueue_in`. Both set the per-kind value: batches 3, chained and hourly jobs 5.
   - The cron triggers set `max = 5`.
5. **Every handler runs under its per-kind timeout.** `with_job_timeout` (bead 7.12) wraps all eleven job handlers. The cron triggers use `run_with_timeout` with `FAST_JOB_TIMEOUT`.
6. **No detached task carries durable work (D11).**
   - The batch failure tail, the rebuild chain, the scan's alerts, and the calibration rows each commit in the same transaction as their lease completion.
   - The only `tokio::spawn` in the crate is the worker runtime itself (`worker.rs`).
7. **cargo-mutants on the scoring and prediction domain code is clean.** `cargo mutants -p maverick-domain` over the modules the jobs call beyond the 2a closeout set (`scoring/regression.rs`, `predictions.rs`, `spend.rs`, `pricing.rs`): 213 mutants, 206 caught, 7 unviable, 0 missed. The 2a closeout covered `ewma.rs`, `penalties.rs`, and `run_quality.rs` (0 missed), and no scoring or prediction source changed since.

## Gates

- `cargo fmt --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `scripts/check-instrumentation.sh`: pass.
- `cargo deny check`: pass (advisories, bans, licenses, sources ok).
- `cargo test -p maverick-domain -p maverick-jobs` on real Postgres: pass (520 tests).

## Divergences

No new divergence is introduced. The jobs tests tag the rows they rely on:
- D6: availability, and no `q_run` for errors.
- D7: censored latency.
- D10: engine-owned leases.
- D11: the transactional failure tail.

## Open follow-up

`model-maverick-5bt` (P3) is open. When a job is enqueued again while its first delivery runs, graphile_worker's `add_jobs` sets the running job's `attempts` to its maximum. If that worker then crashes, the duplicate still completes the window, but the original stays behind as a permanently failed row.
