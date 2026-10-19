# Model Maverick: API Reference

This document describes the HTTP surface that the Rust engine serves (spec §9;
reference Part 5). Where the Rust engine differs from the Go engine it
replaces, the text names the row in `parity/DIVERGENCES.md` (`D<n>`). Two
listeners (spec §2.1):

| Listener | Default address | Serves |
|---|---|---|
| Public | `:8080` (`MAVERICK_PUBLIC_ADDR`) | `/up`, `/v1/*`, `/mcp` |
| Internal | `:8081` (`MAVERICK_INTERNAL_ADDR`) | `/health/detail`, `/internal/v1/*` |

All request and response bodies are JSON (`Content-Type: application/json`).
Request bodies are capped at 2 MiB (2,097,152 bytes; `413 payload_too_large`).
Request bodies and query strings are decoded strictly: unknown JSON fields,
trailing content, and unknown query parameters are `422 invalid_request`.
Timestamps in responses are RFC 3339 in UTC, in Go's RFC3339Nano form: a `Z`
suffix, and a fraction with its trailing zeros trimmed (none on a whole
second).

`POST /v1/recommend`, `GET /v1/policies`, `GET /v1/models`, and the history
lists carry `matrix_version`: the active corpus version, or `""` when no corpus
is active.

---

## Authentication and rate limiting (public listener)

- **API keys**: `Authorization: Bearer mk_live_...`. Keys are issued through
  the internal API (`POST /internal/v1/tenants/{id}/api-keys`), shown once,
  and stored as a SHA-256 hash. A missing, malformed, or revoked key answers
  `401 unauthorized`; a valid key of a suspended tenant answers
  `403 tenant_suspended`.
- **Rate limit**: fixed window per key, default 300 requests/minute
  (`MAVERICK_RATELIMIT_RPM`). Every authenticated response carries:

  ```
  X-RateLimit-Limit: 300
  X-RateLimit-Remaining: 298
  X-RateLimit-Reset: 1783472580        (unix seconds, window end)
  ```

  Exceeding the window yields `429 rate_limited` with a `Retry-After` header
  (seconds). The limiter is in memory and per process by design for v1.

## Error envelope and codes

Every error on both listeners uses one envelope; `details` is always present
(`{}` when empty):

```json
{
  "error": {
    "code": "model_not_in_pool",
    "message": "allowed_models entry 'gpt-5.5' is not in pool 'vertex-only'",
    "details": { "entry": "gpt-5.5", "pool": "vertex-only" }
  }
}
```

The code list is closed. No response carries a code outside this table:

| HTTP | `code` | Meaning |
|---|---|---|
| 401 | `unauthorized` | Missing, invalid, or revoked key (or internal token) |
| 403 | `tenant_suspended` | Valid key, suspended tenant |
| 404 | `not_found` | Resource absent **or** owned by another tenant (no existence oracle) |
| 409 | `zero_retention_conflict` | Capture window requested by a zero-retention tenant |
| 409 | `no_compliant_columns` | Zero-retention tenant, and no pool column has valid zero-retention evidence (D1, D17) |
| 410 | `capture_expired` | Replay or read of an expired or hard-deleted capture |
| 413 | `payload_too_large` | Body over 2 MiB |
| 422 | `invalid_request` | Malformed body, unknown fields or params, bad enums, bad values |
| 422 | `unknown_model` | `allowed_models` entry not in the system catalog (names the entry) |
| 422 | `model_not_in_pool` | `allowed_models` entry outside the caller's pool (names the entry) |
| 422 | `unknown_pool` | Pool name not configured for the tenant |
| 422 | `pool_would_be_empty` | Config write or retirement would empty a pool (names the pool) |
| 422 | `zero_retention_requires_declared` | Zero-retention tenant sent the prompt form, and no zero-retention classifier column is configured (D2) |
| 422 | `grader_unsupported` | Corpus load: reserved grader type in v1 (no HTTP route returns it in v1) |
| 429 | `rate_limited` | Per-key limit; `Retry-After` set |
| 500 | `internal_error` | Bug; logged with the request id |

There is no 405 entry: a wrong method on a known path answers exactly like an
unknown path (`404 not_found`). Every authenticated public route, `/mcp`
included, can answer 401, 403, 413, and 429. Every route that reads the store
can answer 500. Internal routes have no 403 and no 429.
`crates/maverick-api/tests/CONTRACT.md` lists every (endpoint, status, code)
pair the API returns, with the test that pins it.

The four 422 policy codes (`unknown_model`, `model_not_in_pool`,
`unknown_pool`, `pool_would_be_empty`) are contractual (G3): callers are
expected to branch on them. So are the two zero-retention codes
(`no_compliant_columns`, `zero_retention_requires_declared`): they are
terminal, and SDK cores never fail open past them (D17).

## Pagination contract

All `GET /v1/history/*` list endpoints paginate identically:

- `?limit=`: default 50, maximum 200; out of range is `422 invalid_request`.
- `?cursor=`: an opaque token from a previous page. A cursor from another
  endpoint is refused.
- Responses carry `next_cursor`: the token for the next page, or `null` on
  the last page.
- `?since=` filters accept RFC 3339 timestamps or bare `YYYY-MM-DD` dates
  (midnight UTC).

---

## POST /v1/recommend

Recommends model columns for a workload. `workload` is required and carries
**exactly one** of two forms:

- **Declared form**: `{"capability": "...", "difficulty": 1-5, "domain_tags": [...]}`.
  `capability` is one of the ten v1 capabilities (see `GET /v1/policies`);
  `difficulty` defaults to 3 (the response then carries
  `classification.difficulty_source: "default"`); `domain_tags` is optional.
  Nothing leaves the engine: no classifier call.
- **Prompt form**: `{"prompt": "...", "projection_version": "p1"}`. No
  `difficulty` or `domain_tags` alongside. `prompt` is non-empty and at most
  500,000 characters. `projection_version` is optional; an MM component that
  built the prompt from a chat request names its projection version, which is
  part of the classification cache key (D5). The prompt is classified (spec
  §8.3), fingerprinted (`prompt_hash`), and never stored.

The classifier has one 1.5 s deadline that covers the call, one retry, and the
backoff (D3). Each tenant has a daily classification quota, and a global daily
classifier spend cap applies (D4). A zero-retention tenant's prompt goes only
to the zero-retention classifier column (`MAVERICK_CLASSIFIER_ZERO_RETENTION_COLUMN`);
when none is configured, the prompt form answers
`422 zero_retention_requires_declared` (D2).

Full request (all fields except `workload` optional):

```json
{
  "workload": { "prompt": "Extract the parties, dates, and obligations from this contract: ..." },
  "pool": "default",
  "allowed_models": ["claude-sonnet-4-6", "claude-haiku-4-5"],
  "mode": "balanced",
  "constraints": { "max_latency_ms": 2000, "max_cost_per_run": 0.01 }
}
```

- `pool` defaults to the tenant default pool.
- `mode` ∈ `quality | balanced | cost`, default `balanced`.
- `allowed_models` entries are **model names** (request-time ergonomics;
  stored pool config references `column_id`s).
- `constraints` are per-request preferences. Standing tenant constraints
  (`/v1/config/constraints`) filter candidates hard; per-request preferences
  only reorder and flag.

Response `200` (measured stack):

```json
{
  "classification": { "capability": "structured_extraction", "difficulty": 3,
    "domain_tags": ["legal"], "high_stakes": true, "confidence": 0.91, "method": "classified" },
  "recommendations": [
    { "provider": "anthropic", "model": "claude-sonnet-4-6", "endpoint": "direct",
      "column_id": "col_01J...",
      "execution_aliases": { "openrouter": "anthropic/claude-sonnet-4-6" },
      "execution_alias_retention": { "openrouter": "zero" },
      "settings": { "thinking": true },
      "param_constraints": { "reasoning_effort": { "allowed": ["low", "high"],
                                                   "order": ["low", "medium", "high"] } },
      "retention_mode": "zero",
      "score": 94.1, "clears_difficulty": true, "low_availability": false,
      "reason": "cheapest column clearing tier 3 structured_extraction at your quality bar in this pool" },
    { "provider": "google", "model": "gemini-3.1-pro", "endpoint": "direct",
      "column_id": "col_01J...", "execution_aliases": {}, "execution_alias_retention": {},
      "settings": {}, "param_constraints": {}, "retention_mode": "standard",
      "score": 92.8, "clears_difficulty": true, "low_availability": false,
      "reason": "cross-family alternative" }
  ],
  "pool": "default",
  "mode": "balanced",
  "check_spec": { "type": "json_schema", "ref": "checks/extraction/v1" },
  "constraints_met": true,
  "violations": [],
  "prompt_hash": "hmac_sha256:9f2c...",
  "recommendation_id": "rec_01J...",
  "matrix_version": "2026-07-07",
  "measured": true,
  "zero_retention": false,
  "telemetry": true
}
```

Field notes (checked against the implementation):

- `classification.method` ∈ `declared | classified | cached |
  fallback_default`. When the classifier is unavailable (no provider key, the
  classifier column is not in the catalog, the deadline passed, or the budget
  is spent), prompt-form requests still answer `200` with the fallback
  classification:
  `{"capability": "multi_step_reasoning", "difficulty": 3, "domain_tags": [],
  "high_stakes": false, "confidence": 0, "method": "fallback_default"}` (G1).
  A classifier column that is missing at boot is resolved again in the
  background, so the engine recovers without a restart (D41).
- `classification.difficulty_source: "default"` appears only when a declared
  request omitted `difficulty`; `classification.truncated: true` appears only
  when an oversize prompt was classified on its head and tail.
- `recommendations` is the whole pool-constrained fallback chain, best first
  (G2). `reason` strings are informative, not contractual; the flags are.
  Each entry's keys, in order:
  - `provider`, `model`, `endpoint`, `column_id`.
  - `execution_aliases`: the column's execution-backend alias map (for
    example `{"openrouter": "<slug>"}`), `{}` when it has none. It rides along
    so an SDK configured for an alternate backend needs no extra call.
  - `execution_alias_retention`: each alias key with its effective retention
    mode (`zero`, `standard`, or `unknown`; expired evidence reads as
    `unknown`) (D15). OpenRouter execution for a zero-retention tenant uses
    only aliases whose mode is `zero`.
  - `settings`: the column's settings (reasoning effort, thinking mode). MM
    components apply them unless the caller opts out (D9, D14).
  - `param_constraints`: the model's parameter constraints from the catalog,
    `{}` when none. MM components normalize caller values with them (D29).
  - `retention_mode`: the column's effective retention mode (D1, D9).
  - `score`: the absolute displayed quality (0 to 100, one decimal). It is
    `null` when the workload cell has no quality EWMA yet (cold start), and
    `measured` is then `false`. Every recommendation is still produced (G1);
    see `docs/onboarding.md` for a fully cold response.
  - `clears_difficulty`, and `low_availability`: `true` when the column's
    availability over the trailing 7 nights is below the floor
    (`MAVERICK_RANKING_AVAILABILITY_FLOOR`, default 0.9). Such columns rank
    after all others (D8).
  - `reason`.
- `prompt_hash` is present only on prompt-form requests:
  `"hmac_sha256:" + hex(HMAC-SHA256(tenant key, prompt bytes))`: exact
  bytes, per-tenant key, exact match only.
- `violations` entries: `{"constraint": "max_latency_ms", "limit": 2000,
  "estimated": 3400}`; `constraints_met` is `false` when the pick violates a
  per-request preference.
- `zero_retention` is always present: `true` when the tenant has the
  zero-retention constraint. SDK cores then switch to the declared form and
  treat the tenant as zero-retention (D9, G14). For such a tenant only columns
  with valid zero-retention evidence are candidates; when none remain, the
  request answers `409 no_compliant_columns` and nothing is recommended (D1).
- `telemetry` is always present: `false` when the tenant opted out of
  telemetry, so MM components send none (D39).
- `classification_quota_exceeded: true` appears only when the classifier
  budget stopped the classifier, as the last key (D4).
- Every call (REST or MCP) persists a full decision trace readable at
  `GET /v1/history/recommendations/{id}`.

Errors: `409 no_compliant_columns`, `422 invalid_request | unknown_model |
model_not_in_pool | unknown_pool | zero_retention_requires_declared`, `413`,
`429`.

## GET /v1/policies

`GET /v1/policies?capability=structured_extraction&pool=default`: both
parameters are optional. An absent `capability` returns all ten; an absent
`pool` uses the tenant default.

```json
{
  "pool": "default",
  "quality_bar": 0.8,
  "buffer": 0.05,
  "policies": [{
    "capability": "structured_extraction",
    "bands": [
      { "band": "easy", "tiers": [1, 2],
        "column": { "provider": "anthropic", "model": "claude-sonnet-4-6",
                    "endpoint": "direct", "column_id": "col_01K..." },
        "ceiling": 3, "pass_rate": 0.94, "clears_band": true },
      { "band": "medium", "tiers": [3], "column": { "...": "..." },
        "ceiling": 4, "pass_rate": 0.88, "clears_band": true },
      { "band": "hard", "tiers": [4, 5], "column": { "...": "..." },
        "ceiling": 5, "pass_rate": 0.83, "clears_band": true }
    ]
  }],
  "matrix_version": "2026-07-07"
}
```

On a cold stack every band still resolves a column but carries `ceiling: 0`,
`pass_rate: null`, `clears_band: false`. `quality_bar` reflects the tenant's
weights row when one capability is asked for (default 0.80); for the full
table it is 0.80, though each capability's ceilings use that capability's bar.
`buffer` is the §6.6 ceiling buffer (`MAVERICK_CEILING_BUFFER`).

The candidates are the pool's known, non-retired columns; no allowlist,
standing cost or latency constraint, or preference applies. For a
zero-retention tenant, only columns with valid zero-retention evidence are
candidates, as on `/v1/recommend`; a band with none has `"column": null` and
`"clears_band": false` (D42).

Errors: `422 invalid_request` (unknown capability or query param),
`422 unknown_pool`.

## GET /v1/models

The column catalog visible to the tenant: every non-retired column
(`cold_start`, `active`, `deprecated`), by column id. Query parameters are
ignored. There are no scores here; scores are per cell and live in the
policies and history endpoints.

```json
{
  "models": [
    { "column_id": "col_01K...", "provider": "anthropic",
      "model_id": "claude-sonnet-4-6", "endpoint": "direct", "settings": {},
      "execution_aliases": { "openrouter": "anthropic/claude-sonnet-4-6" },
      "status": "active",
      "pricing": { "input_per_mtok": 3.0, "output_per_mtok": 15.0 },
      "deprecation_date": null, "measured": false,
      "quality_source_column_id": null,
      "retention_mode": "zero",
      "retention_verified_at": "2026-09-01T10:00:00Z",
      "retention_valid_until": "2026-11-30T10:00:00Z",
      "retention_source": "https://example.com/zdr-terms",
      "execution_alias_retention": { "openrouter": "zero" },
      "param_constraints": {} }
  ],
  "matrix_version": "2026-07-07"
}
```

- The first eleven keys are the Go engine's, in its order. The retention
  fields, `execution_alias_retention`, and `param_constraints` follow.
- `measured` is true once the column has any measured aggregate cell.
- `quality_source_column_id` names the column this one inherits quality cells
  from (§4.2; for example a Vertex column that uses its direct sibling's
  quality).
- `execution_aliases` is a flat string-to-string map of execution-backend
  model slugs, `{}` when unset. One key is defined, `"openrouter"` (the
  OpenRouter model slug); unknown keys are kept and ignored. The catalog is
  operator-populated only (`POST`/`PUT /internal/v1/columns`).
- `retention_mode` is the effective mode: `zero` or `standard` only while
  `retention_valid_until` is in the future, else `unknown`. The three evidence
  fields are shown as stored (D1).
- `execution_alias_retention` gives each `execution_aliases` key its effective
  mode under the same rule; a key without evidence reads `unknown` (D15).
- `param_constraints` is the model's stored parameter constraints, `{}` when
  none (D29).

## GET /v1/checks/{ref}

Check-spec refs contain slashes (`checks/extraction/v1`), so the whole
remaining path is the ref:

```
GET /v1/checks/extraction/v1
```

```json
{
  "ref": "checks/extraction/v1",
  "capability": "structured_extraction",
  "type": "json_schema",
  "spec": { "schema": { "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": ["object", "array"] } }
}
```

The registry is static, embedded in the binary, with one spec per capability
(the same `check_spec.ref` values `/v1/recommend` returns). Unknown refs are
`404 not_found`.

## POST /v1/telemetry

**This is SDK and plugin plumbing, not a customer obligation (G13).**
MM-authored components (the client SDKs, gateway plugins, and the proxy) call it automatically as a side effect of routing a request. No public
surface asks a caller to report what happened by hand. A component with
telemetry disabled never calls it: zero calls, no fallback. The consequence is
that the tenant's own outcome blending (v1.1 §7) has no data, and its rankings
stay benchmark-driven. `POST /v1/outcomes`, the retired caller-reported
outcome endpoint, does not exist.

Body: `{"items": [...]}`. `items` is required: an array of at most 500
entries. These are the *only* 4xx cases of this endpoint: an empty body,
malformed JSON, a non-object body, trailing content, missing or null `items`,
a non-array `items`, or more than 500 entries all answer `422 invalid_request`.
Everything about individual items is best effort and never fails the batch.

Item shape:

```json
{ "recommendation_id": "rec_01J...", "model_used": "claude-sonnet-4-6",
  "status": 200, "latency_ms": 812, "tokens_in": 340, "tokens_out": 120,
  "finish_reason": "stop", "error_class": null, "check_pass": true,
  "endpoint_used": "direct", "settings_applied": true }
```

- `recommendation_id`, `model_used`, and `status` are required. `status` is
  the provider's HTTP status; `0` means a transport failure (a connect error or
  a timeout before any response arrived).
- `latency_ms`, `tokens_in`, `tokens_out`: optional non-negative integers.
- `finish_reason`: optional string.
- `error_class`: optional, ∈ `timeout | connect_error | provider_error |
  other`.
- `check_pass`: optional boolean, and three-valued: `true`, `false`, or absent
  when no check ran. Only `true` or `false` is quality evidence (D16).
- `endpoint_used`: optional, ∈ `direct | vertex | bedrock | azure |
  openrouter`; absent means `direct`.
- `settings_applied`: optional boolean, whether the component applied the
  entry's `settings` (D14).
- Unknown fields are ignored.

**Per item, best effort:** an item that is missing or mistypes a required
field, fails an optional field's constraint, names a `recommendation_id` that
does not exist, or names another tenant's recommendation is **dropped and
counted**. Dropping is indistinguishable from unknown, so there is no
cross-tenant existence oracle. It is never a per-item error and never fails
the batch.

`200 {"accepted": N, "dropped": M}`. An empty `items` array is a valid
no-op: `{"accepted": 0, "dropped": 0}`.

**Tenant opt-out (D39):** when the tenant's `telemetry_opt_out` constraint is
set, every item is dropped and counted, and no outcome row is written. The
response has the same shape. Recommend responses carry `"telemetry": false`
for such a tenant, so MM components send nothing for it in the first place.

**Outcome derivation** (server side; the first matching rule wins):

1. `error_class == "timeout"` → `latency_exceeded`
2. `error_class ∈ {"connect_error","provider_error"}`, or `status == 0`, or
   `status == 429`, or `status >= 500` → `provider_error`
3. 2xx `status` and `check_pass == false` → `parse_failure`
4. 2xx `status` and `check_pass == true` → `success`
5. 2xx `status` and no `check_pass` → `completed_unchecked`
6. anything else (for example a semantic 4xx) → `other`

`completed_unchecked` counts toward availability only and never enters outcome
blending. The Go engine derived `success` for any 2xx without
`check_pass: false` (D16). `quality_insufficient` remains a legal value in the
`outcomes` table (historic rows keep it), but no telemetry item produces it.

Stored row (`outcomes`): `id` `out_<ulid>`, `recommendation_id`,
`model_used`, the derived `outcome`, `endpoint_used`, `escalated_to: null` (no
telemetry equivalent), and `details`: a jsonb object with exactly the facts the
item supplied: `status` always, plus `latency_ms`, `tokens_in`, `tokens_out`,
`finish_reason`, `error_class`, `check_pass`, and `settings_applied` only when
present on the item. Several items per recommendation are allowed and
appended (retries, chain escalations).

## History

Five read endpoints, all paginated (see the pagination contract above).
Parameters are checked in this order: an unknown name, `limit`, `difficulty`
(series only), `since`, then the endpoint's own values and the cursor. A
repeated parameter keeps its first value, and an empty value counts as not
supplied.

### GET /v1/history/models/{column_id}

`?capability=&difficulty=&dimension=&since=&limit=&cursor=`

The aggregate series for one column, ascending by `(as_of, capability,
difficulty)`. `dimension` ∈ `quality | pass_rate | ambiguity |
latency_p50_ms | latency_p95_ms | cost_per_run_usd`, default `quality`.
`difficulty` is 1 to 5. Columns are catalog-global (not tenant-scoped), and
retired columns keep their history. The parameters are validated before the
column lookup; an unknown column is `404`.

```json
{
  "column_id": "col_01K...",
  "dimension": "quality",
  "series": [
    { "as_of": "2026-07-07", "capability": "structured_extraction",
      "difficulty": 3, "ewma": 0.91, "variance": 0.004, "n": 15,
      "measured": true, "matrix_version": "2026-07-07", "regression": false }
  ],
  "next_cursor": null,
  "matrix_version": "2026-07-07"
}
```

`ewma` is the stored raw value (quality in [0,1]); the ×100 display scale
applies to recommendation scores, not the series. `regression: true` marks
points that a §6.5 regression alert names (the column, capability, tier,
dimension, and `as_of` night).

### GET /v1/history/recommendations

`?prompt_hash=&capability=&since=&limit=&cursor=`: the tenant audit log,
newest first:

```json
{
  "recommendations": [
    { "id": "rec_01K...", "created_at": "2026-07-07T21:02:58Z",
      "capability": "multi_step_reasoning", "difficulty": 3,
      "pick": { "provider": "anthropic", "model": "claude-sonnet-4-6",
                "endpoint": "direct", "column_id": "col_01K...", "score": null },
      "mode": "balanced", "pool": "default",
      "flags": { "constraints_met": true, "measured": false, "clears_difficulty": false } }
  ],
  "next_cursor": null,
  "matrix_version": "2026-07-07"
}
```

Each summary comes from the stored decision: `capability` and `difficulty`
from its classification; `pick` is the first recommendation (`null` when
there was none); `mode`, `pool`, `constraints_met`, and `measured` from the
response; `clears_difficulty` is the pick's. `since` is inclusive.
`prompt_hash=` answers "how often did this exact prompt arrive, and how was it
classified each time"; hashes are exact-match by design.

### GET /v1/history/recommendations/{recommendation_id}

One full decision trace (§10.2): the request as received (with the prompt
removed on the prompt path), the classification block, every candidate
considered, and the response snapshot. The jsonb columns come back as Postgres
stores them.

```json
{
  "id": "rec_01K...", "api_key_id": "key_01K...", "prompt_hash": null,
  "request": { "mode": "balanced", "workload": { "capability": "structured_extraction", "difficulty": 3 } },
  "classification": { "...": "..." },
  "candidates": [
    { "column_id": "col_01K...", "provider": "anthropic", "model": "claude-sonnet-4-6",
      "endpoint": "direct", "status": "survivor", "rank": 0, "v": 0.87,
      "quality": 94.1, "pass_rate": 0.91, "cost_per_run_usd": 0.0042, "latency_p95_ms": 2100,
      "leader": true, "measured": true, "clears_difficulty": true, "prefs_satisfied": true,
      "availability": 0.99, "low_availability": false },
    { "column_id": "col_01H...", "provider": "openai", "model": "gpt-5.5",
      "endpoint": "direct", "status": "excluded", "exclusion_reason": "not_in_allowlist" }
  ],
  "response": { "...": "the §9.2 response as served..." },
  "matrix_version": "2026-07-07",
  "created_at": "2026-07-07T21:02:47Z"
}
```

Exclusion reasons (a closed enum): `not_in_pool`, `not_in_allowlist`,
`standing_constraint:max_cost_per_run`, `standing_constraint:max_latency_ms`,
`standing_constraint:zero_retention` (D1), `status_retired`, `no_data`.
Survivors carry `availability` and `low_availability` (D8). Another tenant's
id is the same `404` as a missing one.

### GET /v1/history/outcomes

`?recommendation_id=&outcome=&since=&limit=&cursor=` → `{"outcomes": [...],
"next_cursor": ..., "matrix_version": ...}`, newest first, scoped to the
tenant through the owning recommendation:

```json
{ "id": "out_01K...", "recommendation_id": "rec_01K...",
  "model_used": "claude-sonnet-4-6", "outcome": "parse_failure",
  "escalated_to": null, "details": { "status": 200, "check_pass": false },
  "endpoint_used": "direct", "created_at": "2026-07-07T21:03:01Z" }
```

`outcome` ∈ `success | parse_failure | quality_insufficient |
latency_exceeded | provider_error | completed_unchecked | other`. The filter
takes `completed_unchecked`, and the 422 for another value lists it (D40;
D16 introduced the value). Filtering by another tenant's recommendation id
gives an empty page. `details` is `null` when the row has none.

### GET /v1/history/calibration

`?capability=&since=&limit=&cursor=`: the nightly prediction-calibration
ledger (system-wide, not tenant-scoped), newest batch first and capabilities
ascending within a batch:

```json
{
  "calibration": [
    { "batch_id": "bat_01K...", "capability": "structured_extraction",
      "brier_score": 0.11, "overconfidence": 0.03, "n": 45,
      "scored_at": "2026-07-08T03:40:00Z" }
  ],
  "next_cursor": null,
  "matrix_version": "2026-07-07"
}
```

## Tenant configuration

Reads mirror writes. PUTs are **full-replace** per resource and are validated
before anything is written (G4): `200` with the stored config, or `422` with
nothing written.

### GET/PUT /v1/config/pools

```json
{
  "pools": [
    { "name": "default",
      "model_column_ids": ["col_01K...", "col_01L..."],
      "is_default": true }
  ]
}
```

Exactly one pool must be `is_default: true`. Pools reference columns by
`column_id` (precise config; request-time `allowed_models` takes model names
for ergonomics). Violations: `422 unknown_model` (an id not in the catalog),
`422 pool_would_be_empty` (a pool with no usable column once retired columns
and the standing constraints apply, including zero retention: a
zero-retention tenant's pools each need a column with valid zero-retention
evidence, D1), `422 invalid_request`.

### GET/PUT /v1/config/weights

Per-capability ranking weights and quality bar:

```json
{
  "weights": [
    { "capability": "structured_extraction",
      "w_quality": 0.5, "w_cost": 0.35, "w_latency": 0.15,
      "quality_bar": 0.8 }
  ]
}
```

`w_quality`/`w_cost`/`w_latency` are required per entry; `quality_bar`
defaults to 0.80. Capabilities without a row use the mode-default weights.
Violations: `422 invalid_request`.

### GET/PUT /v1/config/constraints

Standing tenant constraints. They filter candidates hard on every request:

```json
{
  "constraints": {
    "max_cost_per_run": 0.01,
    "max_latency_ms": 5000,
    "zero_retention": false,
    "telemetry_opt_out": false
  }
}
```

`null` disables a limit. A write that would leave any pool without usable
columns is refused `422 pool_would_be_empty`. `zero_retention: true` also
forbids debug capture windows (G5). The PUT is a full replace: a missing key
is `null` or `false`.

`telemetry_opt_out: true` (D39) turns off outcome telemetry for the tenant.
Recommend responses then carry `"telemetry": false`, MM components send no
telemetry, and the engine drops any telemetry items it still receives. The
proxy also exports no OTel record for a request whose recommendation
says `telemetry: false`: no log record, and no count in
`maverick_proxy_recommendations_total` (D43). A request that fails before the
engine answers has no flag, so it is still recorded. The constraint filters no
model column, so it never causes `pool_would_be_empty`.

## Debug capture

Debug capture is the only place the engine stores a prompt (G1). A tenant
opens a window. While the window is active, the engine seals matching prompts
with AES-256-GCM and keeps them for replay and reading. A zero-retention
tenant cannot open a window and is never captured, even by a window it opened
before it set the constraint (G5).

### POST /v1/debug/capture-windows

```json
{ "api_key_id": "key_01K...", "confidence_below": 0.5, "duration_minutes": 60 }
```

- `duration_minutes` is required. It is an integer from 1 to
  `MAVERICK_CAPTURE_MAX_WINDOW_MINUTES` (default 1440).
- `confidence_below` is optional. It is a number in (0, 1]. The window
  captures only classifications whose confidence is strictly below it. It is
  stored as float32, so the reply echoes the widened value (0.8 reads
  `0.800000011920929`).
- `api_key_id` is optional. It must name one of your own keys, and the window
  then captures only that key's requests.

A failed check is `422 invalid_request`. After these checks, a zero-retention
tenant gets `409 zero_retention_conflict`. So a bad `api_key_id` answers 422
even for a zero-retention tenant (QUIRK Q8).

A prompt-form recommend request is captured by the first window that is
active, matches its key, and has its confidence bar above the classification's
confidence. Declared requests have no prompt and are never captured. A capture
keeps until the window's end plus `MAVERICK_CAPTURE_RETENTION_HOURS` (default
72). Each capture is sealed under the current key of a key ring, with a new
nonce, and stores its key version (D18). A capture failure is logged and never
fails the recommendation.

`201`:

```json
{ "id": "cw_01K...", "api_key_id": null, "confidence_below": null,
  "starts_at": "2026-07-07T21:03:33Z", "ends_at": "2026-07-07T22:03:33Z",
  "created_at": "2026-07-07T21:03:33Z" }
```

### DELETE /v1/debug/capture-windows/{id}

Ends the window now: `204`. An unknown or already deleted window and another
tenant's window are the same `404`. The window's captures stay, and their
`window_id` becomes `null`.

### POST /v1/debug/replay/{capture_id}

The body is optional: `{"classifier_version": "..."}`. v1 runs only the
current classifier version. Any other value is `422 invalid_request` (details
`classifier_version` and `current`).

The engine decrypts the prompt in memory and classifies it again, without the
classification cache. `match` is true when the replay has the stored
capability and difficulty. Replay never returns the prompt.

```json
{ "original_classification": { "capability": "code_generation", "difficulty": 3, "...": "..." },
  "replayed_classification": { "capability": "code_generation", "difficulty": 3,
    "domain_tags": [], "high_stakes": false, "confidence": 0.91, "method": "classified" },
  "match": true }
```

`original_classification` is the classification block of the capture's trace
row. `replayed_classification` also has `truncated: true` when the classifier
input was truncated.

A replay is a real classifier call, so the prompt path's rules apply. A
zero-retention tenant's prompt goes only to the zero-retention classifier
column while its evidence is valid, and otherwise the answer is
`fallback_default` (D2). The D4 quota and budget apply, and the call is
recorded as classification spend.

A replay appends a `capture_access_log` row (action `replay`, actor your API
key id). An expired capture appends a `read` row and answers `410
capture_expired`. A capture that the sweep already deleted also answers `410`.
Another tenant's capture and an id that never existed are the same `404`. If
the log append fails, the call fails.

### GET /v1/debug/captures

Your own captures, newest first. No parameter widens the tenant scope. The
parameters are `window_id`, `limit` (default 50, 1 to 200), and `cursor` (the
[Pagination contract](#pagination-contract) shape). Any other parameter is
`422 invalid_request`. The list never has prompt text, and it is not
access-logged.

```json
{
  "captures": [
    { "id": "cap_01K...", "window_id": "cw_01K...",
      "recommendation_id": "rec_01K...", "prompt_hash": "a3f2...",
      "classification": { "...": "..." },
      "captured_at": "2026-07-07T21:04:10Z",
      "expires_at": "2026-07-10T22:03:33Z", "expired": false }
  ],
  "next_cursor": null
}
```

`prompt_hash` and `classification` come from the capture's trace row.
`expired` is `expires_at <= now`. An expired capture stays in the list until
the sweep deletes it.

### GET /v1/debug/captures/{id}/prompt

Decrypts **your own** captured prompt in memory and returns it. The window is
the consent act. The prompt is encrypted at rest, and it expires on the
capture's schedule. Every read appends a `capture_access_log` row (action
`read`, actor your API key id), and so does a read that answers 404 or 410.

```json
{ "id": "cap_01K...", "prompt": "...", "prompt_hash": "a3f2...",
  "recommendation_id": "rec_01K...", "classification": { "...": "..." },
  "captured_at": "2026-07-07T21:04:10Z", "expires_at": "2026-07-10T22:03:33Z" }
```

`recommendation_id` joins to `GET /v1/history/recommendations/{id}` for the
full decision trace: the trace says why, and the capture says what. Another
tenant's capture and an unknown id are the same `404`. Expired or
hard-deleted: `410 capture_expired`.

## GET /up

Unauthenticated liveness (the Kamal health check): `200 {"status":"ok"}`
when Postgres answers a ping within 2 s, else `503 {"status":"unavailable"}`.

## MCP server (`/mcp` and stdio)

Two transports serve the same three tools (spec §12). A tool call acts as a
tenant exactly as the REST call does, and an MCP recommendation writes the same
trace row.

- **Streamable HTTP** at `/mcp` on the public listener, behind the `/v1`
  middleware: `Authorization: Bearer mk_live_...`, the per-key rate limit, and
  the 2 MiB body cap. Each message acts as the key on its own HTTP request.
  Each JSON-RPC response is one `event: message` SSE frame. `initialize`
  opens a session and returns its id in `Mcp-Session-Id`. Later POSTs must
  send that header: without it they are `400`, and with an unknown id they are
  `404`. A notification is `202`. `DELETE /mcp` with the header ends the
  session (`204`). Other methods are `405` with `Allow: POST, DELETE`. These
  transport errors are plain text, not the error envelope.
- **stdio** for local development: `maverick mcp-stdio --api-key mk_live_...`.
  The key is authenticated once at startup, and the session acts as that
  tenant. Messages are newline-delimited JSON-RPC. Logs go to stderr, and
  stdout carries only the protocol.

The server identifies as `model-maverick`, version `v1`. It speaks protocol
versions `2025-11-25`, `2025-06-18`, `2025-03-26`, and `2024-11-05`, and it
echoes the client's version when it is on that list. A client that asks for
another version gets `2025-11-25`. The capabilities are `logging` and
`tools.listChanged`. `tools/list` is the Go server's list byte for byte, with
the three tools sorted by name.

| Tool | Input | Returns |
|---|---|---|
| `recommend_model` | The `POST /v1/recommend` body: `{workload, pool?, allowed_models?, mode?, constraints?}`. The MCP prompt form has no `projection_version`. | The `POST /v1/recommend` response |
| `get_policy` | `{capability?, pool?}` | The `GET /v1/policies` response (D42 applies) |
| `list_models` | `{}` | The `GET /v1/models` response |

The tool descriptions ask agents to prefer the declared form when they know
the capability: it is faster, and nothing leaves the session.

A successful result carries the REST body twice: as one text content item and
as `structuredContent`. A failed call is a result with `isError: true` and one
text item. An API error reads `<code>: <message>`, with the HTTP route's code
and message (for example `unknown_pool: ...`, or
`zero_retention_requires_declared: ...` for the prompt form of a
zero-retention tenant). An internal failure reads `internal error`. The
arguments are checked against the tool's input schema first. A violation is
an `isError` result whose text starts with `validating "arguments": ` (D34).
An unknown tool is JSON-RPC error `-32602`, and an unknown method is
`-32601`.

## SDK integration conventions

Every MM-authored component (the client SDKs, the gateway plugins, and the
proxy) follows two conventions (integration-surfaces plan §1). They are
not part of the engine's own wire contract.

- **Model-alias grammar.** `maverick/auto` (balanced), `maverick/quality`,
  `maverick/cost`, or `maverick/<pool>:<mode>`, where `<mode>` is `auto`,
  `quality`, or `cost`. The grammar is exact and case sensitive. The string
  splits at the first `:`, and neither part may be empty. `maverick-core`
  parses it, so the proxy and the SDK cores agree. An alias may end with
  `?settings=caller`: the component then leaves the caller's settings alone
  instead of applying the recommended entry's `settings` (D14). A string
  without the `maverick/` prefix is not an alias: the proxy refuses it with
  `400 invalid_model_alias`, and an SDK passes it through. The component never
  sends an alias to the engine. It turns the alias into a `POST /v1/recommend`
  call (declared or prompt form, per its privacy mode) and runs its own chain
  executor over the returned chain.
- **Correlation response headers.** A component that sits in the request path
  sets `X-Maverick-Recommendation-Id` and `X-Maverick-Model-Used` (the model it
  executed) on the response. A caller can then tie a provider response to its
  recommendation without parsing a body. The engine does not set these
  headers: `POST /v1/recommend` returns `recommendation_id` in the body.

---

# Internal provisioning API (`/internal/v1`)

This is the Rails seam (spec §13). It is served **only** on the internal
listener (`:8081`) and is never proxied. Callers are root: nothing is tenant
scoped.

**Auth:** `Authorization: Bearer <label>:<secret>`. The token is one full
entry of `MAVERICK_INTERNAL_TOKENS`, a comma-separated list of `label:secret`
pairs. The engine compares SHA-256 digests in constant time against every
entry, and the first matching label is the actor in the audit log. A missing
or wrong token is `401 unauthorized`. Example:

```sh
curl -s http://localhost:8081/internal/v1/alerts \
  -H 'Authorization: Bearer ops:dev-internal-secret'
```

Every successful write logs one INFO audit line: `msg="internal provisioning
write"`, `audit=true`, `actor=<label>`, `action=<dotted.action>` (for example
`tenant.create`, `api_key.issue`, `column.update`), and the subject ids.

A route with a tenant `{id}` looks the tenant up before it reads the body. An
unknown tenant is `404 not_found` (`tenant <id> does not exist`).

All 17 routes:

| Method | Path | Purpose |
|---|---|---|
| POST | `/internal/v1/tenants` | Create a tenant |
| GET | `/internal/v1/tenants/{id}` | Tenant and config summary |
| POST | `/internal/v1/tenants/{id}/suspend` | Suspend a tenant |
| POST | `/internal/v1/tenants/{id}/activate` | Reactivate a tenant |
| POST | `/internal/v1/tenants/{id}/api-keys` | Issue an API key |
| DELETE | `/internal/v1/api-keys/{id}` | Revoke an API key |
| PUT | `/internal/v1/tenants/{id}/pools` | Replace pools (as `PUT /v1/config/pools`) |
| PUT | `/internal/v1/tenants/{id}/weights` | Replace weights (as `PUT /v1/config/weights`) |
| PUT | `/internal/v1/tenants/{id}/constraints` | Replace constraints (as `PUT /v1/config/constraints`) |
| PUT | `/internal/v1/tenants/{id}/limits` | Set the classification quota override (D4) |
| GET | `/internal/v1/alerts` | Operator alert feed |
| POST | `/internal/v1/columns` | Create a catalog column |
| PUT | `/internal/v1/columns/{id}` | Edit a column |
| POST | `/internal/v1/models/refresh` | Run provider model discovery now, and optionally enroll new models (D51) |
| POST | `/internal/v1/corpus/activate` | Change the active corpus version |
| GET | `/internal/v1/captures` | List debug captures (no prompt text) |
| GET | `/internal/v1/captures/{id}/prompt` | Decrypt and read one captured prompt (audited) |

### POST /internal/v1/tenants

`{"name": "tenant-one"}` → `201` with the tenant summary. Creation mints the
tenant's prompt-HMAC key (32 random bytes that never leave the engine). It also
creates a `default` pool (`is_default: true`) with **every `active` catalog
column** at that moment. When the catalog has no active column, the tenant
has **no** pool: set one with the pools PUT before the tenant calls
`/v1/recommend`.

```json
{
  "tenant": { "id": "ten_01K...", "name": "tenant-one", "status": "active",
              "created_at": "...", "updated_at": "..." },
  "pools": [ { "name": "default", "model_column_ids": ["col_01K..."], "is_default": true } ],
  "weights": [],
  "constraints": { "max_cost_per_run": null, "max_latency_ms": null, "zero_retention": false,
                   "telemetry_opt_out": false },
  "api_keys": []
}
```

`pools`, `weights`, and `constraints` are exactly what the public config GETs
return. `GET /internal/v1/tenants/{id}` and the suspend and activate POSTs
return the same summary. Each API key appears as `{id, name, last4, status,
created_at, revoked_at}`, never the raw key. Suspension takes effect on the
next request: the tenant's keys answer `403 tenant_suspended` on the public
listener. The suspend and activate POSTs read no body.

### POST /internal/v1/tenants/{id}/api-keys

`{"name": "dev"}` → `201`:

```json
{ "id": "key_01K...", "key": "mk_live_KdVkJFBtDUy0xT5kVgMQWsPV4r2QUZxlLuHeo05Xuf6" }
```

The raw key appears only here. The engine stores its hash and last four
characters, and no endpoint can return it again.

`DELETE /internal/v1/api-keys/{id}` → `200` with the key record (`status:
"revoked"`). The row stays, because history refers to it. Revocation is
idempotent and takes effect on the next request. An unknown key is `404`.

### PUT /internal/v1/tenants/{id}/limits

`{"classification_daily_quota": 500}` sets the tenant's daily classification
quota (D4). An absent field keeps the current override, `null` clears it, and
a value must be at least 0. The reply is the effective quota: the override,
or `MAVERICK_CLASSIFIER_DAILY_QUOTA` (default 1000) when there is none.

```json
{ "classification_daily_quota": 500 }
```

### POST /internal/v1/columns

```json
{
  "provider": "anthropic",
  "model_id": "claude-sonnet-4-6",
  "endpoint": "direct",
  "settings": {},
  "pricing": { "input_per_mtok": 3.0, "output_per_mtok": 15.0 },
  "quality_source_column_id": null,
  "deprecation_date": null,
  "execution_aliases": { "openrouter": "anthropic/claude-sonnet-4-6" },
  "retention_mode": "zero",
  "retention_verified_at": "2026-07-01T00:00:00Z",
  "retention_valid_until": "2026-09-01T00:00:00Z",
  "retention_source": "anthropic ZDR addendum 2026-06",
  "execution_alias_retention": {
    "openrouter": { "retention_mode": "standard" }
  },
  "param_constraints": {
    "reasoning_effort": { "allowed": ["low", "high"], "order": ["low", "medium", "high"] }
  }
}
```

- `provider`, `model_id`, and `endpoint` are required. `provider` is one of
  `anthropic`, `openai`, `google`, and `xai`. `endpoint` is one of `direct`,
  `vertex`, `bedrock`, `azure`, and `openrouter`.
- `settings` is an object and defaults to `{}`. A second column with the same
  provider, model, endpoint, and settings is `422 invalid_request`.
- `pricing` is USD per million tokens.
- `quality_source_column_id`, when set, must name a catalog column.
  `deprecation_date` is `YYYY-MM-DD`.
- `execution_aliases` is optional and defaults to `{}`. It is an object with
  string values only, at most 10 keys, and each value non-empty and at most 200
  characters.
- The retention evidence is optional (spec §4.5; D1, D15). `retention_mode` is
  `unknown`, `standard`, or `zero`. The times are RFC 3339.
  `retention_valid_until` needs `retention_verified_at` and is at most 90 days
  after it. `execution_alias_retention` holds the same evidence for each alias,
  keyed by an `execution_aliases` key.
- `param_constraints` is optional (D29). It is keyed by a known parameter
  (only `reasoning_effort` in v1). Each value has non-empty string lists
  `allowed` and `order`, `order` has no repeated value, and every `allowed`
  value is in `order`.

A failed check is `422 invalid_request` with the field in `details.field`. A
new column always starts at `status: "cold_start"`, with `cold_start_until` =
today + `MAVERICK_CATALOG_COLD_START_NIGHTS` (default 5). The create body does
not take `status`. `201`:

```json
{ "column_id": "col_01K...", "provider": "anthropic", "model_id": "claude-sonnet-4-6",
  "endpoint": "direct", "settings": {}, "execution_aliases": { "openrouter": "anthropic/claude-sonnet-4-6" },
  "status": "cold_start",
  "pricing": { "input_per_mtok": 3.0, "output_per_mtok": 15.0 },
  "quality_source_column_id": null, "cold_start_until": "2026-07-13",
  "deprecation_date": null, "created_at": "...", "updated_at": "..." }
```

The reply has Go's column shape. It does not echo the retention evidence or
`param_constraints`: `GET /v1/models` shows them.

### PUT /internal/v1/columns/{id}

A partial update of `status`, `pricing`, `deprecation_date`,
`execution_aliases`, the retention evidence, `execution_alias_retention`, and
`param_constraints`. An absent or `null` field keeps its stored value. The
validation is the create's. `retention_valid_until` alone is checked against
the stored `retention_verified_at`. `status` is one of `cold_start`, `active`,
`deprecated`, and `retired`. An unknown column is `404`. The reply is the
column, as for create.

- **Retirement guard (§4.1).** A change to `retired` is refused when a tenant
  pool would have no usable column left: `422 pool_would_be_empty` names the
  pools, a critical alert is written, and the column keeps its status.
- **Retention downgrade (D1).** A change that ends a column's valid zero
  evidence is not refused. It runs the G4 simulation, and each zero-retention
  tenant whose pool it would empty gets a critical `pool_would_be_empty`
  alert.
- `execution_aliases`, `execution_alias_retention`, and `param_constraints`
  **replace the stored value whole**. There is no per-key merge.
- A change of status, pricing, `execution_aliases`, or `param_constraints`
  logs an INFO `catalog_changes` line with the old and new values. A price edit
  replaces the list-price snapshot, and past run costs are never restated.

### POST /internal/v1/models/refresh

D51, spec §4.6. Runs provider model discovery now, the same code as the daily
`model_discovery` job, and writes the same alerts, except for the models this
call enrolls. With `enroll`, it also adds new models to the catalog. The body is
optional: an empty body or `{}` only reports. Each provider's list call may take
12 s; the providers are listed one after another.

```json
{
  "enroll": [
    { "provider": "anthropic", "model_id": "claude-sonnet-5-5-20260930",
      "pricing": { "input_per_mtok": 3.0, "output_per_mtok": 15.0 } }
  ]
}
```

`200`:

```json
{
  "ran_at": "2026-10-12T06:20:00Z",
  "providers": [
    { "provider": "anthropic", "status": "ok", "listed": 9 },
    { "provider": "google", "status": "skipped_no_key" },
    { "provider": "openai", "status": "failed", "error": "provider_error (status 503)" },
    { "provider": "xai", "status": "ok", "listed": 6 }
  ],
  "new": [
    { "provider": "anthropic", "model_id": "claude-sonnet-5-5-20260930",
      "base_model_id": "claude-sonnet-5-5", "display_name": "Claude Sonnet 5.5",
      "created_at": "2026-09-30T00:00:00Z" }
  ],
  "withdrawn": [
    { "column_id": "col_01K...", "provider": "xai", "model_id": "grok-3" }
  ],
  "enrolled": [],
  "spend_preview": null
}
```

- `providers` has one entry per provider, in the order `anthropic`, `google`,
  `openai`, `xai`. `status` is `ok` (with `listed`, the count of kept text
  models), `skipped_no_key`, or `failed`. A failure gives only the error class
  and the HTTP status in `error` (`provider_error (status 503)`, or `timeout`),
  never a response body (G5).
- `new` and `withdrawn` follow spec §4.6, as the run found them before any
  enrollment. `new` names the newest listed snapshot as `model_id`, and
  `base_model_id` is the id without the date stamp. `display_name` and
  `created_at` appear when the provider gives them. An empty catalog makes every
  kept model new.
- **Enrollment.** Each entry takes the fields of `POST /internal/v1/columns`.
  `provider`, `model_id`, and `pricing` are required (provider lists carry no
  prices), and `endpoint` defaults to `direct`. `model_id` must be a listed id
  whose base id is in this run's `new` set: the reported snapshot, or another
  listed id of the same base. All entries are created in one transaction, as
  `cold_start` columns, through the column create path and its validation. If
  one entry fails, no column is created. `enrolled` holds the created columns
  in the column reply shape.
- `spend_preview` is `null` without enrollment. With enrollment, it is the
  projection for the next nightly batch with the new columns, from the same
  computation the batch's spend guard runs (`maverick spend preview`, D52):
  `{job, night, runs, projected_usd, night_spent_usd, nightly_cap_usd,
  week_spent_usd, weekly_cap_usd, decision}`. `night` is the night of the next
  `MAVERICK_CRON_NIGHTLY` firing. `decision` is `allow`, `warn`, or `refuse`;
  `refuse` means that batch will not run (G8).
- A run in which every provider is skipped or failed still answers `200`, with
  each provider's status.

Errors:

| Status | Code | When |
|---|---|---|
| 401 | `unauthorized` | Missing or wrong internal token |
| 413 | `payload_too_large` | Body over 2 MiB |
| 422 | `invalid_request` | A malformed body or unknown field. Also: an entry without `pricing`, or with a field that the column create rejects (`details.field`). An entry for a provider that was not `ok` in this run (`details: {provider, provider_status}`). An entry whose model is not listed, or whose base id is already cataloged (`details: {provider, model_id}`). Two entries for the same column. A column that a concurrent call created first |
| 500 | `internal_error` | Store failure |

There is no `409` and no `503`: the error code list is closed, and these cases
are `422` or a `200` with provider statuses.

### GET /internal/v1/alerts

`?since=&kind=` → `{"alerts": [{id, kind, severity, subject, message,
created_at, acked_at}, ...]}`, oldest first, with no pagination. `since` is an
RFC 3339 timestamp or a `YYYY-MM-DD` date (midnight UTC). `subject` is the
alert's JSON subject. Any other parameter is `422 invalid_request`. The same
rows go out by email and webhook, as `docs/operations.md` describes.

### POST /internal/v1/corpus/activate

`{"version": "2026-07-07"}` makes that corpus version the active one, in one
step (the HTTP twin of `maverick corpus activate`). The version must already
be loaded (`maverick corpus load`). An unknown version is `404`. `200`:

```json
{ "version": "2026-07-07", "description": "...", "task_count": 221, "active": true,
  "created_at": "..." }
```

### GET /internal/v1/captures, GET /internal/v1/captures/{id}/prompt

The operator reads debug captures across tenants. To judge whether a routed
model was right for a request, the operator needs the prompt that drove the
classification, not only its fingerprint. The custody rules are the tenant
reads' (see [Debug capture](#debug-capture)): AES-256-GCM at rest, the
capture's own expiry, and an audit row for every prompt read. No tenant route
ever returns another tenant's prompt.

`GET /internal/v1/captures?tenant_id=&window_id=&limit=&cursor=` lists every
tenant's captures, newest first, with the tenant list's paging. Each entry
also has `tenant_id`. The list has **no prompt text**, and it is not
access-logged.

```json
{
  "captures": [
    { "id": "cap_01K...", "window_id": "cw_01K...", "tenant_id": "ten_01K...",
      "recommendation_id": "rec_01K...",
      "prompt_hash": "a3f2...", "classification": { "...": "..." },
      "captured_at": "2026-07-07T21:04:10Z",
      "expires_at": "2026-07-10T22:03:33Z", "expired": false }
  ],
  "next_cursor": null
}
```

`GET /internal/v1/captures/{id}/prompt` decrypts the capture in memory and
returns:

```json
{ "id": "cap_01K...", "tenant_id": "ten_01K...", "prompt": "...",
  "prompt_hash": "a3f2...", "recommendation_id": "rec_01K...",
  "classification": { "...": "..." },
  "captured_at": "2026-07-07T21:04:10Z", "expires_at": "2026-07-10T22:03:33Z" }
```

`recommendation_id` joins to `GET /v1/history/recommendations/{id}` for the
decision trace. Nothing is classified again here, unlike replay. Every attempt
appends a `capture_access_log` row (action `read`, actor the internal token's
label): a hit, an expired capture, a hard-deleted one, and an unknown id.
Expired or hard-deleted: `410 capture_expired`. Unknown id: `404`.

## GET /health/detail (internal listener)

Component health, behind the same internal bearer auth:

```json
{
  "status": "ok",
  "components": {
    "db": { "status": "ok" },
    "job_queue": { "status": "ok", "detail": "graphile_worker queues: default" },
    "provider_client_config": { "status": "ok", "detail": "configured: anthropic, openai" }
  },
  "active_matrix_version": "2026-07-07",
  "last_batch_times": { "nightly_smoke": "2026-07-08T03:41:12Z" }
}
```

A component's `detail` is omitted when it is empty.

- `db` is `ok` when Postgres answers a ping within 2 s. Otherwise it is
  `error` with the failure, the reply is `503` with `status: "degraded"`,
  `job_queue` is `unknown`, and the two top-level fields are `null`.
- `job_queue` reads graphile_worker's worker registry from Postgres (D35).
  So an `--api-only` node reports the workers of a separate `--worker-only`
  node. It is `no_workers` when no worker is registered, `stale` when the
  freshest heartbeat is older than 60 s, and `ok` otherwise. The Go engine
  called this component `river_queues` and also had `paused`.
- `provider_client_config` is `none_configured` when no provider API key is
  set.
- `active_matrix_version` is the active corpus version, or `null` on a cold
  stack.
- `last_batch_times` maps each batch kind to its newest completed batch
  (`batches.completed_at`). A kind that never completed is absent.
