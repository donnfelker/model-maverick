# Model catalog: columns and lifecycle

*The precise unit of benchmarking and recommendation — a (provider, model,
endpoint, settings) tuple — plus the lifecycle machinery that lets the
catalog churn without breaking anyone.*

## The problem it solves

"A model" is not a precise unit. The same model behaves differently with
thinking enabled vs disabled, and costs/performs differently on a direct API
vs Vertex vs Bedrock. Recommending "claude-sonnet-4-6" is ambiguous in
exactly the ways that matter for cost and latency. Meanwhile providers churn
constantly — new releases, price changes, deprecations, sunsets — and a
model disappearing must never silently strand a tenant whose pool depended
on it.

## How it works

- **The column.** Everything is keyed on a *column*: (provider, model,
  endpoint, settings). Thinking on vs off is two columns, measured
  separately. Uniqueness is enforced on the full tuple.
- **Lifecycle.** `cold_start → active → deprecated → retired`.
  - *cold_start:* a new column runs the **full** corpus nightly (not just
    smoke) through a dense-run window, so it accumulates data fast. Its cells
    report unmeasured until they have enough nights of observations. Cold
    columns are recommendable only when nothing measured survives a request's
    pool/allowlist — and then the response says so honestly
    (see [Recommendation](recommendation.md), guarantee G1).
  - *deprecated:* still benchmarked, still recommendable; responses carry the
    provider's sunset date so callers can plan.
  - *retired:* excluded from ranking and no longer benchmarked.
- **The retirement guard (G4).** Before any column transitions to retired,
  the engine simulates every tenant pool containing it as if the column were
  already gone. Any pool that would drop to zero usable columns **blocks the
  transition** and raises a critical `pool_would_be_empty` alert naming the
  affected tenants (delivered per [Alerts](alerts-delivery.md)). The
  transition proceeds only after the pools are amended. The same simulation
  backs config-write validation in the [policy engine](policy-engine.md).
- **Quality transfer.** v1 measures quality on one primary endpoint per
  model. Secondary-endpoint columns (the Vertex variant, say) borrow
  quality/pass-rate cells from their primary column but keep their **own**
  latency and cost — fed by hourly latency probes and their own pricing.
  Rationale: quality is a property of the model; latency and price are
  properties of the path to it.
- **Pricing.** Each column snapshots list price per million tokens. Run
  costs are computed at execution time from the then-current snapshot and
  **never restated** — historical spend stays historical even when prices
  change. In the as-built v1, pricing updates arrive through the column
  admin endpoint (no external price feed is wired).
- **Execution aliases.** A column may carry a flat map of execution-backend
  model slugs (v1 defines one key, `"openrouter"`) alongside its
  provider/model/endpoint identity. This lets an SDK or the trial proxy
  dispatch the engine's pick through an alternate execution path — one
  customer key routing through OpenRouter instead of N provider keys —
  without the engine ever touching that traffic or the catalog needing a
  parallel set of OpenRouter-specific columns. The map rides along on every
  `GET /v1/models` and `POST /v1/recommend` entry so no extra call is needed
  to dispatch a pick, and is operator-set only, same as pricing.
- The catalog is **data, not code**: columns are created and edited through
  the [internal provisioning API](internal-provisioning.md), not migrations.

## Dependencies

- Defines the measurement unit for the [Capability matrix](capability-matrix.md).
- Exercised nightly by the [Benchmark runner](benchmark-runner.md); statuses and drift checked by daily jobs ([Jobs & scheduling](jobs-scheduling.md)).
- Referenced by tenant pools in the [Policy engine](policy-engine.md); guarded transitions raise [Alerts](alerts-delivery.md).
- Administered through the [Internal provisioning API](internal-provisioning.md); listed tenant-side via the models endpoint in the [API reference](../api.md).
- Operator runbook for lifecycle transitions: [Operations — column lifecycle](../operations.md).
- Normative contract: [spec §4](../../specs/model-maverick-v1-spec.md).
