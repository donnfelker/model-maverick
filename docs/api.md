# Model Maverick — API Reference

This document describes the HTTP surface the binary actually serves. Two
listeners (spec §2.1):

| Listener | Default address | Serves |
|---|---|---|
| Public | `:8080` (`MAVERICK_PUBLIC_ADDR`) | `/up`, `/v1/*`, `/mcp` |
| Internal | `:8081` (`MAVERICK_INTERNAL_ADDR`) | `/health/detail`, `/internal/v1/*` |

All request and response bodies are JSON (`Content-Type: application/json`).
Request bodies are capped at 2 MB (`413 payload_too_large`). Request bodies
and query strings are decoded strictly: unknown JSON fields, trailing
content, and unknown query parameters are `422 invalid_request`.

Every `/v1` response carries `matrix_version` — the active corpus version, or
`""` when no corpus has been activated yet.

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
  (seconds). The limiter is in-memory and per-process by design for v1.

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

The code list is closed — no response carries a code outside this table:

| HTTP | `code` | Meaning |
|---|---|---|
| 401 | `unauthorized` | Missing/invalid/revoked key (or internal token) |
| 403 | `tenant_suspended` | Valid key, suspended tenant |
| 404 | `not_found` | Resource absent **or** owned by another tenant (no existence oracle) |
| 409 | `zero_retention_conflict` | Capture window requested by a zero-retention tenant |
| 410 | `capture_expired` | Replay of a hard-deleted capture |
| 413 | `payload_too_large` | Body > 2 MB |
| 422 | `invalid_request` | Malformed body, unknown fields/params, bad enums, bad values |
| 422 | `unknown_model` | `allowed_models` entry not in the system catalog (names the entry) |
| 422 | `model_not_in_pool` | `allowed_models` entry outside the caller's pool (names the entry) |
| 422 | `unknown_pool` | Pool name not configured for the tenant |
| 422 | `pool_would_be_empty` | Config write or retirement would empty a pool (names the pool) |
| 422 | `grader_unsupported` | Corpus load: reserved grader type in v1 |
| 429 | `rate_limited` | Per-key limit; `Retry-After` set |
| 500 | `internal_error` | Bug; logged with the request id |

There is no 405 entry: a wrong method on a known path answers exactly like an
unknown path (`404 not_found`).

The four 422 policy codes (`unknown_model`, `model_not_in_pool`,
`unknown_pool`, `pool_would_be_empty`) are contractual (G3) — callers are
expected to branch on them.

## Pagination contract

All `GET /v1/history/*` list endpoints paginate identically:

- `?limit=` — default 50, maximum 200; out-of-range is `422 invalid_request`.
- `?cursor=` — opaque token (ULID-based) from a previous page.
- Responses carry `next_cursor`: the token for the next page, or `null` on
  the last page.
- `?since=` filters accept RFC 3339 timestamps or bare `YYYY-MM-DD` dates.

---

## POST /v1/recommend

Recommends model columns for a workload. `workload` is required and carries
**exactly one** of two forms:

- **Declared form** — `{"capability": "...", "difficulty": 1-5, "domain_tags": [...]}`.
  `capability` is one of the ten v1 capabilities (see `GET /v1/policies`);
  `difficulty` defaults to 3 (the response then carries
  `classification.difficulty_source: "default"`); `domain_tags` optional.
  Nothing leaves the engine — no classifier call.
- **Prompt form** — `{"prompt": "..."}` only (no `difficulty`/`domain_tags`
  alongside). The prompt is classified (§8.3), fingerprinted (`prompt_hash`),
  and never stored in plaintext. Non-empty, at most 500,000 characters.

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
- `constraints` are per-request preferences; standing tenant constraints
  (`/v1/config/constraints`) filter candidates hard, per-request preferences
  only reorder and flag.

Response `200` (measured stack):

```json
{
  "classification": { "capability": "structured_extraction", "difficulty": 3,
    "domain_tags": ["legal"], "high_stakes": true, "confidence": 0.91, "method": "classified" },
  "recommendations": [
    { "provider": "anthropic", "model": "claude-sonnet-4-6", "endpoint": "direct",
      "column_id": "col_01J...", "execution_aliases": { "openrouter": "anthropic/claude-sonnet-4-6" },
      "score": 94.1, "clears_difficulty": true,
      "reason": "cheapest column clearing tier 3 structured_extraction at your quality bar in this pool" },
    { "provider": "google", "model": "gemini-3.1-pro", "endpoint": "direct",
      "column_id": "col_01J...", "execution_aliases": {},
      "score": 92.8, "clears_difficulty": true, "reason": "cross-family alternative" }
  ],
  "pool": "default",
  "mode": "balanced",
  "check_spec": { "type": "json_schema", "ref": "checks/extraction/v1" },
  "constraints_met": true,
  "violations": [],
  "prompt_hash": "hmac_sha256:9f2c...",
  "recommendation_id": "rec_01J...",
  "matrix_version": "2026-07-07",
  "measured": true
}
```

Field notes (all verified against the implementation):

- `classification.method` ∈ `declared | classified | cached |
  fallback_default`. When the classifier is unavailable (no provider key, or
  the classifier column is missing from the catalog), prompt-form requests
  still answer `200` with the fallback classification:
  `{"capability": "multi_step_reasoning", "difficulty": 3, "domain_tags": [],
  "high_stakes": false, "confidence": 0, "method": "fallback_default"}` (G1).
- `classification.difficulty_source: "default"` appears only when a declared
  request omitted `difficulty`; `classification.truncated: true` appears only
  when an oversize prompt was classified on head+tail.
- `score` is the absolute displayed quality (0–100, one decimal). It is
  `null` when the workload cell has no quality EWMA yet (cold start), and
  `measured` is then `false`. Every recommendation is still produced (G1);
  see `docs/onboarding.md` for what a fully cold response looks like.
- `recommendations` is the whole pool-constrained fallback chain, best first
  (G2). `reason` strings are informative, not contractual; the flags are.
- `execution_aliases` is the recommended column's execution-backend alias map
  (`GET /v1/models` documents the shape) — empty object when the column
  carries none. It rides along on every entry so an SDK configured for an
  alternate execution backend (e.g. OpenRouter) needs zero extra calls to
  dispatch the pick.
- `prompt_hash` is present only on prompt-form requests:
  `"hmac_sha256:" + hex(HMAC-SHA256(tenant key, prompt bytes))` — exact
  bytes, per-tenant key, exact-match only.
- `violations` entries: `{"constraint": "max_latency_ms", "limit": 2000,
  "estimated": 3400}`; `constraints_met` is `false` when the pick violates a
  per-request preference.
- Every call (REST or MCP) persists a full decision trace readable at
  `GET /v1/history/recommendations/{id}`.

Errors: `422 invalid_request | unknown_model | model_not_in_pool |
unknown_pool`, `413`, `429`.

## GET /v1/policies

`GET /v1/policies?capability=structured_extraction&pool=default` — both
parameters optional; absent `capability` returns all ten, absent `pool` uses
the tenant default.

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
weights row for the requested capability (default 0.80); `buffer` is the
§6.6 ceiling buffer (`MAVERICK_CEILING_BUFFER`).

Errors: `422 invalid_request` (unknown capability or query param),
`422 unknown_pool`.

## GET /v1/models

The column catalog visible to the tenant — all non-retired columns
(`cold_start`, `active`, `deprecated`). No scores here; scores are per-cell
and live in policies/history.

```json
{
  "models": [
    { "column_id": "col_01K...", "provider": "anthropic",
      "model_id": "claude-sonnet-4-6", "endpoint": "direct", "settings": {},
      "execution_aliases": { "openrouter": "anthropic/claude-sonnet-4-6" },
      "status": "active",
      "pricing": { "input_per_mtok": 3.0, "output_per_mtok": 15.0 },
      "deprecation_date": null, "measured": false,
      "quality_source_column_id": null }
  ],
  "matrix_version": "2026-07-07"
}
```

`measured` is true once the column has any measured aggregate cells.
`quality_source_column_id` names the column this one inherits quality cells
from (§4.2 — e.g. a Vertex column sourcing quality from its direct sibling).
`execution_aliases` is a flat string→string map of execution-backend model
slugs, empty object when unset. This spec defines one key, `"openrouter"`
(the OpenRouter model slug); unknown keys are preserved and ignored.
Operator-populated only (`POST`/`PUT /internal/v1/columns`) — no tenant-facing
write surface. `endpoint` also accepts `"openrouter"` as a legal catalog
value (comment + validation only; no such columns are created by this spec).

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

The registry is static, embedded in the binary, one spec per capability
(the same `check_spec.ref` values `/v1/recommend` returns). Unknown refs are
`404 not_found`.

## POST /v1/telemetry

**This is SDK/plugin plumbing, not a customer obligation (G13).**
MM-authored components — the client SDKs, gateway plugins, and the trial
proxy — call this automatically as an invisible side effect of routing a
request; no public surface ever asks a caller to report what happened by
hand. A component with telemetry disabled simply never calls it (zero calls,
no fallback phoning home) — the documented consequence is that tenant's own
outcome blending (v1.1 §7) has no data and its rankings stay
benchmark-driven, never a punished caller. `POST /v1/outcomes`, the v1
caller-reported-outcome ceremony this endpoint replaces, is retired.

Body: `{"items": [...]}` — `items` required, an array, at most 500 entries.
These are the *only* 4xx cases this endpoint has: malformed JSON, a
non-object body, missing or non-array `items`, or more than 500 entries all
answer `422 invalid_request`. Everything about individual items is
best-effort and never fails the batch (see below).

Item shape:

```json
{ "recommendation_id": "rec_01J...", "model_used": "claude-sonnet-4-6",
  "status": 200, "latency_ms": 812, "tokens_in": 340, "tokens_out": 120,
  "finish_reason": "stop", "error_class": null, "check_pass": true,
  "endpoint_used": "direct" }
```

- `recommendation_id`, `model_used`, `status` required. `status` is the
  provider's HTTP status; `0` means a transport-level failure (connect error
  or timeout before any response arrived).
- `latency_ms`, `tokens_in`, `tokens_out` — optional non-negative integers.
- `finish_reason` — optional string.
- `error_class` — optional, ∈ `timeout | connect_error | provider_error |
  other`.
- `check_pass` — optional boolean.
- `endpoint_used` — optional, ∈ `direct | vertex | bedrock | azure |
  openrouter`; absent means `direct`.

**Per item, best-effort:** an item that's missing or mistypes a required
field, fails an optional field's constraint, names a `recommendation_id`
that doesn't exist, or belongs to another tenant (no cross-tenant existence
oracle — dropping is indistinguishable from unknown) is **dropped and
counted**, never surfaced as a per-item error, never failing the batch.

`200 {"accepted": N, "dropped": M}`. An empty `items` array is a valid
no-op: `{"accepted": 0, "dropped": 0}`.

**Tenant opt-out (D39):** when the tenant's `telemetry_opt_out` constraint is
set, every item is dropped and counted, and no outcome row is written. The
response has the same shape. Recommend responses carry `"telemetry": false`
for such a tenant, so MM components send nothing for it in the first place.

**Outcome derivation** (server-side; first matching rule wins):

1. `error_class == "timeout"` → `latency_exceeded`
2. `error_class ∈ {"connect_error","provider_error"}`, or `status == 0`, or
   `status == 429`, or `status >= 500` → `provider_error`
3. 2xx `status` and `check_pass == false` → `parse_failure`
4. 2xx `status` → `success`
5. anything else (e.g. a semantic 4xx) → `other`

`quality_insufficient` remains a legal value in the `outcomes` table (it
still appears on historic rows, and survives the check constraint), but no
telemetry item can ever produce it going forward.

Stored row — the same `outcomes` table as before, one new column:
`id` `out_<ulid>`, `recommendation_id`, `model_used`, the derived `outcome`,
`endpoint_used`, `escalated_to: null` (no telemetry equivalent), and
`details` — a jsonb object holding exactly the facts the item supplied:
`status` always, plus `latency_ms`/`tokens_in`/`tokens_out`/`finish_reason`/
`error_class`/`check_pass` only when present on the item. Multiple items per
recommendation are allowed and appended (retries, chain escalations) —
unchanged from the retired `POST /v1/outcomes`.

## History

Five read endpoints, all paginated (see the pagination contract above).

### GET /v1/history/models/{column_id}

`?capability=&difficulty=&dimension=&since=&limit=&cursor=`

The aggregate series for one column. `dimension` ∈ `quality | pass_rate |
ambiguity | latency_p50_ms | latency_p95_ms | cost_per_run_usd`, default
`quality`. Columns are catalog-global (not tenant-scoped) and retired
columns keep their history. Unknown column: `404`.

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
points flagged by the §6.5 regression scan.

### GET /v1/history/recommendations

`?prompt_hash=&capability=&since=&limit=&cursor=` — the tenant audit log,
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

`prompt_hash=` answers "how often did this exact prompt arrive and how was
it classified each time" — hashes are exact-match by design.

### GET /v1/history/recommendations/{recommendation_id}

One full decision trace (§10.2): the verbatim request (prompt excluded on
the prompt path), the classification block, every candidate considered, and
the response snapshot.

```json
{
  "id": "rec_01K...", "api_key_id": "key_01K...", "prompt_hash": null,
  "request": { "mode": "balanced", "workload": { "capability": "structured_extraction", "difficulty": 3 } },
  "classification": { "...": "..." },
  "candidates": [
    { "column_id": "col_01K...", "provider": "anthropic", "model": "claude-sonnet-4-6",
      "endpoint": "direct", "status": "survivor", "rank": 0, "v": 0.87,
      "quality": 94.1, "pass_rate": 0.91, "cost_per_run_usd": 0.0042, "latency_p95_ms": 2100,
      "leader": true, "measured": true, "clears_difficulty": true, "prefs_satisfied": true },
    { "column_id": "col_01H...", "provider": "openai", "model": "gpt-5.5",
      "endpoint": "direct", "status": "excluded", "exclusion_reason": "not_in_allowlist" }
  ],
  "response": { "...": "the §9.2 response as served..." },
  "matrix_version": "2026-07-07",
  "created_at": "2026-07-07T21:02:47Z"
}
```

Exclusion reasons (closed enum): `not_in_pool`, `not_in_allowlist`,
`standing_constraint:max_cost_per_run`, `standing_constraint:max_latency_ms`,
`status_retired`, `no_data`. Another tenant's id: `404`.

### GET /v1/history/outcomes

`?recommendation_id=&outcome=&since=&limit=&cursor=` → `{"outcomes": [...],
"next_cursor": ..., "matrix_version": ...}` with the stored outcome shape —
now also carrying `endpoint_used` (the execution backend the telemetry item
reported: `direct | vertex | bedrock | azure | openrouter`). Query semantics
are unchanged from before `POST /v1/telemetry` replaced `POST /v1/outcomes`
as the producer.

### GET /v1/history/calibration

`?capability=&since=&limit=&cursor=` — the nightly prediction-calibration
ledger (system-wide, not tenant-scoped):

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

Reads mirror writes; PUTs are **full-replace** per resource, validated
before anything is written (G4) — `200` with the stored config or `422` with
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
for ergonomics). Violations: `422 unknown_model` (id not in catalog),
`422 pool_would_be_empty` (a pool with no usable columns after retired
columns and standing constraints are applied), `422 invalid_request`.

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

Standing tenant constraints — hard filters on every request:

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
telemetry, and the engine drops any telemetry items it still receives. It
filters no model column, so it never causes `pool_would_be_empty`.

## Debug capture

### POST /v1/debug/capture-windows

```json
{ "api_key_id": "key_01K...", "confidence_below": 0.5, "duration_minutes": 60 }
```

Only `duration_minutes` is required (1 … `MAVERICK_CAPTURE_MAX_WINDOW_MINUTES`,
default 1440). While a window is active, prompt-form recommend requests
matching the window (tenant, optional key scope, optional
classification-confidence threshold) have their prompt encrypted and stored
for replay. Zero-retention tenants are refused `409
zero_retention_conflict` (G5).

`201`:

```json
{ "id": "cw_01K...", "api_key_id": null, "confidence_below": null,
  "starts_at": "2026-07-07T21:03:33Z", "ends_at": "2026-07-07T22:03:33Z",
  "created_at": "2026-07-07T21:03:33Z" }
```

### DELETE /v1/debug/capture-windows/{id}

Ends the window early. `204` on success; another tenant's window and a
nonexistent one are the same `404`.

### POST /v1/debug/replay/{capture_id}

Body optional: `{"classifier_version": "..."}` (defaults to the current
classifier; in v1 only the current version is runnable — anything else is
`422`). Decrypts the captured prompt in memory, re-runs classification,
answers:

```json
{ "original_classification": { "...": "..." },
  "replayed_classification": { "...": "..." },
  "match": true }
```

The prompt text is never returned by replay. Every replay is access-logged
(§10.4). Expired captures: `410 capture_expired`. When the classification
comparison isn't enough and you need the text itself, use the self-service
read below.

### GET /v1/debug/captures

Your own captures only — the tenant scope is unconditional in the query;
no parameter widens it. `?window_id=&limit=&cursor=` with the standard
[Pagination contract](#pagination-contract). **Never prompt text** — only
the metadata already stored alongside the ciphertext:

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

### GET /v1/debug/captures/{id}/prompt

Decrypts **your own** captured prompt in memory and returns it. The custody
model is unchanged from capture itself: the prompt only exists because you
opened a window (the window is the consent act), it is encrypted at rest
(AES-256-GCM, per-capture keys), it expires on the capture's own schedule
(window end + 72h retention by default), and **every read — including a
failed attempt — appends a `capture_access_log` row** (action `read`, actor
your API key id), so the audit trail shows exactly who read what and when.

```json
{ "id": "cap_01K...", "prompt": "...", "prompt_hash": "a3f2...",
  "recommendation_id": "rec_01K...", "classification": { "...": "..." },
  "captured_at": "2026-07-07T21:04:10Z", "expires_at": "2026-07-10T22:03:33Z" }
```

`recommendation_id` joins to `GET /v1/history/recommendations/{id}` for the
full decision trace — the trace says why, the capture says what. Another
tenant's capture id and a nonexistent id are the same `404` (no
cross-tenant existence oracle — the capture-windows DELETE convention).
Expired or hard-deleted: `410 capture_expired`.

## GET /up

Unauthenticated liveness (the Kamal health check): `200 {"status": "ok"}`
when the process reaches Postgres, `503 {"status": "unavailable"}` otherwise.

## MCP server (`/mcp` and stdio)

Two transports, same tools, same tenants, same audit trail as REST:

- **Streamable HTTP** at `POST /mcp` on the public listener, behind exactly
  the `/v1` middleware chain: `Authorization: Bearer mk_live_...`, per-key
  rate limiting, 2 MB body cap.
- **stdio** for local development:
  `maverick mcp-stdio --api-key mk_live_...` — the key is authenticated once
  at startup and the session acts as that tenant. Logs go to stderr; stdout
  is the protocol channel.

Three tools (thin wrappers over the same internal reads/pipeline — an
MCP-originated recommendation writes an identical trace row):

| Tool | Input | Returns |
|---|---|---|
| `recommend_model` | The `POST /v1/recommend` request shape verbatim: `{workload: {prompt?} \| {capability?, difficulty?, domain_tags?}, pool?, allowed_models?, mode?, constraints?}` | The `POST /v1/recommend` response object |
| `get_policy` | `{capability?, pool?}` | The `GET /v1/policies` response object |
| `list_models` | `{}` | The `GET /v1/models` response object |

Tool descriptions instruct agents to prefer declared workloads when the
capability is already known (faster; nothing leaves the session).

The server identifies as `model-maverick` / `v1`.

## SDK integration conventions

Two conventions every MM-authored component — client SDKs, gateway plugins,
the trial proxy — follows identically (integration-surfaces plan §1).
Neither is part of the engine's own request/response wire contract, so
they're documented here rather than under a numbered endpoint:

- **Model-alias grammar.** `maverick/auto` (= balanced), `maverick/quality`,
  `maverick/cost`, or the extension `maverick/<pool>:<mode>`. SDKs and
  gateway plugins parse this string themselves; it is never sent to the
  engine as a `model` value, and a concrete provider model name is never
  valid where an alias is expected. The component resolves the alias to a
  `POST /v1/recommend` call (declared or prompt form, per its configured
  privacy mode) and drives its own chain executor over the returned pick and
  fallback chain.
- **Correlation response headers.** MM-authored components that sit in the
  request path stamp `X-Maverick-Recommendation-Id` and
  `X-Maverick-Model-Used` on the response they touched, so a caller can
  correlate a provider response back to the recommendation that produced it
  without parsing a body. The engine itself does not set these headers —
  `POST /v1/recommend` already returns `recommendation_id` in the body; the
  headers are a convention the components layer on top when they're the
  ones actually proxying the call.

---

# Internal provisioning API (`/internal/v1`)

The Rails seam (spec §13). Served **only** on the internal listener
(`:8081`), never proxied. Callers are root — no tenant scoping.

**Auth:** `Authorization: Bearer <label>:<secret>` where the presented token
is one full entry of `MAVERICK_INTERNAL_TOKENS` (comma-separated
`label:secret` pairs; compare is constant-time). The matching label is
recorded as the actor in the audit log. Example:

```sh
curl -s http://localhost:8081/internal/v1/alerts \
  -H 'Authorization: Bearer ops:dev-internal-secret'
```

Every mutating call appends a structured audit entry to the process log:
`msg="internal provisioning write"`, `audit=true`, `actor=<label>`,
`action=<dotted.action>`, plus subject ids.

All 15 endpoints:

| Method | Path | Purpose |
|---|---|---|
| POST | `/internal/v1/tenants` | Create tenant |
| GET | `/internal/v1/tenants/{id}` | Tenant + config summary |
| POST | `/internal/v1/tenants/{id}/suspend` | Suspend tenant |
| POST | `/internal/v1/tenants/{id}/activate` | Reactivate tenant |
| POST | `/internal/v1/tenants/{id}/api-keys` | Issue API key |
| DELETE | `/internal/v1/api-keys/{id}` | Revoke API key |
| PUT | `/internal/v1/tenants/{id}/pools` | Replace pools (same payload/validation as `/v1/config/pools`) |
| PUT | `/internal/v1/tenants/{id}/weights` | Replace weights (as `/v1/config/weights`) |
| PUT | `/internal/v1/tenants/{id}/constraints` | Replace constraints (as `/v1/config/constraints`) |
| GET | `/internal/v1/alerts` | Operator alert feed |
| POST | `/internal/v1/columns` | Create catalog column |
| PUT | `/internal/v1/columns/{id}` | Edit column status/pricing/deprecation |
| POST | `/internal/v1/corpus/activate` | Flip the active corpus version |
| GET | `/internal/v1/captures` | List debug captures (no prompt text) |
| GET | `/internal/v1/captures/{id}/prompt` | Decrypt and read one captured prompt (audited) |

### POST /internal/v1/tenants

`{"name": "tenant-one"}` → `201` with the tenant + config summary. Creation
mints the per-tenant prompt-HMAC key (32 random bytes, never leaves the
engine) and a `default` pool containing **every `active` catalog column** at
that moment (`is_default: true`). When the catalog has no active columns the
tenant is created **without** a pool — set one with the pools PUT before the
tenant can call `/v1/recommend`.

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

`GET /internal/v1/tenants/{id}` and the suspend/activate POSTs return the
same summary shape (API keys appear with `last4` only). Suspension takes
effect immediately: the tenant's keys answer `403 tenant_suspended` on the
public listener.

### POST /internal/v1/tenants/{id}/api-keys

`{"name": "dev"}` → `201`:

```json
{ "id": "key_01K...", "key": "mk_live_KdVkJFBtDUy0xT5kVgMQWsPV4r2QUZxlLuHeo05Xuf6" }
```

The raw key appears exactly once, here. Only the hash and last4 are stored;
no endpoint can return it again.

`DELETE /internal/v1/api-keys/{id}` → `200` with the key record
(`status: "revoked"`). Rows are kept (history references them); revocation
is idempotent and takes effect on the next request.

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
  "execution_aliases": { "openrouter": "anthropic/claude-sonnet-4-6" }
}
```

`provider` ∈ `anthropic | openai | google | xai`; `endpoint` ∈ `direct |
vertex | bedrock | azure | openrouter`; `settings` defaults `{}` (identical
provider/model/endpoint/settings duplicates are rejected 422); `pricing` is
USD per million tokens. `execution_aliases` is optional, defaulting to `{}`:
an object with string values only, at most 10 keys, each value non-empty and
at most 200 characters — otherwise `422 invalid_request`. New columns always
enter the lifecycle at `status: "cold_start"` with `cold_start_until` = today +
`MAVERICK_CATALOG_COLD_START_NIGHTS` (default 5) — status is not accepted in
the create body. `201`:

```json
{ "column_id": "col_01K...", "provider": "anthropic", "model_id": "claude-sonnet-4-6",
  "endpoint": "direct", "settings": {}, "execution_aliases": { "openrouter": "anthropic/claude-sonnet-4-6" },
  "status": "cold_start",
  "pricing": { "input_per_mtok": 3.0, "output_per_mtok": 15.0 },
  "quality_source_column_id": null, "cold_start_until": "2026-07-13",
  "deprecation_date": null, "created_at": "...", "updated_at": "..." }
```

### PUT /internal/v1/columns/{id}

Partial update of `status`, `pricing`, `deprecation_date`, `execution_aliases`
(absent fields keep their stored values). `status` ∈ `cold_start | active |
deprecated | retired`. A transition to `retired` runs the §4.1 retirement
guard: if any tenant pool would drop to zero usable columns the transition is
rejected `422 pool_would_be_empty` (naming the pools), a critical alert row
is written, and the column keeps its status. Price edits replace the
list-price snapshot and log a `catalog_changes` entry — historical run costs
are never restated. `execution_aliases`, when present, **replaces the stored
map whole** (same validation as create) and also logs a `catalog_changes`
entry; there is no per-key merge.

### GET /internal/v1/alerts

`?since=&kind=` → `{"alerts": [{id, kind, severity, subject, message,
created_at, acked_at}, ...]}`. The same rows are delivered by email/webhook
per `docs/operations.md`.

### POST /internal/v1/corpus/activate

`{"version": "2026-07-07"}` — flips the active corpus version atomically
(the HTTP twin of `maverick corpus activate`). The version must already be
loaded (`maverick corpus load`); unknown versions are `404`. `200` with the
version record.

### GET /internal/v1/captures, GET /internal/v1/captures/{id}/prompt

The operator capture-read surface: judging whether a routed model was
correct for a request requires reading the actual prompt that drove the
classification, not just its fingerprint. This is the root, cross-tenant
counterpart of the tenant self-service read (`GET /v1/debug/captures`,
`GET /v1/debug/captures/{id}/prompt` — see "Debug capture" above, where a
tenant can read only its own captures), inside the same custody model as
the rest of debug capture: encrypted at rest AES-256-GCM, 72h default
expiry, every touch audited. Capture itself is unchanged, and no tenant
route ever returns another tenant's prompt text.

`GET /internal/v1/captures?window_id=&tenant_id=&limit=&cursor=` — every
capture across tenants, newest first (same `limit`/`cursor`/`next_cursor`
shape as the [Pagination contract](#pagination-contract)). **No prompt
text** — only what's already stored alongside the ciphertext:

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

`recommendation_id` is the join key into
`GET /v1/history/recommendations/{id}` for the full decision trace — nothing
is re-classified here, unlike replay. Every
attempt — a hit, an expired-but-not-yet-swept capture, a hard-deleted one, or
an unknown id — appends a `capture_access_log` row (action `read`, actor the
internal token's label), so the read itself is exactly as audited as a
replay. Expired or hard-deleted: `410 capture_expired`. Unknown id: `404`.

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

`503` with `status: "degraded"` when Postgres is unreachable: `db` reports
`error` with the failure detail, and `job_queue` reports `unknown`
(reading the queue state needs the same connection).
`provider_client_config` reports `none_configured` when no provider API key
is set. `job_queue` reflects live graphile_worker state read from Postgres, so
an `--api-only` node reports the workers running in a separate
`--worker-only` node: `no_workers` when no worker is registered, `stale` when
no worker heartbeat is newer than 60s, `ok` otherwise. (The Go engine named
this component `river_queues` and also had `paused`; see
`parity/DIVERGENCES.md` D35.) `active_matrix_version` is the corpus version active right now
(`null` on a cold stack, matching `matrix_version` elsewhere).
`last_batch_times` maps job kind to its most recent completed batch
(`batches.completed_at`; absent entries mean that kind has never completed).
