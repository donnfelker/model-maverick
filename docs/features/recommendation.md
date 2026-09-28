# Recommendation and ranking

*The product's core promise: ask "which model should run this workload" and
always get a ranked, honest, auditable answer — never an empty one.*

## The problem it solves

Model selection is a live trade-off between quality, cost, and latency, and
the right balance differs per team, per capability, and per call. A useful
answer must be a *chain*, not a single name (callers need somewhere to
escalate when the pick fails), must never come back empty (a router that
refuses under imperfect data is worse than no router), and must be honest
about its own uncertainty rather than dressing a cold-start guess as a
measured verdict.

## How it works

- **Pipeline.** Resolve the workload ([Classification](classification.md))
  → filter to survivors ([Policy engine](policy-engine.md)) → rank from the
  workload cell's aggregates ([Scoring](scoring.md)) → persist the
  [decision trace](history-audit.md) → respond. The declared-workload path
  makes no provider calls and holds a hot-path latency budget measured in
  milliseconds.
- **Modes and weights.** Three modes — quality, balanced, cost — set
  default weights over normalized quality/cost/latency; tenants can
  override weights per capability. Normalization is min-max over the
  surviving set only, computed per query and never stored or displayed.
- **Banded ranking.** Each mode defines a quality band (zero for quality
  mode, wider for cost mode). Candidates within the band of the best
  quality are *leaders*, ordered by weighted value — this is where a
  cheaper, faster near-equal beats the absolute best. Everything below the
  band appends in quality order, so the tail of the list is a sane
  escalation chain. Near-ties inside a noise band prefer the cheaper
  column, with a bias toward same-family downgrades (cross-family swaps
  change failure modes, prompt behavior, and ops surface; a same-family
  sibling is the safer save).
- **Index zero is the pick; the array is the fallback chain** — and because
  it was filtered before ranking, the whole chain respects the pool (G2).
- **Honest flags over refusals (G1).** Constraint violations, difficulty
  ceilings the pick doesn't clear, and unmeasured (cold-start) data all
  arrive as flags on a real recommendation. In the as-built engine a fully
  cold pool still returns its best candidates with null scores and
  `measured: false` rather than erroring.
- **Check specs.** Every response names a machine-readable output check per
  capability, fetchable by reference — a JSON Schema or rule list a gateway
  can run caller-side to verify output *shape*. Deliberately generic: no
  corpus material can leak through them (G7,
  see [Capability matrix](capability-matrix.md)).
- **Reasons and contract.** Each entry carries a human-readable reason
  ("cheapest column clearing tier 3 …", "escalation target", "cross-family
  alternative"); the reasons are explanatory, the *flags* are contractual.
  Every response stamps its `recommendation_id` and `matrix_version` (G6).

## Dependencies

- Inputs: [Classification](classification.md) (workload), [Policy engine](policy-engine.md) (survivors), [Scoring](scoring.md) (cells, ceilings), [Model catalog](model-catalog.md) (column facts, quality transfer).
- Outputs: every response persists a trace to [History & audit](history-audit.md); outcomes reported automatically by MM-authored SDK/plugin/proxy components close the loop there.
- Same surface exposed to agents via the [MCP server](mcp-surface.md).
- Wire contract (request/response JSON, error codes): [API reference](../api.md); first-request walkthrough: [Onboarding](../onboarding.md).
- Normative contract: [spec §7.4, §8, §9.2](../../specs/model-maverick-v1-spec.md).
