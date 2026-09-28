# Predictions and calibration

*The matrix grades its own homework — with the answer sheet locked away
before the exam.*

## The problem it solves

A recommendation engine is a prediction machine: every cell implicitly
claims "this column will pass this kind of task this often." Unchecked,
such claims drift into systematic overconfidence and nobody notices —
recommendations keep flowing, they just quietly get worse. The obvious
check, comparing scores to results after the fact, is hindsight-biased:
it's too easy to evaluate predictions you formed after seeing the outcome.

## How it works

- **Lock before the batch.** Shortly before each night's benchmark batch, a
  job snapshots the EWMA-implied pass probability for every cell scheduled
  to run that night. The predictions are timestamped and immutable —
  formed strictly before any result exists, so hindsight is structurally
  impossible.
- **Score after the batch.** Once the batch completes and aggregates
  rebuild, a chained job compares locked predictions to observed passes and
  computes, per capability, a Brier score (mean squared prediction error —
  rewards being right *and* being appropriately confident) and an
  overconfidence measure (mean predicted minus mean observed; positive
  means the matrix promises more than models deliver).
- **Exposed as a product surface.** The calibration series is queryable per
  capability through the history API, so a tenant can see not just what the
  engine recommends but how honest its underlying beliefs have been over
  time.
- **A ledger for future models.** Predictions record their source method.
  v1 predicts from EWMAs; when matrix completion or IRT-style models arrive
  (v1.1), they compete on the same locked ledger — a better predictor has
  to *prove* it on prospective data, not curve-fit the past.

## Dependencies

- Predictions derive from [Scoring & aggregation](scoring.md) EWMAs over [Capability matrix](capability-matrix.md) cells.
- Lock timing (strictly before the nightly batch) and post-batch chaining are enforced by [Jobs & scheduling](jobs-scheduling.md).
- The series is served by the history endpoints in the [API reference](../api.md); retention follows [History & audit](history-audit.md).
- Normative contract: [spec §3 (predictions, calibration), §11](../../specs/model-maverick-v1-spec.md).
