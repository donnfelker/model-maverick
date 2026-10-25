# Jobs and scheduling

`maverick-jobs` runs the engine's background work on `graphile_worker` 0.13.x. Postgres is the only queue and the only backend. There is no Redis and no River. The worker applies its own schema migrations alongside the engine's, and it runs at a concurrency of 10.

## The job kinds

Each kind is one `JobKind` variant, and each has a row here (`crates/maverick-jobs/tests/job_docs.rs` checks it).

| Kind | Purpose |
|---|---|
| `prediction_lock` | Snapshot each scheduled cell's pass probability, implied by its pass-rate EWMA, before the night's batch. First lock wins. |
| `nightly_smoke` | The nightly benchmark batch over the smoke-member tasks. |
| `weekly_sweep` | The weekly full-corpus benchmark batch. |
| `aggregate_rebuild` | Fold a batch's runs and scores into the cell aggregates. The choke point that chains the follow-ups. |
| `regression_scan` | Detect quality regressions on folded cells, plus the parse-failure watch. |
| `calibration_score` | Compare the locked predictions with the observed outcomes (Brier, overconfidence). |
| `matrix_export` | On an origin that exports, write the signed public matrix bundle after each rebuild (D49). |
| `matrix_pull` | On a replica with a pull URL, import the newest readable bundle each hour (D49). |
| `latency_probes` | Hourly thin latency pings for secondary-endpoint columns. |
| `price_sync` | Refresh each runnable column's pricing snapshot from the price source. |
| `catalog_sync` | Lifecycle transitions, the pool-drift check, and retention-expiry alerts. On a replica, `matrix_stale` instead of the transitions. |
| `model_discovery` | Daily comparison of each provider's text models with the catalog (D51). It never changes the catalog. |
| `capture_expiry` | Hourly hard-delete of expired debug captures and windows. |
| `affinity_prune` | Hourly delete of expired conversation affinity rows (D44). |
| `history_prune` | Daily prune of aged history. Aggregates, calibration, and audit rows are never pruned. |

The role decides which kinds a worker registers (D49). A replica registers no corpus kind
(`prediction_lock`, the two batches, the three rebuild follow-ups, `model_discovery`), and
never claims one that is already queued. `matrix_export` registers only on an origin with
`MAVERICK_MATRIX_EXPORT_DIR`, `matrix_pull` only on a replica with
`MAVERICK_MATRIX_PULL_URL`, and a replica's `price_sync` only with
`MAVERICK_CRON_PRICE_SYNC`.

The batch kinds (`nightly_smoke`, `weekly_sweep`) get three attempts and a batch timeout. The chained and hourly kinds (`aggregate_rebuild`, `regression_scan`, `calibration_score`, `matrix_export`, `matrix_pull`, `latency_probes`) get an analytics timeout. The rest are fast, with five attempts and a 60 s timeout.

## Cron

These kinds are scheduled by a configurable cron. The defaults are defined identically in `maverick-config` and `maverick-jobs`. `matrix_pull` also runs from cron, at the fixed `0 * * * *`, when its URL is set.

| Variable | Default | Kind |
|---|---|---|
| `MAVERICK_CRON_PREDICTION_LOCK` | `50 2 * * *` | prediction lock, 02:50 daily |
| `MAVERICK_CRON_NIGHTLY` | `0 3 * * *` | nightly smoke, 03:00 daily |
| `MAVERICK_CRON_WEEKLY` | `0 4 * * 0` | weekly sweep, 04:00 Sundays |
| `MAVERICK_CRON_LATENCY_PROBES` | `0 * * * *` | latency probes, hourly |
| `MAVERICK_CRON_PRICE_SYNC` | `0 6 * * *` | price sync, 06:00 |
| `MAVERICK_CRON_CATALOG_SYNC` | `10 6 * * *` | catalog sync, 06:10 |
| `MAVERICK_CRON_MODEL_DISCOVERY` | `20 6 * * *` | model discovery, 06:20 |
| `MAVERICK_CRON_CAPTURE_EXPIRY` | `0 * * * *` | capture expiry, hourly |
| `MAVERICK_CRON_AFFINITY_PRUNE` | `5 * * * *` | affinity prune, hourly at :05 |
| `MAVERICK_CRON_HISTORY_PRUNE` | `0 5 * * *` | history prune, 05:00 |

The chained kinds (`aggregate_rebuild`, `regression_scan`, `calibration_score`, `matrix_export`) have no cron. A preceding job enqueues them. Because `graphile_worker` keeps a crontab payload fixed, each scheduled kind gets a small trigger task that, on firing, reads the scheduled time and enqueues the real job for that window under the kind's job key.

## The batches lease

The nightly and weekly batches are long, and more than one worker can pick up the same window, so the engine coordinates them with a lease. The `batches` row keyed by (kind, batch window) is the lease. This is new in the Rust port; the Go engine used River unique jobs.

- `claim_batch` inserts or locks the row in one transaction and reports the outcome: claimed, took over, finished, or held by another worker.
- The default lease is 60 s (`MAVERICK_BATCH_LEASE_TTL`), and a handler renews it every 15 s (`MAVERICK_BATCH_HEARTBEAT_INTERVAL`) using the database clock, so every worker agrees on the deadline.
- A takeover happens when the lease is expired, unowned, owned by the caller, or when the owner is gone. The last case is checked against the worker heartbeats with a five-minute recovery threshold, which allows a takeover before the lease expires. The attempt count increments on a takeover.
- `finish_batch` writes the completed or skipped status and clears the lease.

A batch resumes where it left off: a resumed batch skips runs already done (D10), and the rebuild is idempotent per batch.

One `graphile_worker` behavior needs cleanup. On a duplicate enqueue while the keyed job is locked (running), `graphile_worker` clears that job's key, marks its attempts exhausted, and inserts a fresh keyed job (its migration m000019). If the running worker then dies, the recovery sweep unlocks the old row, but that row can never run again. The batch completion transaction deletes such rows for its task and night (`batches::delete_orphaned_failed_jobs`), so no permanently failed job is left behind (spec §11.2).

## Graceful shutdown

The engine owns the signals, so the workers are told not to listen for OS signals themselves. On SIGINT or SIGTERM there is one ten-second deadline:

1. The HTTP servers stop accepting and drain first.
2. The workers are told to stop and drain their in-flight jobs within the deadline.
3. On timeout the worker task is aborted. Its unfinished jobs retry after the `graphile_worker` recovery sweep, and an unfinished batch resumes under its lease (D10).
4. OpenTelemetry is flushed and shut down.

The signal handlers are installed at startup, so a signal that arrives during startup is not lost.
