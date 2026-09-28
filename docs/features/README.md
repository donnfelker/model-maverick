# Feature documentation

One document per feature: the problem it solves (the why), how it works (in
prose — no code), and its dependencies on other features. These describe the
system **as built**; the normative contract is
[`specs/model-maverick-v1-spec.md`](../../specs/model-maverick-v1-spec.md),
with measurement methodology in
[`specs/capability-matrix-design.md`](../../specs/capability-matrix-design.md).
Wire-level details live in the [API reference](../api.md); procedures live
in [Operations](../operations.md) and [Onboarding](../onboarding.md).
[`chat-gpt-planning.md`](../chat-gpt-planning.md) is the historical product
planning artifact that preceded the spec.

## The system in one paragraph

Model Maverick answers "which model should run this workload?" It
continuously benchmarks model columns against a private task corpus, folds
graded results into a capability matrix, and serves ranked recommendations
under tenant policy — with an audit trail that can settle any routing
dispute from stored data alone.

## Reading order and dependency map

The features form four layers; each layer consumes the one above it.
Arrows read "feeds into."

    MEASUREMENT      model-catalog ──┐
                     capability-matrix ──▶ benchmark-runner ──▶ grading ──▶ scoring ──▶ calibration
                                                                                │
    DECISION         classification ──▶ policy-engine ──▶ recommendation ◀─────┘
                                                                │
    ACCOUNTABILITY   privacy-capture ◀──── history-audit ◀──────┘        alerts-delivery
                                                                          ▲
    PLATFORM         tenancy-auth   jobs-scheduling ──────────────────────┘
                     internal-provisioning   mcp-surface   observability
                     proxy (external caller — drives recommendation + history like any SDK)

## The documents

### Measurement — how the numbers come to exist

| Doc | What it covers |
|---|---|
| [Capability matrix & corpus](capability-matrix.md) | The cell grid, the private versioned corpus, why leaderboards weren't enough (G7) |
| [Model catalog](model-catalog.md) | Columns as the precise unit, lifecycle, retirement guard, quality transfer, pricing |
| [Benchmark runner](benchmark-runner.md) | Batch execution discipline: fan-out, repeats, retries, determinism |
| [Grading](grading.md) | Deterministic graders + the cross-family judge; failure as absence, not zero |
| [Scoring & aggregation](scoring.md) | Variance/ambiguity penalties, EWMA, regression detection, difficulty ceilings |
| [Predictions & calibration](calibration.md) | Hindsight-proof locked predictions; Brier and overconfidence per capability |

### Decision — how a request becomes an answer

| Doc | What it covers |
|---|---|
| [Classification](classification.md) | Declared vs prompt workloads, the cached fail-soft classifier, the durable fix |
| [Policy engine](policy-engine.md) | Pools, allowlists, constraints; loud errors and write-time validation (G2–G4) |
| [Recommendation & ranking](recommendation.md) | Modes, bands, tie-breaks, honest flags over refusals (G1), check specs |

### Accountability — how decisions stay defensible

| Doc | What it covers |
|---|---|
| [History & audit](history-audit.md) | Full decision traces, outcome reporting, retention (G6) |
| [Privacy & debug capture](privacy-capture.md) | Prompt fingerprints, encrypted time-boxed capture, zero retention (G5) |
| [Alerts & delivery](alerts-delivery.md) | The durable alert ledger and its email/webhook fan-out at insert |

### Platform — what everything runs on

| Doc | What it covers |
|---|---|
| [Tenancy & auth](tenancy-auth.md) | Tenants as config, API keys, rate limits, two listeners; no user system by design |
| [Jobs & scheduling](jobs-scheduling.md) | The Postgres-backed queue, chaining, idempotency, the spend guard (G8) |
| [MCP server](mcp-surface.md) | The agent surface: same services, same auth, same audit |
| [Internal provisioning API](internal-provisioning.md) | The Rails seam: root control plane on the private listener |
| [Observability & health](observability.md) | Structured logs, the two health endpoints, why no metrics stack yet |
| [The trial proxy](proxy.md) | Zero-code-change routing via a base-URL swap: chain execution, BYOK, automatic telemetry |

## The eight guarantees

Cross-cutting invariants referenced throughout as G1–G8 (defined
normatively in spec §1.1): **G1** always a model — flags, never empty
responses ([recommendation](recommendation.md)); **G2** pools are hard
boundaries ([policy engine](policy-engine.md)); **G3** loud caller bugs
([policy engine](policy-engine.md)); **G4** no empty configurations
([policy engine](policy-engine.md), [model catalog](model-catalog.md));
**G5** prompts never stored by default ([privacy](privacy-capture.md));
**G6** every decision auditable ([history](history-audit.md)); **G7** the
corpus is private ([capability matrix](capability-matrix.md),
[grading](grading.md)); **G8** spend is capped
([jobs & scheduling](jobs-scheduling.md)).
