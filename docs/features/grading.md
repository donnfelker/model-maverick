# Grading

*How raw model output becomes dimension scores: deterministic graders first,
a cross-family LLM judge only where judgment is genuinely required.*

## The problem it solves

"Did the model do well?" is multi-dimensional — an extraction can be
schema-valid but factually wrong; a translation can be accurate but
unidiomatic. Two traps dominate naive grading. First, using an LLM to grade
everything: expensive, noisy, and circular. Second, LLM self-preference —
judges systematically favor outputs from their own model family, which would
quietly bias the entire matrix. A third, subtler trap: treating grader
*failures* as zero scores, which poisons aggregates with infrastructure
noise disguised as model weakness.

## How it works

- **Graders are data.** Every task carries its graders as configuration,
  validated at corpus load time. Each grader emits one or more named
  dimensions scored between 0 and 1; the task's dimension weights combine
  them into run quality (see [Scoring](scoring.md)).
- **Five grader kinds in v1**, deterministic wherever possible:
  - *JSON schema* — extracts the first JSON value and validates against an
    embedded schema, with optional per-field checks.
  - *Rule scan* — ordered must-include / must-not-include / length /
    language rules; a failed critical rule zeroes the grader's dimensions.
  - *Golden answer* — normalized comparison against accepted answers (exact,
    numeric tolerance, set equality, regex-anchored).
  - *Key facts* — weighted fact coverage, plus forbidden facts that act as
    hallucination traps and subtract when present.
  - *Judge rubric* — the LLM judge, for qualities rules can't reach (idiom,
    readability, holistic quality).
  - A *test-suite* kind is schema-reserved but rejected in v1: **no
    model-generated code is ever executed**. Code generation is graded by
    static assertions, checkable outputs, and reference-anchored judging.
- **The judge protocol.** A pool of two columns from different model
  families; for any output, the judge is the first pool member whose family
  differs from the graded column's — every output is judged cross-family,
  including outputs from the judges' own families. Each judgment is two
  structured-output calls at temperature zero with rubric order reversed;
  the score is the mean, and the disagreement between the two calls feeds
  the ambiguity penalty in [Scoring](scoring.md). Judges receive reference
  material (golden answer, hidden reference solution) — reference-anchored
  judging — but rubrics and references never reach benchmarked models
  (guarantee G7).
- **Failure is absence, not zero.** A grader or judge error marks the
  affected dimensions absent for that run and increments a failure counter;
  elevated judge-failure rates in a batch raise an operator
  [alert](alerts-delivery.md). Nothing silently scores zero.
- **Empirical judge selection.** Judging is itself a measured capability
  (critique/judging cells), so the operator re-points the judge pool at
  whatever columns actually judge best — the procedure is in
  [Operations](../operations.md).

## Dependencies

- Grades outputs produced by the [Benchmark runner](benchmark-runner.md) for tasks defined in the [corpus](capability-matrix.md).
- Emits the dimensions consumed by [Scoring & aggregation](scoring.md), including the multi-grader disagreement that becomes the ambiguity penalty.
- Judge failures surface through [Alerts](alerts-delivery.md); judge pool configuration is an operator concern in [Operations](../operations.md).
- Grading dimensions per capability and the high-stakes overlay: [`specs/capability-matrix-design.md`](../../specs/capability-matrix-design.md).
- Normative contract: [spec §5.2–§5.5](../../specs/model-maverick-v1-spec.md).
