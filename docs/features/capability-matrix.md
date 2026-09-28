# The capability matrix and the private corpus

*The measured heart of the product: a continuously refreshed grid of how well
each model column performs each kind of work at each difficulty, fed by a
private, versioned benchmark corpus.*

## The problem it solves

Model choice decays. A team picks a model at project start, the ecosystem
ships better/cheaper models monthly, and nobody re-evaluates because
re-evaluating is expensive. The public alternatives don't help: leaderboards
are contaminated (test sets leak into training data), gamed, aggregated into
a single number that hides workload differences, and say nothing about cost
or latency. "Model X is #1" is useless when the real question is "what should
run *this* extraction task at *this* difficulty within *this* budget."

The matrix answers the real question by decomposing performance into
**cells**: one cell per (model column, capability, difficulty tier), each
carrying quality, pass rate, ambiguity, latency (p50/p95), and cost — all
measured, all dated, all versioned.

## How it works

- **Ten capabilities** (code generation, structured extraction,
  summarization, multi-step reasoning, instruction/schema following,
  long-context QA, factual QA, rewriting, translation, critique/judging) ×
  **five difficulty tiers** form the grid's axes; the third axis is the
  [model column](model-catalog.md).
- **The corpus** is 20–25 authored tasks per capability spanning all five
  tiers, living in a **separate private repository** — never in any public
  artifact. Each task carries its prompt, grading configuration, dimension
  weights, and pass threshold as data (see [Grading](grading.md)).
- **Smoke subset:** five tasks per capability are marked smoke members,
  always including an easy anchor (tier ≤ 2) and a hard anchor (tier ≥ 4), so
  the cheap nightly run still brackets the difficulty range instead of
  sampling the middle.
- **Versioning:** the corpus loads under a date-stamped version
  (`matrix_version`, e.g. `2026-07-01`); validation runs before any write and
  activation flips atomically. Every API response stamps the matrix version
  it answered from, so any recommendation can be traced to the exact corpus
  that produced its numbers.
- Cells are fed nightly and weekly by the [benchmark runner](benchmark-runner.md),
  scored by [graders](grading.md), and smoothed into decision-grade numbers
  by [scoring and aggregation](scoring.md).

## Why the corpus is private (G7)

Contamination resistance is the whole ballgame: a public benchmark stops
measuring the model and starts measuring memorization. Task prompts, golden
answers, hidden rubrics, and grader internals never leave the engine through
any public API and never enter a benchmarked model's prompt beyond the task
prompt itself. The [check specs](recommendation.md) callers receive validate
output *shape*, not content, precisely so nothing corpus-derived leaks.

The authored corpus doubles as labeled training data for a future trained
classifier (see [Classification](classification.md)).

## Dependencies

- Measured over: [Model catalog](model-catalog.md) — the column is the unit of measurement.
- Fed by: [Benchmark runner](benchmark-runner.md) → [Grading](grading.md) → [Scoring & aggregation](scoring.md).
- Consumed by: [Recommendation & ranking](recommendation.md), [Policy engine](policy-engine.md), [Calibration](calibration.md).
- Methodology (dimensions per capability, difficulty tiers, judge bias handling, statistical grounding): [`specs/capability-matrix-design.md`](../../specs/capability-matrix-design.md).
- Normative contract: [spec §1, §3, §5.6](../../specs/model-maverick-v1-spec.md).
- Operator procedures for loading/activating versions: [Operations — corpus versioning](../operations.md).
