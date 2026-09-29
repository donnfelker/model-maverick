# Jobs and scheduling

`maverick-jobs` runs the engine's background work on `graphile_worker` 0.13.x. Postgres is the only queue and the only backend. There is no Redis and no River. The worker applies its own schema migrations alongside the engine's, and it runs at a concurrency of 10.

## The job kinds

There are eleven kinds (`JobKind`).

| Kind | Purpose |
|---|---|
| `prediction_lock` | Snapshot each scheduled cell's pass probability, implied by its pass-rate EWMA, before the night's batch. First lock wins. |
| `nightly_smoke` | The nightly benchmark batch over the smoke-member tasks. |
| `weekly_sweep` | The weekly full-corpus benchmark batch. |
| `aggregate_rebuild` | Fold a batch's runs and scores into the cell aggregates. The choke point that chains the follow-ups. |
| `regression_scan` | Detect quality regressions on folded cells, plus the parse-failure watch. |
| `calibration_score` | Compare the locked predictions with the observed outcomes (Brier, overconfidence). |
| `latency_probes` | Hourly thin latency pings for secondary-endpoint columns. |
| `price_sync` | Refresh each runnable column's pricing snapshot from the price source. |
| `catalog_sync` | Lifecycle transitions, the pool-drift check, and retention-expiry alerts. |
| `capture_expiry` | Hourly hard-delete of expired debug captures and windows. |
| `history_prune` | Daily prune of aged history. Aggregates, calibration, and audit rows are never pruned. |

The batch kinds (`nightly_smoke`, `weekly_sweep`) get three attempts and a batch timeout. The chained and hourly kinds (`aggregate_rebuild`, `regression_scan`, `calibration_score`, `latency_probes`) get an analytics timeout. The rest are fast, with five attempts and a 60 s timeout.

## Cron

Eight kinds are scheduled by cron. The defaults are defined identically in `maverick-config` and `maverick-jobs`.

| Variable | Default | Kind |
|---|---|---|
| `MAVERICK_CRON_PREDICTION_LOCK` | `50 2 * * *` | prediction lock, 02:50 daily |
| `MAVERICK_CRON_NIGHTLY` | `0 3 * * *` | nightly smoke, 03:00 daily |
| `MAVERICK_CRON_WEEKLY` | `0 4 * * 0` | weekly sweep, 04:00 Sundays |
| `MAVERICK_CRON_LATENCY_PROBES` | `0 * * * *` | latency probes, hourly |
| `MAVERICK_CRON_PRICE_SYNC` | `0 6 * * *` | price sync, 06:00 |
| `MAVERICK_CRON_CATALOG_SYNC` | `10 6 * * *` | catalog sync, 06:10 |
| `MAVERICK_CRON_CAPTURE_EXPIRY` | `0 * * * *` | capture expiry, hourly |
| `MAVERICK_CRON_HISTORY_PRUNE` | `0 5 * * *` | history prune, 05:00 |

The chained kinds (`aggregate_rebuild`, `regression_scan`, `calibration_score`) have no cron. A preceding job enqueues them. Because `graphile_worker` keeps a crontab payload fixed, each scheduled kind gets a small trigger task that, on firing, reads the scheduled time and enqueues the real job for that window under the kind's job key.

## The batches lease

The nightly and weekly batches are long, and more than one worker can pick up the same window, so the engine coordinates them with a lease. The `batches` row keyed by (kind, batch window) is the lease. This is new in the Rust port; the Go engine used River unique jobs.

- `claim_batch` inserts or locks the row in one transaction and reports the outcome: claimed, took over, finished, or held by another worker.
- The default lease is 60 s (`MAVERICK_BATCH_LEASE_TTL`), and a handler renews it every 15 s (`MAVERICK_BATCH_HEARTBEAT_INTERVAL`) using the database clock, so every worker agrees on the deadline.
- A takeover happens when the lease is expired, unowned, owned by the caller, or when the owner is gone. The last case is checked against the worker heartbeats with a five-minute recovery threshold, which allows a takeover before the lease expires. The attempt count increments on a takeover.
- `finish_batch` writes the completed or skipped status and clears the lease.

A batch resumes where it left off: a resumed batch skips runs already done (D10), and the rebuild is idempotent per batch.

## Graceful shutdown

The engine owns the signals, so the workers are told not to listen for OS signals themselves. On SIGINT or SIGTERM there is one ten-second deadline:

1. The HTTP servers stop accepting and drain first.
2. The workers are told to stop and drain their in-flight jobs within the deadline.
3. On timeout the worker task is aborted. Its unfinished jobs retry after the `graphile_worker` recovery sweep, and an unfinished batch resumes under its lease (D10).
4. OpenTelemetry is flushed and shut down.

The signal handlers are installed at startup, so a signal that arrives during startup is not lost.
