# Policy engine

*Tenant governance that cannot be silently violated and cannot silently
strand a workload: pools, allowlists, and constraints with loud errors and
write-time validation.*

## The problem it solves

Organizations adopting model routing need governance: an approved model
list (compliance may forbid certain providers or endpoints), cost and
latency limits, and different rules for different workloads. Governance
mechanisms have two classic failure modes. **Silent widening** — the engine
"helpfully" recommends outside the approved set when the approved set looks
bad — destroys the compliance story. **Silent emptying** — a config change
or a model retirement leaves a pool with nothing usable, discovered only
when production requests start failing — destroys the reliability story.
The policy engine is built so neither can happen quietly.

## How it works

Filtering runs in a fixed order, and every step records its exclusions with
a machine-readable reason into the [decision trace](history-audit.md):

1. **Pool restriction (G2).** The request's named pool, else the tenant's
   default. Pools are *hard* boundaries: a request can narrow its candidate
   set but never widen beyond its pool — escalation chains are
   pool-constrained by construction.
2. **Allowlist (G3).** Entries resolve against the *system* catalog first:
   an unknown entry is a hard error naming it (likely a typo — the caller
   must know), and a known entry outside the caller's pool is a different
   hard error naming it (a boundary violation — the caller must know that
   too). No silent dropping, no silent widening.
3. **Standing constraints.** The tenant's stored cost/latency limits filter
   hard against the workload cell's aggregates — safe only because writes
   are validated (below). Aggregates drift between writes; if drift has
   emptied the set by request time, the least-violating columns are
   retained and flagged, and a critical alert fires: **G1 outranks the
   filter.**
4. **Request preferences deprioritize, never drop.** Per-request
   constraints partition candidates into satisfying and violating;
   violators rank after satisfiers everywhere. If the final pick violates a
   preference, the response says so with the named violations — a real
   recommendation with honest flags beats an empty list.
5. **Rank** (see [Recommendation](recommendation.md)).

**Write-time validation (G4).** Pool and constraint writes are validated
together before commit: for every pool as it would exist after the write,
applying the constraints as they would exist after the write against
current aggregates must leave at least one usable column — otherwise the
write is rejected wholesale, naming the pool and the constraint that
empties it. The same simulation backs the
[retirement guard](model-catalog.md) and runs nightly as a drift check
(aggregates move; a pool can drift toward empty), raising a warning
[alert](alerts-delivery.md) before it becomes a request-time incident.

## Dependencies

- Filters columns from the [Model catalog](model-catalog.md) using cell aggregates from [Scoring](scoring.md).
- Sits between [Classification](classification.md) (workload in) and [Recommendation](recommendation.md) (ranking out); exclusions land in [History & audit](history-audit.md).
- Emptiness signals raise [Alerts](alerts-delivery.md); config is per-tenant ([Tenancy & auth](tenancy-auth.md)) and writable both tenant-side and via the [Internal provisioning API](internal-provisioning.md) with identical validation.
- Config endpoints and error codes (the four contractual policy errors): [API reference](../api.md).
- Normative contract: [spec §7](../../specs/model-maverick-v1-spec.md).
