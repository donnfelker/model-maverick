# Model Maverick — OpenRouter Execution Specification

**Status:** draft for review (2026-07-14)
**Plan:** `plans/model-maverick-openrouter-execution-plan.md`
**Depends on:** integration-surfaces Phase 0 (`/v1/telemetry`, alias grammar) and
the SDK v0 shared core (`plans/model-maverick-sdk-v0-plan.md` §2). Engine v1
contract (`model-maverick-v1-spec.md`) and v1.1 §12 referenced, not modified,
except where stated in §2.
**Guarantees:** G13 applies unchanged. G14 applies with the privacy rule adopted
2026-09-28 (v1 spec §1.1): privacy outranks fail-open. No new guarantees.
**Revised 2026-09-28:** per-alias retention status, OpenRouter Zero Data
Retention for zero-retention tenants, and direct execution as the default with
OpenRouter as an opt-in backend (adopted from `plans/master-plan.md` §3.6).

## 1. Overview

OpenRouter becomes a supported **execution backend**: the engine recommends
exactly as today; an SDK configured with `execution: openrouter` dispatches every
chain entry to OpenRouter's OpenAI-compatible API using a per-column model slug
the engine supplies. One customer key replaces N provider keys. MM never touches
the traffic.

**Non-goals:** MM-held OpenRouter credentials; proxying production traffic;
requiring OpenRouter-measured columns; any OpenRouter-specific SDK package.

## 2. Engine changes

### 2.1 Schema — migration `0002_execution_aliases.sql`

```sql
alter table model_columns
  add column execution_aliases jsonb not null default '{}';
```

- Shape: a flat string→string map. This spec defines one key: `"openrouter"`,
  whose value is the OpenRouter model slug (e.g.
  `"anthropic/claude-sonnet-4-6"`). Unknown keys are preserved and ignored.
- The column identity index (provider, model_id, endpoint, settings) is
  unchanged — aliases are metadata, not identity.
- **Per-alias retention** (added 2026-09-28; the Go engine implements the prior
  rule, aliases without retention status). Retention is a property of the
  execution path, so each alias carries its own retention status under the same
  rules as the column (v1 spec §4.5):

  ```sql
  alter table model_columns
    add column execution_alias_retention jsonb not null default '{}';
  ```

  Keyed by the same alias key as `execution_aliases` (e.g. `"openrouter"`);
  each value is `{"retention_mode": "unknown" | "standard" | "zero",
  "retention_verified_at", "retention_valid_until", "retention_source"}`.
  `retention_valid_until` is at most 90 days after `retention_verified_at`;
  expired evidence and a missing key both read as `unknown`. `catalog_sync`
  raises `retention_expiring` 14 days before any alias expiry. The
  `execution_aliases` string map itself is unchanged, so slug readers are
  unaffected.
- `endpoint` gains `'openrouter'` as a legal value (comment + validation only;
  no such columns are created by this spec). A future measured-via-OpenRouter
  column is an ordinary new column with `endpoint: "openrouter"`, subject to
  v1.1 §12 promotion rules.

### 2.2 Internal API

- `POST /internal/v1/columns` accepts optional `execution_aliases` (object,
  string values, ≤10 keys, values non-empty ≤200 chars; `422 invalid_request`
  otherwise). Endpoint validation set becomes
  `direct | vertex | bedrock | azure | openrouter`.
- `PUT /internal/v1/columns/{id}` accepts `execution_aliases`; when present the
  stored map is **replaced whole** (consistent with the endpoint's partial-update
  semantics at field granularity). Edits log a `catalog_changes` entry.
- Both endpoints accept `execution_alias_retention` under the same
  replace-whole rule; every key must also exist in `execution_aliases`, and the
  90-day rule is validated (`422 invalid_request` otherwise).
- G4 parity note: aliases carry no tenant-facing config semantics; no
  tenant-side write surface exists or is added.

### 2.3 Public surfaces

- `GET /v1/models`: each entry gains `"execution_aliases": {...}` (empty object
  when unset) and `"execution_alias_retention": {...}` (effective modes after
  the expiry rule).
- `POST /v1/recommend`: each `recommendations[]` entry gains
  `"execution_aliases": {...}` and `"execution_alias_retention": {...}`, beside
  the entry's own `retention_mode` and the response's `zero_retention` flag
  (v1 spec §9.2). SDKs therefore need **zero extra calls** to execute via
  OpenRouter.
- MCP tools inherit both automatically (same response objects, spec §12 of v1).
- `matrix_version` semantics unchanged; alias edits do not bump it.

### 2.4 Telemetry

The `/v1/telemetry` item (integration-surfaces §1) gains:

```
"endpoint_used": "direct" | "openrouter" | "vertex" | "bedrock" | "azure"
```

Optional; absent means `direct`. Stored on the derived outcome row. v1.1 §7
blending semantics are unchanged by this field in v0; it exists so §12
endpoint-drift analysis and any future endpoint-aware blending have real data.

## 3. SDK core changes (all four languages)

### 3.1 Configuration

- `execution: "direct" | "openrouter"` — default `direct`. Env:
  `MODELMAVERICK_EXECUTION`. Direct execution is the default everywhere;
  OpenRouter is an opt-in backend, one of several the alias mechanism can carry
  (Vercel AI Gateway and Bedrock are the next candidates).
- OpenRouter credentials come from the ecosystem-standard `OPENROUTER_API_KEY`;
  upstream base URL default `https://openrouter.ai/api/v1`, overridable via
  `MODELMAVERICK_OPENROUTER_BASE_URL` (self-hosted gateways impersonating the
  schema).
- These are core-spec additions (SDK v0 plan §2.1); all four SDKs implement
  identically.

### 3.2 Chain executor — openrouter mode

- Every chain entry dispatches to the single OpenRouter upstream; the request's
  `model` is the entry's `execution_aliases.openrouter` value. Wire format is
  OpenAI-compatible in and out — no translation, no per-provider dispatch.
- Advance/stop rules are unchanged from the core spec (advance on 429/5xx/
  timeout/connect error, respect `Retry-After`, never on semantic 4xx).
- **Alias-less entries are skipped** with a debug log (they cannot be executed
  on this backend). If no entry in the chain carries an `openrouter` alias, the
  executor reports chain-exhaustion with error class
  `no_executable_candidates` — this is a configuration gap (catalog missing
  aliases), not a fail-open event.
- Fail-open ladder (G14) still applies to MM unavailability; the cached
  last-good chain caches entries **with** their aliases and alias retention.
- **Zero-retention tenants** (added 2026-09-28; the Go engine and proxy
  implement the prior rule, no retention handling). When the recommend response
  carries `zero_retention: true`, the executor:
  1. sends OpenRouter's per-request Zero Data Retention routing option
     (`provider.zdr: true` in the request body) on every attempt;
  2. skips any entry whose `execution_alias_retention.openrouter` is not an
     unexpired `zero`;
  3. if nothing remains, fails closed with error class `no_compliant_columns`
     (the hosted proxy answers `409 no_compliant_columns`). It never falls back
     to a direct provider key, and the G14 privacy rule governs the fail-open
     ladder (cached entries stamped `zero` only).

### 3.3 Telemetry

Openrouter-mode executions stamp `endpoint_used: "openrouter"` on every item.
Direct mode omits the field. Opt-out behavior (G13) unchanged.

## 4. Trial proxy OpenRouter backend (opt-in; details decided at milestone N5)

The trial proxy executes per-provider direct by default. OpenRouter is an
opt-in backend, not the default: OpenRouter is being acquired by Stripe and
runs a free Auto Router that overlaps MM, so defaulting evaluators through it
sends them to a competitor and adds a single-vendor dependency. On the hosted
proxy, a caller who supplies an OpenRouter key in the provider-credential slot
(`Authorization`; the tenant's Maverick key travels in `X-Maverick-API-Key`,
integration-surfaces plan §4) has every chain entry executed through
OpenRouter; alias-less entries are skipped. The hosted proxy reads no provider
env keys. Zero-retention tenants get the §3.2 rules: the Zero Data Retention
option on every attempt, non-`zero` aliases skipped, `409 no_compliant_columns`
when nothing remains. Custody stays transit-only. All other Track 3 fencing
(rate limits, no production SLA, tenant telemetry opt-out honored) is
unchanged. (changed 2026-09-28: the prior draft made the proxy OpenRouter-only
with an `X-Maverick-OpenRouter-Key` header)

## 5. Measurement fidelity

Recommendations executed via OpenRouter run against numbers measured on the
column's own endpoint (typically `direct`). This spec's honesty posture:

- `measured` semantics are **unchanged** — they describe the recommended
  column, and the response's `endpoint` field states what was measured.
- The gap is made observable, not hidden: `endpoint_used` in telemetry lets the
  operator see direct-measured/openrouter-executed volume per column.
- The structural fix is pre-existing: create `endpoint: "openrouter"` columns
  and let v1.1 §12 promotion govern when they earn their own quality cells.
  Out of scope for v0.

## 6. Conformance harness additions (SDK v0 plan §2.3)

10. Openrouter mode: three-entry chain, first entry 500s → second entry
    succeeds; mock asserts both requests hit the single upstream with the
    correct slugs and `endpoint_used: "openrouter"` appears in telemetry.
11. Alias-less entry in the chain is skipped; alias-less entire chain →
    `no_executable_candidates` surfaced, no upstream request.
12. Direct-mode requests carry no `endpoint_used`; opt-out still yields zero
    telemetry bytes in openrouter mode.

Scenario 10 gains a zero-retention case (added 2026-09-28): a recommend
response with `zero_retention: true` and an OpenRouter key; the mock asserts
the Zero Data Retention option on every upstream request, a non-`zero` alias is
skipped, and an all-non-`zero` chain yields `no_compliant_columns` with no
upstream request and no direct-key fallback. The proxy's own test suite runs
the same case against the hosted proxy.

## 7. Testing (engine)

- Migration + store round-trip for `execution_aliases` (empty, populated,
  replace-whole on PUT, ≤10-key/length validation).
- API: `/v1/models` and recommend entries carry aliases and alias retention;
  internal create/update accept/reject per §2.2 (including the 90-day rule and
  unknown alias keys); `openrouter` accepted as endpoint value; expired alias
  evidence reads as `unknown`.
- Telemetry ingestion maps `endpoint_used` onto outcome rows; absent → direct.
- Existing G4/retirement-guard/matrix-version tests unaffected (aliases are
  metadata).

## 8. Open decisions (mirrors the plan)

1. Alias shape: generic map (recommended, spec as written) vs single column.
2. Trial proxy execution: per-provider direct by default (recommended,
   2026-09-28) with OpenRouter as one opt-in backend among several, vs
   OpenRouter-backed only (§4).
3. Alias population: operator-only at v0 (recommended, spec as written) vs a
   sync job against OpenRouter's model list.
