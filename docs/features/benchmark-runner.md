# Benchmark runner

*Disciplined execution of task × column × repeats batches against real
provider APIs — because a matrix built on sloppy measurement lies with
confidence.*

## The problem it solves

Benchmark numbers are only as trustworthy as the execution that produced
them. The failure modes are mundane and fatal: prompts that drift slightly
between models (now you're comparing prompts, not models), retries that
silently mask provider flakiness, concurrency that distorts latency, and
single runs mistaken for signal when model output varies run to run.

## How it works

- **Batches.** A batch is one scheduled execution — the nightly smoke (smoke
  tasks × all non-retired columns) or the weekly sweep (full corpus × all
  non-retired columns) — of (task, column) pairs × three repeats. Cold-start
  columns run the full corpus even on smoke nights
  (see [Model catalog](model-catalog.md)).
- **Fan-out with bounds.** A worker pool per provider, bounded by per-provider
  concurrency and request-per-minute limits. Repeats of the same pair run
  *sequentially* — they exist to measure variance, not to race each other.
- **Honest failure.** Rate-limit and server errors retry a bounded number of
  times with backoff and jitter; when retries are exhausted the run is
  recorded as a provider error and **not rescheduled**. The
  [scoring math](scoring.md) treats missing repeats explicitly rather than
  papering over them.
- **Determinism.** Temperature comes from the task (default zero). Prompts
  are assembled byte-identically across columns — same task prompt, same
  attachment order — so any output difference is attributable to the model.
  Nothing tenant-related ever enters a benchmark prompt (guarantee G7,
  see [Privacy](privacy-capture.md)).
- **Measurement.** Latency is wall-clock from request start to final token,
  per run. Cost is computed from the column's pricing snapshot at execution
  time (see [Model catalog](model-catalog.md)) so later price changes never
  restate history.
- **Financial backstop.** Every batch computes projected cost before starting
  and refuses to run past its cap — the spend guard, described with the rest
  of the background machinery in [Jobs & scheduling](jobs-scheduling.md).

## Dependencies

- Executes tasks from the [Capability matrix corpus](capability-matrix.md) against [Model catalog](model-catalog.md) columns.
- Outputs flow to [Grading](grading.md), then [Scoring & aggregation](scoring.md).
- Scheduled, chained, and spend-guarded by [Jobs & scheduling](jobs-scheduling.md); failures surface via [Alerts](alerts-delivery.md).
- Tested against fake providers in CI; the only live-provider test is the opt-in smoke test in [Operations](../operations.md).
- Normative contract: [spec §5.1, §2.3](../../specs/model-maverick-v1-spec.md).
