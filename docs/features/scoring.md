# Scoring and aggregation

*From noisy per-run scores to decision-grade cells: penalties for
inconsistency, exponential smoothing, regression detection, and difficulty
ceilings.*

## The problem it solves

Single benchmark runs are noise. Models vary run to run; graders can
disagree with each other; one bad provider night shouldn't crater a score
that took weeks to earn — yet a genuine regression (a stealth model swap
behind an API, a quantization change) must surface within a night or two.
And the question tenants actually ask — "how hard a task can I trust this
model with?" — needs a defensible, bar-relative answer, not a vibe.

## How it works

- **Run quality** is the task's weighted sum of graded dimensions. Runs that
  produced no gradable output (provider error, timeout) score zero and fail;
  truncated or refused outputs are graded on whatever text exists — the low
  scores they earn are real signal. A high-stakes overlay (medical, legal,
  financial tags) reweights dimensions and force-fails runs with dangerous
  or fabricated content.
- **Variance penalty.** Across a pair's three repeats, the task score is the
  mean minus half a standard deviation: a model that nails a task twice and
  faceplants once is worth less than its average suggests. Consistency is
  part of quality.
- **Ambiguity penalty.** When two graders score the same dimension on the
  same output (for example key-facts and the judge both scoring
  faithfulness), their disagreement subtracts from the task score at quarter
  weight, and is also aggregated per cell as its own dimension. Disagreement
  between measurement instruments is uncertainty, and uncertainty should
  cost something. Judge order-swap disagreement
  (see [Grading](grading.md)) folds in here.
- **EWMA smoothing.** A batch's cell observation is the mean of its task
  scores; cells fold observations into an exponentially weighted moving
  average with α = 0.3 — one noisy night moves the smoothed score about a
  third of the way, three consistent nights dominate. A running variance
  accompanies each cell, and a cell is `measured` only after enough distinct
  nights. Aggregates are **append-only**: queries read the latest, history
  endpoints read the series.
- **Regression detection.** A cell flags when the smoothed value drops more
  than three quality points below its trailing week's high, or when a raw
  observation shocks below the smoothed value by more than twice the cell's
  own noise level. Flags raise [alerts](alerts-delivery.md) — escalated to
  critical when the drop crosses a quality bar of a tenant actually pooling
  that column — with deduplication so a sustained dip doesn't page nightly.
- **Ceilings.** For a column × capability at a tenant's quality bar, the
  ceiling is the highest tier such that every tier at or below it holds a
  pass rate above the bar plus a safety buffer. Ceilings power the banded
  policy table (cheapest column that clears each difficulty band) and the
  per-entry `clears_difficulty` flag in
  [recommendations](recommendation.md).
- **Absolute display.** Displayed quality is the same 0–100 scale
  everywhere; the min-max normalization used for ranking is internal and
  never shown, so a 94.1 means the same thing in every response.

## Dependencies

- Consumes dimensions from [Grading](grading.md) over runs from the [Benchmark runner](benchmark-runner.md).
- Produces the cells of the [Capability matrix](capability-matrix.md) consumed by [Recommendation](recommendation.md) and the [Policy engine](policy-engine.md).
- EWMA-implied pass probabilities seed [Calibration](calibration.md)'s locked predictions.
- Regression and parse-failure signals flow to [Alerts](alerts-delivery.md); rebuild/scan ordering is owned by [Jobs & scheduling](jobs-scheduling.md).
- Statistical grounding for the penalties and thresholds: [`specs/capability-matrix-design.md`](../../specs/capability-matrix-design.md).
- Normative contract: [spec §6](../../specs/model-maverick-v1-spec.md).
