# Jobs, scheduling, and the spend guard

*All background work — benchmarking, aggregation, syncs, pruning — on a
Postgres-backed queue with idempotent scheduling, deterministic chaining,
and a hard financial ceiling.*

## The problem it solves

The matrix is fed by recurring, money-spending background work. That
demands real scheduling (a missed nightly batch is a hole in the data),
idempotency (a restarted worker must not double-run a batch), ordering (you
cannot scan for regressions before aggregates rebuild), and a financial
backstop (a corpus edit or catalog growth must not silently triple the
provider bill). And it should demand **no new infrastructure**: v1 is one
binary and one database by design — the queue lives in Postgres (River), so
jobs are transactional with the data they operate on and there is no Redis
to run, monitor, or lose.

## How it works

- **The job table.** Eleven periodic jobs, cron-configured, UTC: the
  prediction lock (strictly before the nightly batch — see
  [Calibration](calibration.md)); the nightly smoke and weekly sweep
  batches ([Benchmark runner](benchmark-runner.md)); hourly latency probes
  for secondary-endpoint columns ([Model catalog](model-catalog.md));
  aggregate rebuild, regression scan, and calibration scoring
  ([Scoring](scoring.md)); daily price and catalog syncs (the latter runs
  the pool-drift check from the [Policy engine](policy-engine.md) and
  routes sunset retirements through the retirement guard); hourly capture
  expiry ([Privacy](privacy-capture.md)); and the daily history prune
  ([History](history-audit.md)).
- **Idempotency and chaining.** Every job is unique per (job, batch
  window) — reschedules and restarts collapse into one execution. Batches
  chain: a completed batch enqueues the aggregate rebuild, which enqueues
  the regression scan and calibration score. A partially failed batch
  still rebuilds from what completed, records its completion percentage,
  and raises a job-failure [alert](alerts-delivery.md) below threshold —
  partial data plus a loud signal beats no data.
- **The spend guard (G8).** Before executing, every batch projects its cost
  from prompt sizes, output caps, and current pricing. Over the cap
  (separate nightly and weekly ceilings), the job **refuses to run** and
  raises a critical spend-refusal alert carrying the projection; at 80% it
  runs but warns. Actual spend is recorded per batch, so projections are
  auditable against reality. The cap is a hard ceiling on mistakes: no
  corpus edit, catalog addition, or bug can spend past it unnoticed.
- **The parse-failure watch.** Alongside regression scanning, each column's
  batch parse-failure rate is compared to its own trailing baseline;
  elevated *and* anomalous rates alert. This is a leading indicator — rising
  parse failures at benchmark time predict caller-side silent escalation
  before tenants feel it.
- **Timeouts sized to the work.** River's own default per-job timeout (one
  minute) suits the chained/hourly jobs but is far too short for a real
  batch — a full weekly sweep runs thousands of provider calls over hours.
  `nightly_smoke` and `weekly_sweep` override it (configurable, sanity-capped
  so a misconfiguration can't wedge a batch open indefinitely); the
  chained/hourly jobs use a fixed, shorter override since none of them place
  provider calls at batch scale. Even a batch that outruns its budget still
  gets its job-failure alert and its aggregate-rebuild enqueue: both run on
  a short-lived context detached from the job's own, so the failure tail
  survives independently of a job context that may have already expired.
- **As-built note:** the price sync job has no external price source wired
  in v1; pricing changes arrive as operator data edits through the
  [internal API](internal-provisioning.md).

## Dependencies

- Executes and sequences: [Benchmark runner](benchmark-runner.md), [Scoring](scoring.md), [Calibration](calibration.md), [Model catalog](model-catalog.md) syncs, [Privacy](privacy-capture.md) expiry, [History](history-audit.md) prune.
- Raises and delivers: [Alerts & delivery](alerts-delivery.md) (workers share the same delivery fan-out as the API).
- Queue health is visible on the internal health endpoint ([Observability](observability.md)); schedules and tuning: [Operations](../operations.md).
- Normative contract: [spec §11](../../specs/model-maverick-v1-spec.md).
