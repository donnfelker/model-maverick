# Model Maverick — OpenRouter Execution Plan

**Status:** draft for review (2026-07-14)
**Parents:** `model-maverick-integration-surfaces-plan.md` (Track 1/3),
`model-maverick-sdk-v0-plan.md` (shared core).
**Normative spec:** `specs/model-maverick-openrouter-execution-spec.md`.

## Why

The SDK plan quietly assumes the customer holds API keys for every provider in
their pool. Many hold exactly one. OpenRouter is one key, one OpenAI-compatible
schema, ~300 models — so supporting it as an **execution backend** kills the
N-provider-keys adoption objection with no new SDK: MM stays the brain,
OpenRouter is rented muscle. The engine already models the honesty problem this
creates (same model, different endpoint, different numbers): `model_columns.endpoint`
exists in v1 and v1.1 §12 (per-endpoint quality promotion) exists for exactly
this case.

## Scope

1. **Engine:** columns gain an `execution_aliases` map (key `openrouter` defined
   now, future aggregators later); exposed on `/v1/models` and on every
   recommend chain entry so SDKs need zero extra calls. `openrouter` becomes a
   legal `endpoint` value for future measured-via-OpenRouter columns.
2. **SDK core (all four languages):** config `execution: direct | openrouter`.
   In openrouter mode the chain executor sends every entry to one upstream with
   the entry's alias as the model slug — strictly *simpler* than direct mode.
   Telemetry records which endpoint actually executed (`endpoint_used`).
3. **Proxy variant:** OpenRouter-backed execution, so BYOK shrinks to a single
   key. Evaluated as the preferred Track 3 shape at N5.

## Non-goals

- MM never holds OpenRouter keys, resells OpenRouter traffic, or proxies
  production calls (unchanged Track 3 fencing).
- No OpenRouter-measured benchmark columns required at v0 — the alias executes
  against direct-endpoint numbers with the fidelity caveat documented; measured
  OpenRouter columns are a §12 follow-on, not a blocker.
- No OpenRouter-specific SDK. This is a config value in the four planned SDKs.

## Risks, named

- **Fidelity:** we recommend from direct-endpoint measurements, the customer
  executes via OpenRouter (+1 hop latency; open-weight slugs fan out to varying
  hosts/quantizations). Mitigation: `endpoint_used` in telemetry makes the gap
  observable; §12 promotion is the structural fix when traffic justifies it.
- **Competitive adjacency:** OpenRouter ships its own `auto` router. Assessed
  low-risk — theirs is generic, ours is capability×difficulty benchmarked and
  tenant-tuned; they earn margin on the traffic we send.

## Milestones (mapped into the existing N-table, no new track)

| Where | Work |
|---|---|
| N1 (engine prereqs) | `execution_aliases` migration + internal/public API exposure; `openrouter` accepted as endpoint value; telemetry item gains `endpoint_used` |
| N2 (core spec + harness) | `execution` mode in the shared core spec; conformance scenarios 10–12 |
| N3–N6 (each SDK) | openrouter mode ships with each SDK's core — no per-language extra |
| N5 (proxy) | decide OpenRouter-backed variant vs per-provider BYOK |

## Open decisions

1. **Alias shape** — generic `execution_aliases` map (recommended; Bedrock/
   Azure/Vertex aliases become the same mechanism later) vs a single
   `openrouter_slug` column.
2. **Proxy default** — OpenRouter-backed (recommended: one BYOK key,
   one upstream, no per-provider compat matrix) vs per-provider compat
   endpoints as originally planned.
3. **Alias population** — operator-maintained via internal API only (v0,
   recommended) vs a sync job against OpenRouter's public model list later.
