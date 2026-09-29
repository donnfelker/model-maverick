# Model Maverick v1 — Engineering Specification

Status: draft for review
Source plan: `plans/model-maverick-v1-plan.md`
Companion: `specs/capability-matrix-design.md` (measurement methodology: grading dimensions, judge handling, difficulty methodology, research grounding)

This document is the implementation-ready specification for the v1 Go engine. Where the plan states a decision, this spec elaborates it; where the plan is silent, this spec decides. Conflicts resolve in favor of the plan unless a section here explicitly says it refines the plan.

Requirement keywords MUST, MUST NOT, SHOULD, and MAY are used per RFC 2119.

---

## 1. Overview and invariants

Model Maverick is a recommendation engine. It benchmarks language models continuously, classifies incoming workloads, applies tenant policy, and returns a ranked list of model columns. It never executes caller inference.

### 1.1 System guarantees

These invariants hold everywhere; any implementation that violates one is wrong regardless of what any other section says.

- **G1 — Always a model.** A well-formed request from a configured tenant always returns at least one recommendation. Difficulty shortfalls and preference violations arrive as flags on a real recommendation, never as an empty response. Cold start, classifier outage, classifier quota exhaustion, and missing data degrade honestly. **Exception (privacy outranks G1):** a zero-retention tenant receives `422 zero_retention_requires_declared` when it sends the prompt form and no zero-retention classifier is configured (§8.3), and `409 no_compliant_columns` when no column in its pool holds valid zero-retention evidence (§7.1 step 3). These are the only no-model responses besides the G3 caller-bug errors. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule)
- **G2 — Pools are hard boundaries.** A request can narrow its candidate set (allowlist) but can never widen it beyond its pool. Escalation chains are pool-constrained by construction.
- **G3 — Loud caller bugs.** An `allowed_models` entry unknown to the system, or outside the caller's pool, is a hard error naming the entry. No silent dropping, no silent widening.
- **G4 — No empty configurations.** Configuration writes that would leave any pool with zero usable columns under standing constraints are rejected at write time. A model retirement that would do the same raises an operator alert instead of silently shrinking the pool.
- **G5 — Prompts are never stored by default.** Only the per-tenant HMAC fingerprint is persisted. Storage of prompt content happens only inside an explicitly enabled, time-boxed, encrypted debug-capture window, and never for zero-retention tenants. A zero-retention tenant's prompt is never sent to a classifier column, and its pick never points at a column, that lacks valid zero-retention evidence (§4.5, §7.1, §8.3). Prompt text, completions, request bodies, and credentials never appear in spans, log fields, or metric labels (§14.3). (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule)
- **G6 — Every decision is auditable.** Every response carries `recommendation_id` and `matrix_version`. The full decision trace (classification, survivors with scores, exclusions with reasons, flags) is retrievable by id for as long as the tenant retains history.
- **G7 — The corpus is private.** Task prompts, golden answers, hidden rubrics, and grader internals never leave the engine through any public API and never enter a benchmarked model's prompt beyond the task prompt itself.
- **G8 — Spend is capped.** Every benchmark job computes projected cost before starting and refuses to run past its configured ceiling. Request-time classifier spend is capped too: a per-tenant daily classification quota and a global daily classifier spend cap; past either limit a prompt-form request degrades to `fallback_default` instead of spending (§8.3). (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule)

G9 to G12 are defined in the v1.1 spec (§1.1). G13 and G14 are defined in `plans/model-maverick-integration-surfaces-plan.md` §0 and restated here because the engine contract depends on them:

- **G13 — Zero ceremony.** No public surface asks the caller to report back. Outcome data is produced only by MM-authored components (SDKs, plugins, proxy) through `POST /v1/telemetry` (§9.10). Telemetry is opt-out in every component and off means zero bytes.
- **G14 — Fail-open.** An MM component failure never breaks the customer's LLM call: last-known-good chain, then the configured static default, then the original model string. The trial proxy is exempt by positioning. **Privacy outranks G14:** for a zero-retention tenant, fallback may use only cached chain entries stamped `retention_mode: zero`, and the static default or original model only if locally configured as verified zero-retention; otherwise the call fails closed. `422 zero_retention_requires_declared` and `409 no_compliant_columns` are terminal privacy errors, never fail-open triggers. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule)

### 1.2 Non-goals for v1

No inference execution, no Rails/product shell (v1.1), no tenant-private corpora (v2), no IRT calibration or matrix completion (v1.1 — but the data they need is collected from day one), no per-endpoint quality columns (latency/cost only), no self-hosted providers (v2), no public leaderboard.

### 1.3 Vocabulary

- **Column** — a `(model, endpoint, settings)` triple; the unit that is benchmarked and recommended. Thinking on vs off is two columns.
- **Capability** — the primary workload key (10 in v1, listed in Appendix C).
- **Cell** — an `(column, capability, difficulty_tier)` aggregate.
- **Tier** — authored difficulty 1–5. Bands: easy = tiers 1–2, medium = tier 3, hard = tiers 4–5.
- **Batch** — one scheduled benchmark execution (a nightly smoke or weekly sweep instance).
- **matrix_version** — the active corpus version tag (date-stamped, e.g. `2026-07-01`), bumped whenever tasks or graders change.

---

## 2. Architecture and package layout

### 2.1 Process model

One Go module, one binary.

- Module path: `github.com/donnfelker/modelmaverick`
- Binary: `maverick`, subcommands:
  - `maverick serve` — HTTP API + MCP server + River workers in one process (default).
  - `maverick serve --api-only` / `maverick serve --worker-only` — split deployment when load demands it.
  - `maverick migrate` — run goose migrations (also runs River's migrations).
  - `maverick corpus load --version <v>` — validate and load a corpus version (§5.6).
  - `maverick mcp-stdio --api-key <key>` — stdio MCP transport for local development (§12).
- Two listeners:
  - Public: `:8080` — `/v1/*`, `/mcp`, `/up`. This is the only listener the Kamal proxy exposes.
  - Internal: `:8081` — `/internal/v1/*` (provisioning; §13) and `/health/detail`. Never proxied publicly.

### 2.2 Packages

```
cmd/maverick/            entrypoint, subcommands, wiring
internal/api/            chi routers, handlers, middleware, request/response DTOs
internal/auth/           API-key issue/verify, internal-token auth
internal/catalog/        model columns, pricing, lifecycle, retirement guard
internal/tasks/          corpus loading, task validation, corpus versioning
internal/runner/         benchmark execution: task × column × repeats
internal/graders/        grader interface + one subpackage per grader type
internal/scoring/        penalties, cell aggregation, EWMA, regression scan
internal/policy/         pools, allowlists, constraints, config-write validation
internal/recommend/      ranking, modes, bands, tie-breaks, response assembly
internal/classify/       classifier client, cache, fallback
internal/history/        history/audit query endpoints
internal/tracing/        prompt HMAC, decision traces, debug capture
internal/predictions/    prediction locking, Brier scoring
internal/jobs/           River job definitions, cron schedules, spend guard
internal/providers/      ProviderClient interface + anthropic/openai/google/xai
internal/mcp/            MCP server wrapping the REST handlers
internal/alerts/         operator alerts: email + webhook fan-out
internal/store/          sqlc-generated queries, transactions, migrations/
internal/config/         env parsing, validation, defaults
```

Rule of thumb enforced in review: `internal/api` contains no business logic; `internal/recommend` and `internal/policy` contain no SQL; everything reaches Postgres through `internal/store`.

### 2.3 Request flows

**Recommend, declared path (hot path):** auth → validate → policy filter (§7) → rank from `aggregates` (§6/§7) → persist `recommendations` row → respond. One Postgres round trip for aggregates, one insert. No provider calls. Target p95 < 50 ms intra-region.

**Recommend, prompt path:** as above, plus classifier lookup: cache hit on `(tenant, prompt_hash, classifier_version)` skips the model call; miss calls the classifier column with structured output (§8.3). The classifier call dominates latency; target p95 < 1.8 s on cache miss, inside the SDK core's 2 s recommend timeout (changed 2026-09-28 for the Rust port; the prior target was 2.5 s). The trial proxy's added time to first byte is measured separately; M6 gates on the declared, uncached-prompt, and proxy targets.

**Benchmark path (background):** cron fires a batch job → spend guard → runner fans out task × column × 3 repeats through provider clients → graders score each output → `runs` + `scores` persisted → `aggregate_rebuild` recomputes cells and EWMAs → `regression_scan` and `calibration_score` run off the fresh aggregates.

### 2.4 Dependencies

Standard library first. Approved third-party (about a dozen, per plan):

`go-chi/chi/v5`, `jackc/pgx/v5`, `sqlc` (codegen, not runtime), `pressly/goose/v3`, `riverqueue/river`, `anthropics/anthropic-sdk-go`, `openai/openai-go`, `google.golang.org/genai`, `modelcontextprotocol/go-sdk`, `oklog/ulid/v2`, plus a YAML parser for corpus files (`goccy/go-yaml`). xAI uses the OpenAI client with a custom base URL. Anything beyond this list needs a reason recorded in the commit that adds it.

---

## 3. Data model

Postgres 16+. All DDL below is normative. Migrations live in `crates/maverick-store/migrations` and are managed by sqlx. They are up only (D37). `0001_baseline.sql` equals the Go goose head (`parity/schema/schema.sql`, D21). Each later file is additive and names its spec section and D-row. graphile_worker manages its own schema (D22). (changed 2026-09-28 for the Rust port; the Go engine used goose, and River managed its own tables)

Conventions: primary keys are prefixed ULIDs stored as `text` (`ten_`, `key_`, `col_`, `pool_`, `task_` (authored slug, see below), `run_`, `rec_`, `out_`, `cw_`, `cap_`, `alr_`, `bat_`). Timestamps are `timestamptz` UTC. Money is `numeric` USD. All tables get `created_at timestamptz not null default now()`; mutable tables also get `updated_at`.

```sql
create table tenants (
  id               text primary key,            -- ten_<ulid>
  name             text not null,
  status           text not null default 'active'
                   check (status in ('active','suspended')),
  prompt_hmac_key  bytea not null,              -- 32 random bytes, generated at creation
  classification_quota_override int,            -- daily classifier calls; null = config default (§8.3)
  created_at       timestamptz not null default now(),
  updated_at       timestamptz not null default now()
);

create table api_keys (
  id          text primary key,                 -- key_<ulid>
  tenant_id   text not null references tenants(id),
  key_hash    bytea not null unique,            -- sha256(raw key)
  last4       text not null,
  name        text not null default '',
  status      text not null default 'active'
              check (status in ('active','revoked')),
  created_at  timestamptz not null default now(),
  revoked_at  timestamptz
);
create index on api_keys (tenant_id);

create table model_columns (
  id                        text primary key,   -- col_<ulid>
  provider                  text not null
                            check (provider in ('anthropic','openai','google','xai')),
  model_id                  text not null,      -- provider's identifier, e.g. 'claude-sonnet-4-6'
  endpoint                  text not null,      -- 'direct' | 'vertex' | 'bedrock' | 'azure'
  settings                  jsonb not null default '{}',  -- e.g. {"thinking": true}
  status                    text not null default 'cold_start'
                            check (status in ('cold_start','active','deprecated','retired')),
  pricing                   jsonb not null default '{}',
  -- pricing: {"input_per_mtok": 3.00, "output_per_mtok": 15.00,
  --           "currency": "USD", "as_of": "2026-07-01", "source": "list"}
  quality_source_column_id  text references model_columns(id),
  -- set on latency/cost-only columns: quality transfers from the primary
  -- endpoint column; price and latency do not (plan §Model columns)
  cold_start_until          date,               -- dense-run window end for new columns
  deprecation_date          date,               -- provider-published, when known
  retention_mode            text not null default 'unknown'
                            check (retention_mode in ('unknown','standard','zero')),  -- §4.5
  retention_verified_at     timestamptz,        -- when the evidence was last checked
  retention_valid_until     timestamptz,        -- ≤ retention_verified_at + 90 days; past it reads as 'unknown'
  retention_source          text,               -- link to the provider evidence
  execution_aliases         jsonb not null default '{}',  -- {"openrouter": "<slug>"}; openrouter spec §2.1
  execution_alias_retention jsonb not null default '{}',  -- per alias: the four retention fields (§4.5)
  param_constraints         jsonb not null default '{}',
  -- per-model parameter constraints (D29, §8.1):
  -- {"<field>": {"allowed": [...], "order": [...]}}, keyed by a surface-neutral
  -- field name such as "reasoning_effort"; {} = no constraints (Go mappings apply)
  created_at                timestamptz not null default now(),
  updated_at                timestamptz not null default now()
);
create unique index model_columns_identity
  on model_columns (provider, model_id, endpoint, md5(settings::text));

create table corpus_versions (
  version     text primary key,                 -- '2026-07-01'
  description text not null default '',
  task_count  int not null default 0,
  active      boolean not null default false,   -- exactly one active at a time
  created_at  timestamptz not null default now()
);

create table tasks (
  id                text primary key,           -- authored slug: 'task_sx_017'
  capability        text not null,
  domain_tags       text[] not null default '{}',
  difficulty_tier   smallint not null check (difficulty_tier between 1 and 5),
  difficulty_knobs  jsonb not null default '{}',
  prompt            text not null,
  attachments       jsonb not null default '[]', -- [{"name": "...", "content": "..."}]
  temperature       real not null default 0,
  max_tokens        int not null,
  grader_specs      jsonb not null,              -- array; schemas in §5.4
  dimension_weights jsonb not null,              -- {"schema_validity": 0.4, ...}, sums to 1
  pass_threshold    real not null default 0.7,
  smoke_member      boolean not null default false,
  corpus_version    text not null references corpus_versions(version),
  active            boolean not null default true,
  created_at        timestamptz not null default now(),
  updated_at        timestamptz not null default now()
);
create index on tasks (capability, difficulty_tier) where active;

create table runs (
  id               text primary key,            -- run_<ulid>
  batch_id         text not null,               -- bat_<ulid>, one per job execution
  task_id          text not null references tasks(id),
  model_column_id  text not null references model_columns(id),
  repeat_index     smallint not null,           -- 0..2
  output           text,                        -- raw model output (null on provider_error)
  tokens_in        int,
  tokens_out       int,
  latency_ms       int,
  cost             numeric(12,6),
  temperature      real not null,
  status           text not null
                   check (status in ('ok','provider_error','timeout','parse_failure')),
  error            text,
  matrix_version   text not null,
  ran_at           timestamptz not null default now()
);
create unique index on runs (batch_id, task_id, model_column_id, repeat_index);
create index on runs (task_id, model_column_id, ran_at);

create table scores (
  run_id     text not null references runs(id),
  grader_idx smallint not null,                 -- index into the task's grader_specs
  dimension  text not null,
  value      real not null check (value >= 0 and value <= 1),
  detail     jsonb,                             -- per-grader evidence (rule hits, judge text)
  primary key (run_id, grader_idx, dimension)
);

-- Append-only: one row per cell dimension per rebuild. "Current" = max(as_of).
create table aggregates (
  model_column_id  text not null references model_columns(id),
  capability       text not null,
  difficulty_tier  smallint not null,
  dimension        text not null,
  -- dimensions: 'quality', 'pass_rate', 'ambiguity',
  --             'latency_p50_ms', 'latency_p95_ms', 'cost_per_run_usd',
  --             'availability' (§6.4)
  ewma             real not null,
  variance         real not null default 0,     -- variance of nightly observations
  n                int not null,                -- observations folded into the EWMA
  measured         boolean not null,            -- n >= scoring.min_nights
  matrix_version   text not null,
  as_of            date not null,
  primary key (model_column_id, capability, difficulty_tier, dimension, as_of)
);

create table pools (
  id               text primary key,            -- pool_<ulid>
  tenant_id        text not null references tenants(id),
  name             text not null,               -- 'default', 'vertex-only', ...
  model_column_ids text[] not null,
  is_default       boolean not null default false,
  created_at       timestamptz not null default now(),
  updated_at       timestamptz not null default now(),
  unique (tenant_id, name)
);

create table tenant_weights (
  tenant_id   text not null references tenants(id),
  capability  text not null,
  w_quality   real not null,
  w_cost      real not null,
  w_latency   real not null,                    -- w_q + w_c + w_l = 1, enforced on write
  quality_bar real not null default 0.80,       -- pass-rate bar for ceilings (§6.6)
  primary key (tenant_id, capability)
);

create table tenant_constraints (
  tenant_id        text primary key references tenants(id),
  max_cost_per_run numeric(12,6),               -- null = unconstrained
  max_latency_ms   int,
  zero_retention   boolean not null default false,
  telemetry_opt_out boolean not null default false,  -- D39, §9.10
  updated_at       timestamptz not null default now()
);

create table recommendations (
  id              text primary key,             -- rec_<ulid>
  tenant_id       text not null references tenants(id),
  api_key_id      text not null references api_keys(id),
  prompt_hash     text,                         -- null on declared-only requests
  request         jsonb not null,               -- verbatim request, prompt text EXCLUDED
  classification  jsonb not null,
  candidates      jsonb not null,               -- survivors + exclusions, §10.2
  response        jsonb not null,               -- the response body as returned
  matrix_version  text not null,
  created_at      timestamptz not null default now()
);
create index on recommendations (tenant_id, created_at desc);
create index on recommendations (tenant_id, prompt_hash);

create table outcomes (
  id                 text primary key,          -- out_<ulid>
  recommendation_id  text not null references recommendations(id),
  model_used         text not null,
  outcome            text not null
                     check (outcome in ('success','parse_failure','quality_insufficient',
                                        'latency_exceeded','provider_error',
                                        'completed_unchecked','other')),  -- §9.10
  escalated_to       text,
  details            jsonb,
  endpoint_used      text not null default 'direct',  -- execution backend that served the model; openrouter spec §2.4
  created_at         timestamptz not null default now()
);
create index on outcomes (recommendation_id);

create table predictions (
  batch_id            text not null,
  model_column_id     text not null references model_columns(id),
  capability          text not null,
  difficulty_tier     smallint not null,
  predicted_pass_prob real not null,
  source              text not null default 'ewma',  -- 'ewma' now; 'matrix_completion' in v1.1
  matrix_version      text not null,
  locked_at           timestamptz not null,
  primary key (batch_id, model_column_id, capability, difficulty_tier)
);

create table calibration (
  batch_id       text not null,
  capability     text not null,
  brier_score    real not null,
  overconfidence real not null,                 -- mean(predicted) - mean(observed)
  n              int not null,
  scored_at      timestamptz not null default now(),
  primary key (batch_id, capability)
);

create table capture_windows (
  id               text primary key,            -- cw_<ulid>
  tenant_id        text not null references tenants(id),
  api_key_id       text references api_keys(id),      -- null = all keys
  confidence_below real,                        -- null = capture all; else sample low-confidence only
  starts_at        timestamptz not null,
  ends_at          timestamptz not null,        -- enforced max duration: config capture.max_window
  created_at       timestamptz not null default now()
);

create table debug_captures (
  id                 text primary key,          -- cap_<ulid>
  tenant_id          text not null references tenants(id),
  recommendation_id  text not null references recommendations(id),
  ciphertext         bytea not null,            -- AES-256-GCM(prompt)
  nonce              bytea not null,            -- 96-bit, fresh from a CSPRNG per capture
  capture_key_version int not null,             -- master-key ring version used to encrypt (§10.4)
  window_id          text references capture_windows(id) on delete set null,
  -- the window that captured the row; the only FK that is not NO ACTION, because
  -- ending a window early hard-deletes it while its captures live on
  captured_at        timestamptz not null default now(),
  expires_at         timestamptz not null
);
create index on debug_captures (expires_at);
create index on debug_captures (window_id);
create index on debug_captures (tenant_id);

create table capture_access_log (
  id          text primary key,
  capture_id  text not null,                    -- no FK: log survives hard-delete of the capture
  tenant_id   text not null,
  actor       text not null,                    -- api key id or internal token label
  action      text not null check (action in ('replay','read','delete')),
  created_at  timestamptz not null default now()
);

create table alerts (
  id          text primary key,                 -- alr_<ulid>
  kind        text not null,
  -- 'regression','parse_failure_rate','job_failure','spend_warning',
  -- 'spend_refusal','pool_would_be_empty','classifier_degraded',
  -- 'retention_expiring' (§4.5)
  severity    text not null check (severity in ('info','warning','critical')),
  subject     jsonb not null,                   -- machine-readable: column/cell/job/tenant refs
  message     text not null,
  created_at  timestamptz not null default now(),
  acked_at    timestamptz
);

create table classification_cache (
  tenant_id          text not null references tenants(id),
  prompt_hash        text not null,
  classifier_version text not null,             -- classifier column id + prompt template version
  classification     jsonb not null,
  created_at         timestamptz not null default now(),
  primary key (tenant_id, prompt_hash, classifier_version)
);

-- Classifier budget counters (§8.3, D4), shared by every API replica. Before a
-- cache-miss classifier call, an upsert increments calls only while calls is
-- below the tenant's quota; no returned row means the quota is used up. The
-- global spend cap compares the sum of spend_usd for the UTC day. New
-- 2026-09-28 for the Rust port.
create table classifier_usage (
  usage_date  date not null,                    -- UTC day
  tenant_id   text not null references tenants(id),
  calls       int not null default 0,           -- classifier calls counted against the quota
  spend_usd   numeric(12,6) not null default 0, -- provider cost of those calls
  updated_at  timestamptz not null default now(),
  primary key (usage_date, tenant_id)
);

-- One row per job kind × schedule window: the engine-owned uniqueness and
-- lease record for every job (§11.2). New 2026-09-28 for the Rust port.
create table batches (
  id                text primary key,           -- bat_..., deterministic per (kind, batch_window);
                                                 -- equals runs.batch_id for batch kinds
  kind              text not null,              -- job name from §11, e.g. 'nightly_smoke'
  batch_window      text not null,              -- the schedule window, e.g. '2026-09-28'
  status            text not null default 'running'
                    check (status in ('running','completed','skipped')),
  lease_owner       text,                       -- queue worker id holding the lease
  lease_expires_at  timestamptz,
  heartbeat_at      timestamptz,
  attempt           int not null default 1,
  completed_at      timestamptz,
  created_at        timestamptz not null default now(),
  updated_at        timestamptz not null default now(),
  unique (kind, batch_window)
);
```

The engine has 22 tables. The Go engine has twenty. The Rust port adds `batches` (the Go engine relies on River unique jobs) and `classifier_usage` (the Go engine has no classifier budget), both on 2026-09-28. The window column is named `batch_window` because `window` is a reserved word in Postgres.

Raw values live in `scores` and `aggregates`. Normalized scores are computed per query and never stored (plan §Data model). The `recommendations.response` jsonb is the exception by design: it snapshots what the caller actually saw.

---

## 4. Model catalog and columns

### 4.1 Column lifecycle

```
cold_start ──(dense-run window ends, n ≥ scoring.min_nights)──▶ active
active ──(provider announces sunset)──▶ deprecated ──(sunset date)──▶ retired
```

- **cold_start:** the column runs the full corpus nightly (not just smoke) until `cold_start_until` (creation + 5 nights, config `catalog.cold_start_nights`). Its cells report `measured: false` until each has ≥ `scoring.min_nights` (default 3) observations. Cold-start columns are recommendable only when a request's pool or allowlist leaves nothing measured; responses then carry `measured: false`.
- **deprecated:** still benchmarked, still recommendable; responses include `deprecation_date`. Operators see it in `GET /v1/models`.
- **retired:** excluded from ranking (exclusion reason `status_retired`), no longer benchmarked. **Retirement guard (G4):** before a column transitions to retired, the catalog service simulates every pool containing it; any pool that would drop to zero usable columns blocks the transition and raises a `pool_would_be_empty` critical alert naming the tenants. The transition proceeds only after the pools are amended.

### 4.2 Quality transfer

V1 measures quality on one primary endpoint per model. Secondary-endpoint columns (e.g. the Vertex or Bedrock variant) set `quality_source_column_id` to the primary column: ranking reads quality/pass-rate cells from the source column and latency/cost cells from the column itself (fed by `latency_probes` and its own pricing). If a cell is missing on the secondary column, latency falls back to the source column's value and the response is flagged `measured: false` for that entry.

### 4.3 Pricing and catalog sync

`pricing` snapshots list price per million tokens. `price_sync` and `catalog_sync` (§11) refresh pricing and statuses from provider data; both write data, never code. Price changes append to the audit trail via `updated_at` and a `catalog_changes` slog entry; historical run costs are never restated (runs store the cost computed at execution time).

### 4.4 Seed catalog

The launch catalog (~12 columns across Anthropic, OpenAI, Google, xAI) is data, not contract; the illustrative seed list lives in Appendix C and is confirmed during M0.

### 4.5 Retention status

Every column carries `retention_mode` ∈ `unknown | standard | zero`, `retention_verified_at`, `retention_valid_until`, and `retention_source` (a link to the provider evidence). Retention is a property of the execution path, so each `execution_aliases` entry carries its own `retention_mode`, `retention_verified_at`, `retention_valid_until`, and `retention_source` under the same rules, stored in `model_columns.execution_alias_retention` (`specs/model-maverick-openrouter-execution-spec.md` §2.1).

- `retention_valid_until` is at most 90 days after `retention_verified_at`; writes that exceed it are `422 invalid_request`.
- Only `zero` with an unexpired `retention_valid_until` satisfies a tenant's zero-retention constraint. `unknown` never does, and expired evidence reads as `unknown` everywhere (policy, classifier selection, responses).
- `catalog_sync` (§11) raises a `retention_expiring` warning alert 14 days before any column's or alias's `retention_valid_until`. The owner revalidates evidence quarterly through the internal column API (§13).
- If expiry or a downgrade leaves a zero-retention tenant's pool with no compliant column, requests return `409 no_compliant_columns` (§7.1 step 3); stale compliance is never assumed.

Recommend entries and `GET /v1/models` expose each column's `retention_mode` (as effective after the expiry rule). (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: it stores the tenant flag but enforces it only for debug capture, and columns carry no retention status)

---

## 5. Benchmark runner and graders

### 5.1 Runner

The runner executes a batch: a set of `(task, column)` pairs × `runner.repeats` (default 3) repeats.

- **Fan-out:** a worker pool per provider, bounded by `providers.<name>.max_concurrency` (default 8) and `providers.<name>.rpm_limit`. Repeats of the same pair run sequentially (they exist to measure variance, not to race).
- **Provider client interface:**

```go
type CompletionRequest struct {
    Model       string
    System      string
    Prompt      string            // task prompt + attachments, assembled by the runner
    Temperature float32
    MaxTokens   int
    Settings    map[string]any    // column settings, e.g. thinking
}

type CompletionResponse struct {
    Text      string
    TokensIn  int
    TokensOut int
}

type ProviderClient interface {
    Complete(ctx context.Context, req CompletionRequest) (CompletionResponse, error)
}
```

- **Retries:** 429 and 5xx retry up to 3 times with exponential backoff + jitter; context timeout per run: `runner.timeout` (default 120s; tasks MAY override upward for long-context, capped at 600s). Exhausted retries record `status: provider_error`; the repeat is not rescheduled (the variance math treats missing repeats per §6.2).
- **Latency:** wall-clock from request start to final token, recorded per run. Cost: `tokens_in × input_price + tokens_out × output_price` from the column's pricing snapshot at execution time.
- **Determinism:** temperature comes from the task (default 0). Prompts are assembled byte-identically across columns: same task prompt, same attachment order. Nothing tenant-related ever enters a benchmark prompt (G7).

### 5.2 Grader framework

Every task carries its graders as data (`grader_specs` jsonb array), validated at corpus load time. Each grader emits one or more named dimensions scored in [0,1].

```go
type GradeInput struct {
    Task   *tasks.Task
    Output string            // the model's raw text
    Run    *runner.RunMeta   // status, tokens, latency (for parse-failure handling)
}

type Grader interface {
    Kind() string                                  // 'json_schema', 'rule_scan', ...
    ValidateSpec(spec json.RawMessage) error       // called by the corpus loader
    Grade(ctx context.Context, spec json.RawMessage, in GradeInput) (map[string]float64, json.RawMessage, error)
    // returns dimension→score and a detail blob persisted to scores.detail
}
```

Registry in `internal/graders`; one subpackage per kind. A grader error (as opposed to a low score) marks the run's affected dimensions as absent and increments a grader-failure counter; it never silently scores 0.

### 5.3 Grader kinds in v1

| Kind | Class | What it does |
|---|---|---|
| `json_schema` | deterministic | Extracts the first JSON value from the output (fenced block or bare), validates against an embedded JSON Schema (draft 2020-12). Dimensions: `schema_validity` (1/0), plus optional per-field `checks` (equality, regex, numeric tolerance) scored as fraction passed. |
| `rule_scan` | deterministic | Ordered rule list over the raw output: `must_include` / `must_not_include` (literal or regex, case-folding optional), `max_length_chars` / `max_length_words`, `line_count`, `language` (expected output language code). Score = weighted fraction of rules satisfied; each rule MAY be marked `critical: true` — a failed critical rule zeroes the grader's dimensions. |
| `golden_answer` | reference | Normalized comparison against one or more accepted answers: exact, numeric (tolerance), set-equality, or regex-anchored. Dimension: `answer_correctness`. |
| `key_facts` | reference | A list of facts, each with accepted surface forms (literals/regexes); score = weighted fraction of facts found. Supports `forbidden_facts` (hallucination traps) that subtract their weight when present. Dimensions: `fact_coverage`, `hallucination_free`. |
| `judge_rubric` | judge | Cross-family judge scores rubric dimensions (§5.5). Dimensions: as named by the rubric (e.g. `idiom`, `readability`, `overall`). |
| `test_suite` | deterministic | **Reserved — deferred past v1.** The spec schema is defined (files, entrypoint, runtime image, timeout) so corpora can be authored forward-compatibly, but the v1 corpus loader REJECTS any task that uses it (`grader_unsupported`). Lands with the v1.1 multi-turn harness. In v1, `code_generation` is graded by `rule_scan` (static assertions: required signatures, imports, forbidden patterns), `key_facts`/`golden_answer` where outputs are checkable, and `judge_rubric` with the hidden reference solution supplied to the judge only. No model-generated code is ever executed in v1. |

Full JSON schemas for each spec kind live with the code (`internal/graders/*/spec.schema.json`) and are normative; the companion doc carries worked examples.

### 5.4 Grader spec envelope

```json
{
  "type": "rule_scan",
  "version": 1,
  "weight_dimensions": { "constraint_compliance": 1.0 },
  "spec": { "rules": [ { "must_include": "\\bWHERE\\b", "regex": true, "weight": 2, "critical": false } ] }
}
```

Every spec carries `type` and `version`; the loader validates `spec` against the registered schema for `(type, version)` and rejects the corpus otherwise. `weight_dimensions` maps the dimensions this grader emits onto the task's dimension names (multiplier per dimension, normally 1.0); two graders MAY feed the same task dimension, which is what makes shared-dimension ambiguity measurable (§6.3). This is the tagged-union containment strategy from the plan: versioned JSON schemas at the boundary, a small Go interface per type.

### 5.5 Judge protocol

- **Judge pool:** two columns from different families, config `judge.pool` (seed: `gemini-3.1-pro` primary, `gpt-5.5` secondary). For any graded output, the judge is the first pool member whose family differs from the graded column's family — so every output is judged cross-family, including outputs from the judges' own families. The worker resolves the pool against the catalog at startup. If it does not resolve, judge_rubric gradings fail, and the worker retries every 30 s. Once the pool resolves, gradings resume without a restart; after that the worker re-resolves every 5 min (D41). (changed 2026-09-29 for the Rust port; the Go engine resolves the pool once at boot)
- **Calls:** temperature 0, structured output. Two calls per (output, rubric), with rubric item order reversed on the second call; the per-dimension score is the mean of the two, and the absolute per-dimension disagreement feeds the ambiguity penalty (§6.3).
- **Inputs:** the judge receives the task prompt, the model output, the rubric, and any reference material (golden answer, reference solution) — reference-anchored judging. Judge prompts and rubrics are corpus data, never exposed to benchmarked models (G7).
- **Failures:** a judge call that fails after retries omits its dimensions for that run and increments the judge-failure counter; ≥10% judge failure in a batch raises a `job_failure` warning alert.
- **Selection:** the pool assignment is empirical once `critique_and_judging` aggregates exist — the operator re-points `judge.pool` at the top-ranked judging columns; monthly calibration procedure is specified in the companion doc.

### 5.6 Corpus loading

Tasks are authored as YAML files in a **private** `corpus/` directory (separate private repository; never in any public artifact). `maverick corpus load --version 2026-07-01` validates every task (schema, grader specs, weights summing to 1, tier in range, smoke-set constraints) and upserts into `tasks` under a new `corpus_versions` row; activation flips `corpus_versions.active` in one transaction. The loader enforces: 20–25 tasks per capability spanning all five tiers; exactly 5 `smoke_member` tasks per capability including at least one tier ≤2 and one tier ≥4 (the easy/hard anchors); long-context smoke tasks ≤ 32k tokens.

---

## 6. Scoring and aggregation

All constants in this section are config with the stated defaults (§14).

### 6.1 Per-run quality

Each grader emits dimensions in [0,1]. Run quality is the task's weighted sum:

```
q_run = Σ_d  w_d · s_d        (Σ w_d = 1, weights from tasks.dimension_weights)
```

A run with `status` of `provider_error` or `timeout` has no gradable output: it is **excluded from quality** (it has no `q_run`; quality measures successful outputs), counts as `pass = false` in pass rate (errors and timeouts stay in the pass-rate denominator as failures, so ceilings still penalize them), and counts against the cell's `availability` (§6.4). (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule, `q_run = 0`, which contradicted §6.2's exclusion of error repeats from the mean) A `parse_failure` run (output exists but was truncated at `max_tokens` or refused) is still graded on whatever text exists — the low scores it earns are real signal — and counts toward the column's parse-failure rate (§11).

`pass = (q_run ≥ task.pass_threshold)` — feeds pass rates, ceilings, and prediction scoring.

**High-stakes overlay (medical/legal/financial tags):** dimension weights are replaced by the overlay weighting (companion doc §4) and any autofail condition (dangerous or fabricated answer, fabricated citation) forces `q_run = 0` for that run regardless of other dimensions.

### 6.2 Repeats: variance penalty

For the `runner.repeats` (3) repeats of `(task, column)` in a batch, over the repeats that produced gradable outputs:

```
μ = mean(q_run),  σ = stddev(q_run)            (σ = 0 when only 1 gradable repeat)
task_score = max(0, μ − λ_var · σ)              λ_var = 0.5
```

Missing repeats (provider errors and timeouts) do not enter μ but are recorded; a pair with zero gradable repeats contributes no task score and counts toward the parse-failure/provider-error rates and against availability.

### 6.3 Ambiguity penalty

Separate from variance (plan §Statistical handling): when ≥2 graders score the same output on the same task dimension (via the `weight_dimensions` mapping — e.g. `faithfulness` fed by both `key_facts` and the judge), the disagreement on that single output is:

```
a_output = stddev(scores for the shared dimension on this output)
a = mean(a_output over repeats)
task_score = max(0, task_score − λ_amb · a)     λ_amb = 0.25
```

Judge order-swap disagreement (§5.5) is folded into `a_output` as two observations of the judge's dimension. Cells also aggregate `a` as the `ambiguity` dimension for observability.

### 6.4 Cell aggregation and EWMA

A batch's cell observation is the mean of its task scores:

```
x_t(cell) = mean(task_score over the cell's tasks in this batch)
```

Dimensions aggregated per cell per batch: `quality` (as above, ×100 for display; gradable runs only), `pass_rate` (fraction of task-repeats passing; provider errors and timeouts count as failures), `ambiguity`, `latency_p50_ms`, `latency_p95_ms` (percentiles over the cell's run latencies, with each timeout included as censored at the timeout value), `cost_per_run_usd` (mean), and `availability` (fraction of every scheduled repeat in the cell, including timeouts and provider errors, that returned output). (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: no `availability` dimension, and latency percentiles without censored timeouts)

EWMA per (cell, dimension), seeded by the first observation:

```
S_0 = x_0
S_t = α · x_t + (1 − α) · S_{t−1}               α = 0.3
```

One noisy night moves the smoothed score about a third of the way; three consistent nights dominate. Smoke and sweep observations feed the same EWMA stream (a sweep observation simply covers more tasks). `aggregates.variance` tracks the running variance of the x_t series (Welford), used by the regression scan. `n` counts observations; `measured = (n ≥ scoring.min_nights)`, default 3.

The nightly `aggregate_rebuild` appends new `aggregates` rows (as_of = batch date); queries read the latest `as_of` per cell. History endpoints read the series.

### 6.5 Regression detection

`regression_scan` flags a cell when either:

- smoothed drop: `S_t < max(S over trailing 7 days) − 3.0` (quality points, 0–100 scale), or
- raw shock: `x_t < S_{t−1} − max(2·√variance, 3.0)`

Flag → `regression` alert (severity warning; critical if the cell's column is in any tenant pool and the drop crosses that tenant's quality bar). Deduplication: one alert per cell per 7 days unless the drop deepens by ≥2 further points.

### 6.6 Ceilings and banded policy

For a column × capability at a tenant's quality bar `B` (default 0.80) with safety buffer `β = 0.05`:

```
ceiling = max { T ∈ 1..5 : ∀ t ≤ T, ewma_pass_rate(t) ≥ B + β }   (0 if none)
```

A column **clears** difficulty `d` iff `ceiling ≥ d`. The banded policy table (`GET /v1/policies`) maps each band (easy 1–2, medium 3, hard 4–5) to the cheapest column in the pool whose ceiling ≥ the band's top tier, where "cheapest" uses the capability-weighted cost proxy `cost_per_run_usd` EWMA averaged over the band's tiers. Unmeasured cells (`measured: false`) count as not clearing — except during a tenant's early life if *nothing* in a pool clears a band, the best available column is listed with `clears_band: false` (G1 applied to policies).

### 6.7 Display scale

`quality` EWMA is stored in [0,1] and displayed ×100 with one decimal (94.1). Displayed scores are absolute — a 94.1 means the same thing in every response; ranking normalization (§7.4) is internal and never displayed.

---

## 7. Policy engine

### 7.1 Order of operations

Given a request with resolved workload `(capability, difficulty, domain_tags)`:

1. **Pool restriction.** Resolve the pool: `request.pool` by name, else the tenant's default pool. Unknown pool name → `422 unknown_pool`. Candidates = the pool's columns minus `retired`.
2. **Allowlist.** If `allowed_models` present: each entry must resolve against the *system catalog* — an entry matching no known column is `422 unknown_model` naming the entry; an entry that resolves but matches nothing in the pool is `422 model_not_in_pool` naming the entry. Otherwise narrow candidates to the allowlist (G3). An entry matches a column when it equals the column's `model_id` (all endpoints/settings variants of that model in the pool) or the column `id` (exact).
3. **Standing constraints.** Apply the tenant's stored filters against the workload's cell aggregates: `max_cost_per_run` vs `cost_per_run_usd` EWMA, `max_latency_ms` vs `latency_p95_ms` EWMA. These are hard filters — but can never empty the set, because config writes are validated against them (§7.2). Exclusions are recorded with reason `standing_constraint:<name>`. Aggregates drift between config writes; if drift has emptied the set at request time, the least-violating columns are retained and flagged exactly like request preferences (`constraints_met: false`), and a `pool_would_be_empty` critical alert fires — G1 outranks the cost and latency filters.
   **Zero retention.** When the tenant's `zero_retention` constraint is set, every candidate whose effective `retention_mode` (§4.5) is not `zero` is excluded with reason `standing_constraint:zero_retention`. This is a privacy constraint and fails closed: it is never relaxed to satisfy G1. §7.2 validation, the retirement guard, and the nightly drift check simulate it like the other standing constraints, so an empty zero-retention set is prevented at write time; if a column's retention status changes (downgrade or expiry) and empties the set anyway, the request returns `409 no_compliant_columns`, nothing is recommended, and a `pool_would_be_empty` critical alert fires naming the tenant and pool. Least-violating relaxation of the cost and latency filters draws only from zero-compliant columns. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: it stores the flag but enforces it only for debug capture)
4. **Request preferences.** Per-request `constraints` (`max_cost_per_run`, `max_latency_ms`) **deprioritize, never drop**: candidates are stably partitioned into satisfying / violating; violators sort after satisfiers at every subsequent step. If the final pick violates any preference, the response carries `constraints_met: false` and the named violations (G1).
5. **Rank** (§7.4) and assemble the response.

Every step records excluded columns and reasons into the decision trace (§10.2).

### 7.2 Config-write validation (G4)

`PUT /v1/config/pools` and `PUT /v1/config/constraints` are validated together before commit: for every pool (as it would exist after the write), applying the standing constraints (as they would exist after the write) against *current* aggregates must leave ≥1 non-retired column. The simulation includes `zero_retention`: with it set, each pool must keep ≥1 column with valid zero-retention evidence (§4.5). Violation → `422 pool_would_be_empty` naming the pool and the constraint that empties it; nothing is written. Weight writes validate `w_q + w_c + w_l = 1` (±0.001) and `0 < quality_bar ≤ 1`.

The same simulation runs in the retirement guard (§4.1) and nightly as a drift check (aggregates move and retention evidence expires; a pool can drift toward empty): a pool within one column of empty raises a `pool_would_be_empty` warning alert. An internal column write that downgrades retention (§13) runs the simulation too and raises a `pool_would_be_empty` critical alert naming each zero-retention tenant it would empty; the write is not blocked, because retention status is evidence about the provider, not tenant configuration.

### 7.3 Score inputs

For the workload cell `(capability, difficulty)`, each candidate reads its latest aggregates: `Q` = quality EWMA ×100, `P` = pass_rate EWMA, `C` = cost_per_run_usd EWMA, `L` = latency_p95_ms EWMA, `A` = mean `availability` over the trailing 7 nights, `measured`. Quality/pass cells come from `quality_source_column_id` when set (§4.2). A candidate with no aggregate row for the cell at all is excluded with reason `no_data` — unless that would empty the survivors, in which case unmeasured candidates stay in, flagged `measured: false` (G1).

### 7.4 Ranking algorithm

Over the surviving set only (query-time normalization, plan §Policy engine):

```
nQ_i = minmax(Q_i)                    # 0..1 over survivors
nC_i = 1 − minmax(C_i)                # cheaper is better
nL_i = 1 − minmax(L_i)                # faster is better
V_i  = w_q·nQ_i + w_c·nC_i + w_l·nL_i # tenant weights for this capability
```

(minmax over a single survivor or equal values yields 1.0 for all.)

Tenant weights default per mode when the tenant has no `tenant_weights` row for the capability: quality (0.8/0.1/0.1), balanced (0.5/0.35/0.15), cost (0.25/0.6/0.15).

1. **Band.** `band` by mode: quality 0.0, balanced 1.5, cost 5.5 (quality points; config `ranking.band.<mode>`). Leaders = candidates with `Q ≥ max(Q) − band` (within the preference-satisfying partition first).
2. **Order leaders** by `V` descending.
3. **Append non-leaders** by `Q` descending — the tail of the escalation chain stays quality-ordered.
4. **Noise-band ties.** Adjacent candidates with `|Q_i − Q_j| ≤ 0.75` reorder by: cheaper `C` first; if the more expensive one would be replaced by a cross-family column while a same-family cheaper sibling is also inside the noise band, prefer the same-family downgrade (plan §Tie-breaking).
5. Preference-violating candidates (step 4 of §7.1) sort after all satisfying candidates, internally ordered by the same rules.
6. **Availability floor.** Candidates with `A` below `ranking.availability_floor` (default 0.9) rank after every candidate at or above it, internally ordered by the same rules, and carry `low_availability: true`. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: no availability floor)

Index 0 is the pick; the array is the pool-constrained fallback chain (G2).

**Chain entries carry settings.** The recommended unit is (model, settings): every entry returns its column's `settings` (reasoning effort, thinking mode) and its effective `retention_mode` (§4.5). MM-authored components (trial proxy, SDK cores) apply the entry's `settings` by default and record `settings_applied` in telemetry (§9.10); callers opt out with the alias suffix `?settings=caller` (for example `maverick/<pool>:<mode>?settings=caller`). (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: entries omit `settings` and components never apply them)

### 7.5 Flags

- `clears_difficulty` per entry: ceiling(candidate, capability, tenant bar) ≥ difficulty (§6.6). A pick below the ceiling still returns, flagged `clears_difficulty: false` (plan §Ceiling flag).
- `constraints_met` + `violations[]` on the response (§7.1 step 4).
- `measured` on the response: false if the pick's cell is unmeasured (cold start).
- `low_availability` per entry: the column's trailing 7-night availability is below the floor (§7.4 step 6).
- `zero_retention` on the response: true when the tenant's zero-retention constraint is set. SDK cores switch to declared mode on seeing it (§9.2).
- `classification_quota_exceeded` on the response: true when the classifier was skipped because a quota or spend cap was reached (§8.3).

---

## 8. Recommendation and classification

### 8.1 Workload resolution

`POST /v1/recommend` accepts exactly one of:

- **Declared:** `workload.capability` (+ optional `workload.difficulty` 1–5, `workload.domain_tags[]`). No classifier call, nothing leaves the engine (plan's privacy note). Missing difficulty defaults to 3 with `classification.difficulty_source: "default"`.
- **Prompt:** `workload.prompt` (string, ≤ 500,000 chars). Classifier path below. For a zero-retention tenant with no usable zero-retention classifier column, the prompt form is refused with `422 zero_retention_requires_declared` and the prompt never leaves the engine (§8.3). The error has `details: {"field": "workload.prompt"}`. The optional `workload.projection_version` (string, D5) belongs to the prompt form: on a declared workload it is `422 invalid_request` with `details: {"field": "workload.projection_version"}`.

Both present or both absent → `422 invalid_request`.

**Workload projection** (changed 2026-09-28 for the Rust port; the Go proxy implements the prior rule, last user message text only). MM-authored components that build a prompt-form workload from a chat request (the trial proxy and every SDK core) use one versioned projection: system/developer instructions, bounded recent conversation text (not only the last user message), the response-format schema, and tool names and descriptions, excluding binary content, truncated per §8.3. The component sends the projection version as `workload.projection_version` with the prompt form, and the engine folds it into the classification cache key (§8.3). M6 gates on multi-turn and tool-use classification fixtures.

**Projection format p1** (defined 2026-09-29 for the Rust port; `maverick-core` module `projection`; conformance vectors `crates/sdk-conformance/vectors/projection.p1.json`). The input is the chat request body as JSON: an OpenAI chat-completions body or an Anthropic Messages body. One rule set reads both shapes. A body that is not a JSON object has no projection. Keys match exactly. A value of an unexpected JSON type is ignored.

- **Text of a content value:** a string as is, or the `text` of each array element whose `type` is `text`, joined with a newline. Empty pieces are dropped. Images, audio, files, documents, base64 data, and thinking blocks add nothing.
- **Sections,** in this order: `[system]`, `[response_format]`, `[tools]`, `[conversation]`. Each is a header line and its lines. One blank line separates sections. An empty section is left out. There is no trailing newline.
- `[system]`: the top-level `system` value, then each message with role `system` or `developer`, in order.
- `[response_format]`: the first object at `response_format`, `output_config.format`, or `output_format`, when its `type` is a non-empty string other than `text`. Lines: the type; `name: <name>` and `description: <description>` when present (chat nests them under `json_schema`); `schema: <schema>`.
- `[tools]`: one line per tool that has a name, read from the element's `function` object, else its `custom` object, else the element: `<name>` or `<name>: <description>`.
- `[conversation]`: one line per turn, labeled `user: `, `assistant: `, or `tool: `. In a user message, each `tool_result` element becomes a `tool` turn in its place. An assistant turn adds the piece `tool_call <name> <arguments>` for each `tool_use` block and each `tool_calls` entry; string arguments are canonical JSON when they parse as JSON, else as is. A turn with no text is dropped. The last `user` turn (else the last turn) is always kept. Other turns are added newest first while the section stays within 16,000 characters, where a turn counts its length plus one. The first turn that does not fit ends the walk.
- Characters are Unicode scalar values. Embedded JSON values are written in RFC 8785 canonical form (JCS), so each SDK language can match the bytes with a standard library.
- An empty result has no projection, and the component sends the declared fallback workload. A result longer than 100,000 characters becomes the first 80,000, the marker `\n[...]\n`, and the last 20,000. The result then stays over `classifier.max_input_chars`, so the engine makes the same cut, caps confidence at 0.8, and records `truncated: true` (§8.3).
- A change to the output bytes is a new version. The version is part of the classification cache key.

**Declared-workload header.** Callers of the trial proxy and SDK cores MAY send `X-Maverick-Workload: <capability>[:<tier>]`; the component then sends the declared form instead of a projection. This is the recommended form for production routes. The engine itself only ever sees the declared or prompt form. The value splits at the first `:`. The capability is non-empty; the tier, when present, is ASCII digits. The engine validates both. A present but malformed header never falls back to the projection, because the caller asked that the prompt not leave the process.

**Trial proxy behavior** (adopted 2026-09-28 by owner approval for the Rust port; `parity/DIVERGENCES.md` D26 to D31; the Go proxy implements the prior rules in `reference/engine-reference.md` Part 6):

- The chain executor obeys `Retry-After` on a 429 before it advances to the next entry (D26).
- Telemetry goes through the `maverick-core` batcher: every 10 s or at 20 items, with a flush at shutdown, per tenant key. Telemetry off means zero bytes (D27).
- The proxy applies a fixed-window rate limit per tenant key. The limit is lower than the API limit (§9.1). A rejected request gets `429 rate_limited` with `Retry-After` (D28). The limit is `MAVERICK_PROXY_RATELIMIT_RPM` requests per clock minute (default 60; a value that is not a positive integer is a startup error). Hosted mode limits each tenant key; self-hosted mode has one key. The check runs after the tenant key is known and before the body is read. The response body is `{"error":{"code":"rate_limited","message":"rate limit exceeded"}}`, `Retry-After` is the seconds to the next minute (at least 1), and `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` are set as on the API. The engine and the providers are not called.
- Caller parameter values that depend on the model (for example reasoning effort tiers) are normalized to the nearest value that the routed model accepts. The accepted values are per-model constraints in the model catalog, and each chain entry carries them. When the catalog has no constraint for a field, the proxy applies the Go mappings: chat `reasoning_effort` `xhigh` to `high`, messages `output_config.effort` `xhigh` to `max` (D29). The exact rule, shared by the proxy and the SDK cores: an accepted value is kept; a value in the constraint's `order` but not accepted becomes the accepted value nearest to it by position in `order`, and on a tie the higher value wins; a value that is not in `order`, or has no accepted value in `order` to move to, is sent unchanged. A constraint that is not an object with a non-empty `allowed` list counts as no constraint, so the Go mappings apply. A field the caller did not send is never added. The conformance vectors are `crates/sdk-conformance/vectors/normalize.d29.json`.
- The proxy rejects a request body larger than 32 MiB (`MAVERICK_PROXY_MAX_BODY_BYTES`) with `413 payload_too_large`, before any engine or provider call (D38). The cap applies to the declared `Content-Length` and to the bytes read from a chunked body. The message is `request body is larger than <cap> bytes`.
- The deployment mode is fixed per deployment by `MAVERICK_PROXY_MODE`: `self-hosted` (the default, Go behavior: one configured `MAVERICK_API_KEY`, provider env keys as a credential fallback) or `hosted` (MM's shared instance: the tenant key arrives per request in `X-Maverick-API-Key`, and the provider env keys are never read) (D12, D13). Any other value is a startup error.
- Hosted tenant auth (D13): the first `X-Maverick-API-Key` value must start with `mk_live_`, as the engine requires. A missing or other value is `401` with `{"error":{"code":"unauthorized","message":"missing or invalid X-Maverick-API-Key"}}`, checked before the body is read, and the engine is not called. An engine `401` for that key maps to the same response. An engine `403 tenant_suspended` is relayed as `403` with message `tenant is suspended`. Self-hosted mode keeps Go's rule: every engine failure is `502 engine_error`.
- Provider credentials (D12): the chat surface reads the caller's key from `Authorization: Bearer`, the messages surface from `x-api-key`. The key is used only for the surface's native provider (openai, anthropic). Self-hosted mode falls back to the provider env keys for other entries. Hosted mode has no env keys, so the chain is restricted to the credential's provider. In hosted mode a caller key that starts with `sk-or-` is an OpenRouter key: it selects OpenRouter execution and is never sent to a direct provider.
- The proxy relays the engine's terminal privacy errors with their status and code, `422 zero_retention_requires_declared` and `409 no_compliant_columns`, with the engine's message. Every other recommend failure is `502 engine_error` (D17).
- A present `X-Maverick-Workload` value that is not `<capability>[:<tier>]` is `400 invalid_request`, message `X-Maverick-Workload must be <capability>[:<tier>]`. The engine is not called, and the proxy never falls back to the projection (D5).
- The proxy strips `X-Maverick-API-Key` from every upstream request. The tenant's Maverick key never reaches a provider (D30).
- OTel export uses `tracing-opentelemetry` and `opentelemetry-otlp` with W3C trace context. The content stays the same (one `maverick.recommendation` log record per settled request, and the counter `maverick_proxy_recommendations_total` by `measured` and `classification_method`). The payload bytes are not reproduced. The §14.3 redaction rules apply (D31).

### 8.2 Classification contract

`classification` in every response:

```json
{
  "capability": "structured_extraction",
  "difficulty": 3,
  "domain_tags": ["legal"],
  "high_stakes": true,
  "confidence": 0.91,
  "method": "classified"            // "declared" | "classified" | "cached" | "fallback_default"
}
```

`high_stakes` is derived: any of `medical`, `legal`, `financial` in `domain_tags`. Declared requests get `confidence: 1.0`.

### 8.3 Classifier

- **Model:** config `classifier.column` (seed: `claude-haiku-4-5`, direct endpoint), temperature 0, structured output against the JSON schema `{capability: enum(10), difficulty: 1..5, domain_tags: string[], confidence: 0..1}` enforced by a forced tool call. The system prompt (versioned with `classifier.prompt_version`) defines each capability and tier in one line each and instructs tag emission only for confident domains. The workload prompt is fenced as data with a standing instruction never to answer or execute it; the fence and the schema are part of `classifier.prompt_version`.
- **Zero-retention tenants:** the classifier call uses `classifier.zero_retention_column`, a column whose effective `retention_mode` is `zero` (§4.5), validated at boot and covered by the G4 simulations like any pool member. If it is unset, or its evidence has expired, a zero-retention tenant's prompt-form request is refused with `422 zero_retention_requires_declared` and the prompt never leaves the engine. A zero-retention tenant's prompt is never sent to `classifier.column` unless that column itself holds valid zero evidence. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: every tenant classifies on `classifier.column`)
- **Cache:** key `(tenant_id, prompt_hash, classifier_version)` where `classifier_version = <classifier column> + "/" + classifier.prompt_version`, plus `"/" + workload.projection_version` when the request carries one (§8.1). Hit → `method: "cached"`. The cache is per-tenant by construction (the hash is tenant-keyed); that duplication is accepted for privacy.
- **Timeout/fallback:** one total deadline of 1.5 s (`classifier.deadline`) covering the call, one retry, and backoff, bounded further by the caller's remaining deadline when one is propagated, so a cache miss answers inside the SDK core's 2 s recommend timeout. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: 5 s timeout plus 1 retry) On failure, G1 wins over precision: classify as `multi_step_reasoning`, difficulty 3, no tags, `confidence: 0.0`, `method: "fallback_default"`. Sustained classifier failure (>5% over 15 min) raises `classifier_degraded` (critical) — operators can then re-point `classifier.column`.
- **Resolution:** the engine resolves `classifier.column` and `classifier.zero_retention_column` against the catalog at startup. A column that does not resolve logs `classifier column unavailable; ...`. Its requests get `fallback_default`, and a zero-retention tenant's prompt-form request gets `422 zero_retention_requires_declared`. The engine retries the resolution every 30 s. Once it succeeds, the engine uses the classifier without a restart and logs INFO. After that it re-resolves every 5 min, so a catalog change (a column added, retired, or re-pointed) takes effect without a restart. With no classifier, no calls are made, so the failure-rate alert cannot fire. A classifier that stays unresolved for 5 min therefore raises `classifier_degraded` (critical, subject `{classifier_column, reason: "unresolved", unresolved_seconds, zero_retention_column}`). A later resolution writes `classifier_degraded` at `info` with `reason: "resolved"` (D41). (changed 2026-09-29 for the Rust port; the Go engine implements the prior rule: resolved once at boot, degraded until a restart)
- **Budget (G8):** each tenant has a daily classification quota (classifier calls per UTC day; config `classifier.daily_quota`, overridden per tenant by `tenants.classification_quota_override` through `PUT /internal/v1/tenants/{id}/limits`, §13), and a global daily classifier spend cap (`classifier.daily_spend_cap_usd`) applies across tenants. Cache hits never count. Past either limit, a cache-miss request makes no classifier call: it degrades to `fallback_default` exactly as on failure and the response carries `classification_quota_exceeded: true`. M6 measures classifier cost per tenant. Documentation recommends the declared form for production code paths. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: no classifier quota or cap)
- **Truncation:** prompts over `classifier.max_input_chars` (default 100,000) are classified on head 80k + tail 20k; classification confidence is capped at 0.8 and the trace records `truncated: true`.

The authored corpus doubles as labeled training data for an eventual trained classifier; when `classification_routing` ships (v1.1) its rankings pick the classifier column empirically. Nothing in this contract changes for callers.

### 8.4 Check specs

Every response includes `check_spec: {type, ref}` — a machine-readable output check a gateway can run caller-side (plan design decision 1). Refs are versioned per capability (`checks/extraction/v1`, `checks/schema_following/v1`, ...) and resolvable at `GET /v1/checks/{ref}`, returning the executable spec: a JSON Schema for extraction/schema-following capabilities, a rule list (`rule_scan` rule grammar, §5.3) otherwise. Check specs are generic per capability — they validate output *shape*, not content, and contain no corpus material (G7).

### 8.5 Reasons

Each recommendation entry carries a human-readable `reason`, generated from the trace: `"cheapest column clearing tier {d} {capability} at your quality bar in this pool"` (pick), `"escalation target"` (higher-Q non-pick), `"cross-family alternative"` (first entry from a different family than the pick), `"cheapest in pool"` (cost mode pick), `"below difficulty ceiling — best available"` (unclears pick). Exact strings are not contractual; the flags are.

---

## 9. Public API

Base: `https://api.modelmaverick.dev/v1` (placeholder, plan §API). All requests `Content-Type: application/json`, body cap 2 MB (`413 payload_too_large`). All responses include `matrix_version`. Auth: `Authorization: Bearer mk_live_...` (§9.1). Full endpoint catalog with methods/codes: Appendix A. Error envelope and codes: Appendix B.

### 9.1 Authentication and rate limiting

- Keys: `mk_live_` + 43 chars base62 (32 random bytes). Shown once at issue; stored as SHA-256. Lookup by hash; `revoked` or tenant `suspended` → `401 unauthorized` / `403 tenant_suspended`.
- Rate limit: fixed-window per key, default 300 req/min (`ratelimit.rpm`), `429 rate_limited` with `Retry-After`. Headers on every response: `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`.

### 9.2 POST /v1/recommend

Request (prompt form shown; declared form per §8.1):

```json
{
  "workload": { "prompt": "Extract the parties, dates, and obligations from this contract: ..." },
  "pool": "vertex-only",
  "allowed_models": ["claude-sonnet-4-6", "claude-opus-4-8", "gemini-3.1-pro", "gemini-3.1-flash"],
  "mode": "balanced",
  "constraints": { "max_latency_ms": null, "max_cost_per_run": null }
}
```

All fields optional except `workload`. `pool` defaults to the tenant default; `mode` ∈ quality|balanced|cost, default balanced; unknown fields → `422 invalid_request` (strict decoding).

Response `200`:

```json
{
  "classification": { "capability": "structured_extraction", "difficulty": 3,
    "domain_tags": ["legal"], "high_stakes": true, "confidence": 0.91, "method": "classified" },
  "recommendations": [
    { "provider": "anthropic", "model": "claude-sonnet-4-6", "endpoint": "vertex",
      "column_id": "col_01J...", "settings": {}, "retention_mode": "standard",
      "score": 94.1, "clears_difficulty": true, "low_availability": false,
      "reason": "cheapest column clearing tier 3 extraction at your quality bar in this pool" },
    { "provider": "anthropic", "model": "claude-opus-4-8", "endpoint": "vertex",
      "column_id": "col_01J...", "settings": {"thinking": true}, "retention_mode": "standard",
      "score": 96.3, "clears_difficulty": true, "low_availability": false, "reason": "escalation target" },
    { "provider": "google", "model": "gemini-3.1-pro", "endpoint": "vertex",
      "column_id": "col_01J...", "settings": {}, "retention_mode": "unknown",
      "score": 92.8, "clears_difficulty": true, "low_availability": false, "reason": "cross-family alternative" }
  ],
  "pool": "vertex-only",
  "mode": "balanced",
  "check_spec": { "type": "json_schema", "ref": "checks/extraction/v1" },
  "constraints_met": true,
  "violations": [],
  "prompt_hash": "hmac_sha256:9f2c...",
  "recommendation_id": "rec_01J...",
  "matrix_version": "2026-07-01",
  "measured": true,
  "zero_retention": false
}
```

`score` is the absolute displayed quality (§6.7). `prompt_hash` present only on prompt-form requests. Every entry carries its column's `settings`, its `param_constraints` after `settings` (§3, D29; the stored value verbatim, `{}` when none), and effective `retention_mode` (§4.5, §7.4), and `low_availability` (§7.4 step 6). `zero_retention` is always present; when true, SDK cores switch to declared mode and treat the tenant as zero-retention for their fail-open ladder (G14). `telemetry` follows it and is always present: `false` when the tenant has opted out of telemetry, else `true` (§9.10, D39). `classification_quota_exceeded: true` appears only when the classifier budget was hit (§8.3), as the last key of the response. The prompt form also accepts `workload.projection_version` (string, §8.1). (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: entries carry no `settings`, `param_constraints`, `retention_mode`, or `low_availability`, and responses carry no `zero_retention`) `violations` entries: `{"constraint": "max_latency_ms", "limit": 2000, "estimated": 3400}`.

### 9.3 GET /v1/policies

`GET /v1/policies?capability=structured_extraction&pool=vertex-only` (capability optional — absent returns all ten).

```json
{
  "pool": "vertex-only",
  "quality_bar": 0.80,
  "buffer": 0.05,
  "policies": [{
    "capability": "structured_extraction",
    "bands": [
      { "band": "easy", "tiers": [1,2],
        "column": { "provider": "google", "model": "gemini-3.1-flash", "endpoint": "vertex", "column_id": "col_01J..." },
        "ceiling": 3, "pass_rate": 0.94, "clears_band": true },
      { "band": "medium", "tiers": [3], "column": { "...": "..." }, "ceiling": 4, "pass_rate": 0.88, "clears_band": true },
      { "band": "hard", "tiers": [4,5], "column": { "...": "..." }, "ceiling": 5, "pass_rate": 0.83, "clears_band": true }
    ]
  }],
  "matrix_version": "2026-07-01"
}
```

### 9.4 GET /v1/models

Returns the column catalog visible to the tenant (all non-retired columns): provider, model_id, endpoint, settings, status, pricing snapshot, `deprecation_date`, `measured` (any measured cells), `quality_source_column_id`, and the retention fields (`retention_mode` as effective after expiry, `retention_verified_at`, `retention_valid_until`, `retention_source`; §4.5), then `execution_alias_retention` (each alias's effective mode; openrouter spec §2.3) and `param_constraints` (§3, D29; the stored value verbatim, `{}` when none) as the last key. No scores here — scores are per-cell and live in policies/history.

### 9.5 POST /v1/outcomes

**Superseded (2026-07-14):** `POST /v1/outcomes` was removed in favor of the internal `POST /v1/telemetry` producer (G13, zero ceremony — see `plans/model-maverick-integration-surfaces-plan.md` §1). Customers are never asked to report outcomes; outcome rows are now produced only by MM-authored SDK/plugin/proxy components as an automatic side effect of routing a request, deriving the same outcome enum from the provider status/error signal instead of a caller's own report. The `outcomes` storage and the §9.6 `GET /v1/history/outcomes` read surface are unchanged by this — only the producer changed. The rest of this subsection is retained for history.

```json
{ "recommendation_id": "rec_01J...", "model_used": "claude-sonnet-4-6",
  "outcome": "parse_failure", "escalated_to": "claude-opus-4-8", "details": {} }
```

`recommendation_id` must belong to the caller's tenant (`404 not_found` otherwise — no existence oracle across tenants). `outcome` ∈ success | parse_failure | quality_insufficient | latency_exceeded | provider_error | other. Multiple outcomes per recommendation are allowed and appended (retries, escalation chains). `201` with the stored outcome. V1 collects and exposes; blending into scores is v1.1 (plan §API).

### 9.6 History

All list endpoints paginate: `?limit=` (default 50, max 200) `&cursor=`; responses carry `next_cursor` (opaque, ULID-based).

- `GET /v1/history/models/{column_id}?capability=&difficulty=&dimension=quality&since=` — the aggregate series: `[{as_of, ewma, variance, n, measured, matrix_version, regression: false}, ...]` with regression markers.
- `GET /v1/history/recommendations?prompt_hash=&capability=&since=` — the tenant audit log (summaries: id, created_at, capability, difficulty, pick, mode, pool, flags).
- `GET /v1/history/recommendations/{recommendation_id}` — one full decision trace (§10.2).
- `GET /v1/history/outcomes?recommendation_id=&outcome=&since=` — reported outcomes. `outcome` is one of `success`, `parse_failure`, `quality_insufficient`, `latency_exceeded`, `provider_error`, `completed_unchecked`, `other`; any other value is `422 invalid_request` (`outcome "<v>" is not one of [success parse_failure quality_insufficient latency_exceeded provider_error completed_unchecked other]`). (changed 2026-09-29 for the Rust port, D40; the Go engine never stored `completed_unchecked`, so its filter and message lack it)
- `GET /v1/history/calibration?capability=&since=` — `[{batch_id, capability, brier_score, overconfidence, n, scored_at}, ...]`.

### 9.7 Tenant configuration

Reads mirror writes: `GET/PUT /v1/config/pools`, `GET/PUT /v1/config/weights`, `GET/PUT /v1/config/constraints`. PUTs are full-replace per resource, validated per §7.2, `200` with the stored config or `422` with nothing written. Pool payload:

```json
{ "pools": [ { "name": "vertex-only", "model_column_ids": ["col_01J...", "col_01K..."], "is_default": false } ] }
```

Exactly one pool must be `is_default: true`. Columns are referenced by `column_id` (config is precise; request-time `allowed_models` accepts model names for ergonomics).

### 9.8 Debug capture (tenant-facing)

- `POST /v1/debug/capture-windows` — body `{api_key_id?, confidence_below?, duration_minutes}` (≤ `capture.max_window_minutes`, default 1440). Refused `409 zero_retention_conflict` for zero-retention tenants (G5). Returns the window with `ends_at`.
- `DELETE /v1/debug/capture-windows/{id}` — end early.
- `POST /v1/debug/replay/{capture_id}` — body `{classifier_version?}` (default: current). Decrypts the captured prompt in memory, re-runs classification, returns `{original_classification, replayed_classification, match: bool}`. Never returns the prompt text. Access-logged (§10.4).

### 9.9 Health

`GET /up` → `200` when the process can reach Postgres (Kamal health check). `GET /health/detail` (internal listener only) → component statuses: db, job queue, provider client config, active matrix_version, last batch times. (changed 2026-09-28 for the Rust port, `parity/DIVERGENCES.md` D35: the component is `job_queue`, with statuses `no_workers | stale | ok`, and `unknown` when the database or the queue read fails; data comes from graphile_worker; `last_batch_times` maps job kind to the latest `batches.completed_at`. The Go engine reported `river_queues`, which also had `paused`.)

### 9.10 POST /v1/telemetry

The automatic outcome producer that replaced `POST /v1/outcomes` (G13). It is plumbing for MM-authored components, documented in SDK and plugin internals, not customer API surface; the full wire contract is in `docs/api.md`. Summary: body `{"items": [...]}`, at most 500 items; `200 {"accepted": N, "dropped": M}`; malformed or foreign items are dropped and counted, never 4xx the batch, and there is no cross-tenant existence oracle. Item fields: `recommendation_id`, `model_used`, `status` (required); optional `latency_ms`, `tokens_in`, `tokens_out`, `finish_reason`, `error_class`, `check_pass`, `endpoint_used`, and `settings_applied` (boolean: whether the component applied the entry's `settings`, §7.4).

**`check_pass` is three-valued:** `true`, `false`, or absent (no check was run). Only `true` or `false` is quality evidence.

**Outcome derivation** (server-side; first matching rule wins):

1. `error_class == "timeout"` → `latency_exceeded`
2. connect or provider error, `status == 0`, `429`, or `5xx` → `provider_error`
3. 2xx and `check_pass == false` → `parse_failure`
4. 2xx and `check_pass == true` → `success`
5. 2xx and `check_pass` absent → `completed_unchecked`
6. anything else → `other`

`completed_unchecked` counts toward availability only and never enters outcome blending (v1.1 §7). `settings_applied` is stored in the outcome row's `details`. Rows land in `outcomes` and are read through `GET /v1/history/outcomes` (§9.6). (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: any 2xx without `check_pass: false` derives `success`, and there is no `settings_applied`)

**Tenant telemetry opt-out** (added 2026-09-28 by owner decision, `parity/DIVERGENCES.md` D39). `tenant_constraints` gains `telemetry_opt_out` (boolean, default false), written through the constraints payload of `PUT /v1/config/constraints` and `PUT /internal/v1/tenants/{id}/constraints`. When it is true: every recommend response for the tenant carries `"telemetry": false` (otherwise `"telemetry": true`); the trial proxy and every SDK core send no telemetry for that tenant; and the engine accepts but drops every telemetry item from that tenant (counted as dropped, no `outcomes` row written). The proxy still routes the request (G13 holds even where MM is in the request path). The trial proxy also exports no OTel record for that tenant's requests: no `maverick.recommendation` log record, and the request is not counted in `maverick_proxy_recommendations_total` (added 2026-09-29 by owner decision, D43). A request that fails before the engine answers has no opt-out flag and is recorded. The global component switches (`MAVERICK_PROXY_TELEMETRY=off`, `MODELMAVERICK_TELEMETRY=off`) still mean zero bytes regardless of the tenant setting.

---

## 10. Tracing, audit, and debug capture

Built for the support conversation in the plan: "the system keeps returning a small model where a large one was deserved" must be resolvable from stored data alone.

### 10.1 Prompt hash

`prompt_hash = "hmac_sha256:" + hex(HMAC-SHA256(tenant.prompt_hmac_key, utf8_bytes(workload.prompt)))`

Exact bytes, no normalization — a one-character edit produces an unrelated hash (exact-match only, by design). Properties (plan §Prompt hash): one-way; untestable without the tenant key; stable per tenant, so the audit log answers "how often did this exact prompt arrive and how was it classified each time." It is also the classifier cache key. The per-tenant key is generated at tenant creation and never leaves the engine.

### 10.2 Decision trace

`recommendations.candidates` stores every column considered:

```json
[
  { "column_id": "col_01J...", "provider": "anthropic", "model": "claude-sonnet-4-6",
    "endpoint": "vertex", "status": "survivor", "rank": 0,
    "quality": 94.1, "pass_rate": 0.91, "cost_per_run_usd": 0.0042, "latency_p95_ms": 2100,
    "v": 0.87, "leader": true, "measured": true,
    "clears_difficulty": true, "prefs_satisfied": true },
  { "column_id": "col_01H...", "provider": "openai", "model": "gpt-5.5", "endpoint": "direct",
    "status": "excluded", "exclusion_reason": "not_in_allowlist" }
]
```

Exclusion reasons (closed enum): `not_in_pool`, `not_in_allowlist`, `standing_constraint:max_cost_per_run`, `standing_constraint:max_latency_ms`, `standing_constraint:zero_retention` (added 2026-09-28, §7.1 step 3), `status_retired`, `no_data`. Survivors also record `availability` and `low_availability`. The trace plus `classification`, `request` (prompt excluded: every key of `workload` that names the prompt, in any letter case, is deleted), tenant weights used, mode, and band settle Haiku-instead-of-Opus directly: Opus is either a ranked survivor with the numbers that placed it behind, or in the exclusion list with the reason it never competed.

### 10.3 Retention

Recommendations, outcomes, and traces are retained per tenant `history.retention_days` (default 365; config). A daily job prunes past retention. Aggregates and calibration are retained indefinitely (they are the product).

### 10.4 Debug capture internals

- Encryption: AES-256-GCM with a fresh 96-bit nonce from a CSPRNG per capture. Data key = HKDF(master key, salt = capture id). Ciphertext + nonce in `debug_captures`; the master key never touches the database.
- **Key ring:** the engine holds the current master key (`MAVERICK_CAPTURE_MASTER_KEY`, version `MAVERICK_CAPTURE_MASTER_KEY_VERSION`) and at most one previous key (`MAVERICK_CAPTURE_PREVIOUS_MASTER_KEY`, `version:hex`). Each capture stores `capture_key_version`. Encryption uses only the current key; decryption selects the key by the stored version. **Rotation rule:** a retired key is dropped from the ring once `capture.max_window_minutes` plus `capture.retention_hours` have passed since rotation, so a rotation neither strands live captures nor keeps a compromised key indefinitely. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: one master key, no stored version)
- Capture decision at request time: an active window matches (tenant, optional key scope, optional `confidence_below` after classification) → prompt encrypted and stored, `expires_at = window.ends_at + capture.retention_hours` (default 72h).
- Every replay/read/delete appends to `capture_access_log` (log rows survive capture deletion).
- `capture_expiry` job hard-deletes expired rows hourly.
- Zero-retention tenants: window creation refused (`409`), and the capture decision path is short-circuited by the same flag read from `tenant_constraints` — belt and suspenders (G5).

### 10.5 The durable fix

Traces showing one code path repeatedly misclassifying are corrected standing, not forensically: that path declares `capability`/`difficulty` and skips the classifier. `parse_failure` outcomes recorded automatically against the `recommendation_id` (§9.5, superseded — the producer is now `POST /v1/telemetry`) land in the same history, so the trend is visible per capability and per route (plan §Tracing). No engine feature required beyond what §8.1 and the telemetry producer already define; documentation states the pattern.

---

## 11. Jobs and scheduling

River (Postgres-backed; no Redis). All periodic jobs use River's periodic-jobs with cron expressions from config; every job is unique per `(job, batch window)` through the engine-owned `batches` claim (§11.2), not the queue's own uniqueness. UTC throughout. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: River unique jobs with `UniqueOpts{ByArgs}`, no `batches` table)

| Job | Default schedule | What it does |
|---|---|---|
| `prediction_lock` | 02:50 nightly | Snapshot EWMA-implied pass probabilities for every cell scheduled in tonight's batch → `predictions` rows (locked before results exist; hindsight impossible). |
| `nightly_smoke` | 03:00 nightly | Batch: smoke tasks (5/capability, easy+hard anchors) × all non-retired columns (cold-start columns run the *full* corpus) × 3 repeats. |
| `weekly_sweep` | Sun 04:00 | Batch: full active corpus × all non-retired columns × 3 repeats, full-length long-context tasks. |
| `latency_probes` | hourly | Thin pings (fixed 1k-token prompt, 100-token response) for columns with `quality_source_column_id` set; feeds their latency cells only. |
| `aggregate_rebuild` | chained after each batch | Recompute cell observations for the batch, fold into EWMAs, append `aggregates` rows, update `measured`. |
| `regression_scan` | chained after rebuild | §6.5; writes `alerts`. |
| `calibration_score` | chained after rebuild | Brier + overconfidence per capability vs the locked predictions → `calibration`. |
| `price_sync` | 06:00 daily | Refresh pricing snapshots (provider pricing data/config). |
| `catalog_sync` | 06:10 daily | Refresh model statuses/deprecations; runs the pool-drift check (§7.2), including retention expiry; raises `retention_expiring` 14 days before any `retention_valid_until` (§4.5). |
| `capture_expiry` | hourly | Hard-delete expired `debug_captures`. |
| `history_prune` | 05:00 daily | Enforce `history.retention_days` (§10.3). |

**Chaining:** `nightly_smoke` and `weekly_sweep` enqueue `aggregate_rebuild` on completion, which enqueues `regression_scan` + `calibration_score`. A batch that partially fails (some runs errored) still rebuilds from what completed; the job records completion percentage and alerts (`job_failure`, warning) under 90%. The failure tail (alert row plus the follow-up rebuild job) is written in the same transaction that records the batch outcome, before the handler returns; no detached task carries durable work.

**Spend guard (G8):** before executing, every batch computes

```
projected = Σ_{task×column×repeats} (input_tokens_est(task) × p_in + task.max_tokens × p_out)
```

with `input_tokens_est = ceil(chars(prompt+attachments)/4) + overhead(600)`. If `projected > cap` (`spend.nightly_cap_usd` default 100, `spend.weekly_cap_usd` default 500), the job refuses, records a `spend_refusal` critical alert with the projection, and does not run. At 80% of cap it runs and emits `spend_warning`. Actual spend accumulates per batch in the job's record for the plan's M0 validation.

**Parse-failure watch:** `regression_scan` also computes each column's batch parse-failure rate; `rate > 5%` **and** `> 2× the column's trailing 7-day rate` → `parse_failure_rate` alert (leading indicator of caller-side silent escalation, plan §Monitoring).

### 11.1 Alert delivery

`internal/alerts` fans out `alerts` rows at insert: email (SMTP config) and operator webhook (`alerts.webhook_url`, HMAC-signed with `alerts.webhook_secret`, 3 retries). Delivery failure logs loudly but never blocks the writing transaction. Tenant-facing webhooks are v1.1 (plan).

### 11.2 Job uniqueness and leases

Added 2026-09-28 for the Rust port (the Go engine implements the prior rule: River unique jobs covering running and completed jobs). The queue's own dedupe key cannot stop a re-enqueue after a completed job was deleted from starting a second execution, so the engine owns per-window uniqueness through the `batches` table (§3):

- **Claim.** Each handler's first transaction takes a lease on the `batches` row for `(kind, batch_window)`: insert it if absent (deterministic `id`, `status = 'running'`, `lease_owner` = the queue worker id), or take it over if its lease has expired.
- **Outcomes of the claim.** A `completed` or `skipped` row makes the handler a no-op. A row whose lease is live and held by a different worker that is still alive makes the handler complete as a no-op (never fail, which would burn `max_attempts` into a permanent failure); nothing is stranded, because the holder's own job stays locked and the queue recovers it if the holder dies. A row whose owner is gone (the queue recovered the job from that worker) is taken over immediately, even before `lease_expires_at`. A stale `running` row is taken over and the batch resumes under the same `id` from its existing `runs`; `attempt` increments on each takeover.
- **Heartbeat.** The holder renews `lease_expires_at` and `heartbeat_at` well inside the expiry. Startup rejects any configuration where the lease TTL is not shorter than the queue's earliest stale-job recovery threshold.
- **Enqueue.** Jobs are enqueued with the queue's job key `<kind>:<batch_window>` as a first filter only. `max_attempts` is set per kind: batch jobs 3, chained and hourly jobs 5.
- **Idempotency.** Delivery is at-least-once, so every handler is idempotent: a batch resumes by skipping (column, task, repeat) triples that already have a `runs` row, and the rebuild is keyed by batch.
- **Timeouts.** Batch jobs get a long, configurable, sanity-capped timeout (hours); chained and hourly jobs a short one; each handler runs under its own per-kind timeout.
- A spend refusal (§11) records the window as `skipped`.

Acceptance: `kill -9` of the worker mid-batch (including queue recovery firing while the lease is still live), a restart, an enqueue while the batch runs, an enqueue after it completed, and a handler timeout each end with no permanently failed jobs, exactly one completed batch, one rebuild, no duplicate persisted runs or aggregate contributions, and the correct alerts. Duplicate provider calls remain possible when a crash lands between a successful call and its `runs` commit; the spend guard's per-batch accounting bounds them.

---

## 12. MCP server

Official Go MCP SDK. Two transports:

- **Streamable HTTP** at `/mcp` on the public listener — production. Auth: the same `Authorization: Bearer mk_live_...` header, resolved to a tenant exactly like REST.
- **stdio** via `maverick mcp-stdio --api-key ...` — local development convenience.

Three tools, thin wrappers over the same internal services as REST (same auth, same responses, same audit trail — an MCP-originated recommendation writes a `recommendations` row identically):

| Tool | Input schema (JSON) | Returns |
|---|---|---|
| `recommend_model` | `{workload: {prompt?} \| {capability?, difficulty?, domain_tags?}, pool?, allowed_models?, mode?, constraints?}` — mirrors §9.2 | The §9.2 response object |
| `get_policy` | `{capability?, pool?}` | The §9.3 response object |
| `list_models` | `{}` | The §9.4 response object |

Tool descriptions instruct agents to prefer declared workloads when they already know the capability (faster, nothing leaves the session).

---

## 13. Internal provisioning API

The Rails seam, existing before Rails does (plan M5). Internal listener only (`:8081`), never proxied.

**Auth:** `Authorization: Bearer <token>` where token ∈ `MAVERICK_INTERNAL_TOKENS` (comma-separated list to allow rotation; each entry `label:secret`, label recorded as actor in logs). Constant-time compare. No tenant scoping — internal callers are root.

| Endpoint | Purpose |
|---|---|
| `POST /internal/v1/tenants` | Create tenant: `{name}` → tenant with generated HMAC key, default pool (all active columns), default constraints (none), suspended=false. |
| `GET /internal/v1/tenants/{id}` | Tenant + config summary. |
| `POST /internal/v1/tenants/{id}/suspend` / `/activate` | Toggle status. |
| `POST /internal/v1/tenants/{id}/api-keys` | Issue key: `{name}` → `{id, key}` — raw key returned once. |
| `DELETE /internal/v1/api-keys/{id}` | Revoke. |
| `PUT /internal/v1/tenants/{id}/pools` `/weights` `/constraints` | Same payloads and validation as §9.7 (G4 applies). |
| `GET /internal/v1/alerts?since=&kind=` | Operator alert feed (also delivered per §11.1). |
| `POST /internal/v1/columns` / `PUT /internal/v1/columns/{id}` | Catalog administration: create column, edit status/pricing/retention fields (retirement guard §4.1 enforced; retention rules §4.5, downgrade simulation §7.2). |
| `PUT /internal/v1/tenants/{id}/limits` | Operator override of per-tenant limits: `{classification_daily_quota?}` (null clears the override; response echoes effective values). v1.1 §4.2 adds `rpm` and `history_retention_days` to the same endpoint and entitlement sync writes it from M8. (added 2026-09-28 for the Rust port) |
| `POST /internal/v1/corpus/activate` | Flip active corpus version (after `maverick corpus load`). |

All provisioning writes append structured audit log entries (slog, `audit=true`).

---

## 14. Configuration, deployment, observability

### 14.1 Configuration

Env vars, prefix `MAVERICK_`, parsed and validated at boot (fail fast, print the offending var). Selected catalog (full table maintained beside `internal/config`):

| Var | Default | Purpose |
|---|---|---|
| `MAVERICK_DATABASE_URL` | — (required) | Postgres DSN |
| `MAVERICK_PUBLIC_ADDR` / `MAVERICK_INTERNAL_ADDR` | `:8080` / `:8081` | Listeners |
| `MAVERICK_INTERNAL_TOKENS` | — (required) | `label:secret,...` |
| `MAVERICK_CAPTURE_MASTER_KEY` | — (required) | 32-byte hex; capture encryption (current key of the §10.4 ring) |
| `MAVERICK_ANTHROPIC_API_KEY` etc. | — | Provider credentials (one per provider; endpoint variants configured per column) |
| `MAVERICK_ANTHROPIC_BASE_URL` etc. | the provider's default | Provider API base URL, `http://` or `https://`. The engine reads no ambient SDK variables (D23), so this is the only way to move a provider: a gateway, or the e2e fake provider (added 2026-09-29 for the Rust port) |
| `MAVERICK_CLASSIFIER_COLUMN` | `claude-haiku-4-5/direct` | §8.3 |
| `MAVERICK_CLASSIFIER_ZERO_RETENTION_COLUMN` | unset | §8.3; must be a column with valid zero-retention evidence |
| `MAVERICK_CLASSIFIER_DEADLINE` | `1.5s` | §8.3 total classifier deadline, retry included |
| `MAVERICK_CLASSIFIER_DAILY_QUOTA` / `..._DAILY_SPEND_CAP_USD` | `1000` / `5.00` (placeholders set by the owner 2026-09-28; retune from M6 cost measurement) | §8.3 per-tenant default quota / global cap (G8) |
| `MAVERICK_CAPTURE_MASTER_KEY_VERSION` / `MAVERICK_CAPTURE_PREVIOUS_MASTER_KEY` | `1` / unset | §10.4 key ring |
| `MAVERICK_RANKING_AVAILABILITY_FLOOR` | `0.9` | §7.4 step 6 |
| `MAVERICK_JUDGE_POOL` | `gemini-3.1-pro/direct,gpt-5.5/direct` | §5.5 |
| `MAVERICK_SPEND_NIGHTLY_CAP_USD` / `..._WEEKLY_CAP_USD` | `100` / `500` | G8 |
| `MAVERICK_SCORING_ALPHA` | `0.3` | EWMA α |
| `MAVERICK_SCORING_LAMBDA_VAR` / `..._LAMBDA_AMB` | `0.5` / `0.25` | Penalties |
| `MAVERICK_RANKING_BAND_QUALITY/BALANCED/COST` | `0/1.5/5.5` | Mode bands |
| `MAVERICK_RANKING_NOISE_BAND` | `0.75` | Tie threshold |
| `MAVERICK_CEILING_BUFFER` | `0.05` | §6.6 β |
| `MAVERICK_SCORING_MIN_NIGHTS` | `3` | measured flag |
| `MAVERICK_CATALOG_COLD_START_NIGHTS` | `5` | §4.1 |
| `MAVERICK_RATELIMIT_RPM` | `300` | §9.1 |
| `MAVERICK_HISTORY_RETENTION_DAYS` | `365` | §10.3 |
| `MAVERICK_SMTP_URL`, `MAVERICK_ALERTS_WEBHOOK_URL`, `MAVERICK_ALERTS_WEBHOOK_SECRET` | — | §11.1 |
| `MAVERICK_CRON_NIGHTLY`, `MAVERICK_CRON_WEEKLY`, ... | §11 defaults | Schedules |

### 14.2 Deployment

Multi-stage Docker build → distroless static image (CGO off). Kamal to Hetzner (plan): one app server runs `maverick serve`; Postgres as a Kamal accessory (or managed PG) — v1 is a single-node system by design; horizontal API scaling works stateless once needed (`--api-only` replicas + one `--worker-only`). Kamal healthcheck: `GET /up`. The internal listener is reachable only over the host's private network (Kamal `options: --network`, no proxy route). A fresh database can start the engine before the catalog lists the classifier columns or the judge pool. The engine picks them up within 30 s of their creation, with no restart (D41, §8.3).

### 14.3 Observability

slog JSON to stdout. Every request logs: method, path, status, ms, tenant_id, key_id, recommendation_id (when created). Every batch logs: batch_id, projected vs actual spend, runs ok/error counts, duration. Metrics endpoints: deferred (plan: "metrics can wait") — the alerts table plus logs are v1's operational surface. The engine and the trial proxy MAY export OTel traces, logs, and metrics to the ops-box Collector over the private network (never a public metrics endpoint), with W3C trace context propagated from proxy or SDK through the engine to the provider call; export is best-effort and fail-open. The trial proxy exports only its `maverick.recommendation` log record and the `maverick_proxy_recommendations_total` counter, and neither for a tenant that opted out of telemetry (§9.10, D43).

**Telemetry redaction (a G5 control).** Prompt text, completions, request bodies, and credential headers (`Authorization`, `x-api-key`, `X-Maverick-API-Key`) are never span attributes, log fields, or metric labels; fingerprints only. Instrumentation records an explicit allowlist of fields, never every argument. Engine and proxy spans go to a Collector pipeline that does not export GenAI content to Langfuse. Acceptance: a prompt-form recommend and a proxy request each carrying a canary string, after which the canary appears in no exported span, log line, or metric label. (changed 2026-09-28 for the Rust port; the Go engine exports no OTel)

---

## 15. Testing strategy

- **Golden tests** (bulk of coverage): graders, scoring math, and the ranking algorithm run against checked-in fixture files (`testdata/`): input output-text + spec → expected dimensions; batch runs → expected EWMAs; candidate sets → expected order and flags. Every bug found in M6 dogfooding lands as a fixture.
- **Property tests** where the invariants are algebraic: normalization bounds, "G1: survivors never empty after §7.1 on validated config", band monotonicity, EWMA convergence.
- **Store tests:** sqlc queries against real Postgres (dockertest/testcontainers), covering migration freshness and every query.
- **API contract tests:** httptest against the real router with a seeded store — one test per endpoint per error code in Appendix B; response shapes asserted against the JSON in §9 (these tests are the contract).
- **Fake providers:** `internal/providers/fake` returns deterministic canned outputs per (model, prompt-hash) — powers the end-to-end test: load a 3-task mini-corpus, run a batch, rebuild aggregates, hit `/v1/recommend`, assert pick + trace + flags. No network in CI, ever.
- **Classifier/judge fakes:** recorded structured outputs; a nightly *live* smoke test (opt-in, spend-capped) is the only test allowed to touch real providers.
- **Privacy tests:** a declared request cannot select a `standard` or `unknown` column for a zero-retention tenant; neither classification nor recommendation sends a zero-retention tenant's prompt or pick to an unverified endpoint; retention expiry yields `409 no_compliant_columns` rather than stale compliance; the §14.3 canary test.
- **Job tests:** the §11.2 acceptance scenarios against real Postgres.
- **CI:** `go vet`, `golangci-lint`, `go test -race ./...`, sqlc drift check (`sqlc diff`), migration up/down.

---

## Appendix A — Endpoint catalog

| Method | Path | Auth | Success | Notable errors |
|---|---|---|---|---|
| POST | `/v1/recommend` | key | 200 | 422 invalid_request / unknown_model / model_not_in_pool / unknown_pool / zero_retention_requires_declared, 409 no_compliant_columns, 413, 429 |
| POST | `/v1/telemetry` | key | 200 | 422 invalid_request (malformed body only; items are best-effort, §9.10) |
| GET | `/v1/policies` | key | 200 | 422 invalid_request |
| GET | `/v1/models` | key | 200 | — |
| GET | `/v1/checks/{ref}` | key | 200 | 404 not_found |
| POST | ~~`/v1/outcomes`~~ | key | — | Superseded 2026-07-14 (§9.5): removed in favor of the internal `POST /v1/telemetry` producer (G13) |
| GET | `/v1/history/models/{column_id}` | key | 200 | 404 not_found |
| GET | `/v1/history/recommendations` | key | 200 | — |
| GET | `/v1/history/recommendations/{id}` | key | 200 | 404 not_found |
| GET | `/v1/history/outcomes` | key | 200 | — |
| GET | `/v1/history/calibration` | key | 200 | — |
| GET/PUT | `/v1/config/pools` | key | 200 | 422 pool_would_be_empty / unknown_model |
| GET/PUT | `/v1/config/weights` | key | 200 | 422 invalid_request |
| GET/PUT | `/v1/config/constraints` | key | 200 | 422 pool_would_be_empty |
| POST | `/v1/debug/capture-windows` | key | 201 | 409 zero_retention_conflict, 422 |
| DELETE | `/v1/debug/capture-windows/{id}` | key | 204 | 404 |
| POST | `/v1/debug/replay/{capture_id}` | key | 200 | 404, 410 capture_expired |
| GET | `/up` | none | 200 | 503 |
| — | `/mcp` | key | — | MCP protocol |
| * | `/internal/v1/*` | internal token | — | §13 |

### Appendix B — Error envelope and codes

```json
{ "error": { "code": "model_not_in_pool", "message": "allowed_models entry 'gpt-5.5' is not in pool 'vertex-only'",
             "details": { "entry": "gpt-5.5", "pool": "vertex-only" } } }
```

| HTTP | code | Meaning |
|---|---|---|
| 401 | `unauthorized` | Missing/invalid/revoked key |
| 403 | `tenant_suspended` | Valid key, suspended tenant |
| 404 | `not_found` | Resource absent or other-tenant |
| 409 | `zero_retention_conflict` | Capture window vs zero-retention config |
| 409 | `no_compliant_columns` | Zero-retention tenant, and no column in the pool holds valid zero-retention evidence (§7.1 step 3) |
| 410 | `capture_expired` | Replay on hard-deleted capture |
| 413 | `payload_too_large` | Body > 2 MB |
| 422 | `invalid_request` | Malformed/unknown fields, bad enums, bad weights |
| 422 | `unknown_model` | allowed_models entry not in system catalog (names entry) |
| 422 | `model_not_in_pool` | allowed_models entry outside caller's pool (names entry) |
| 422 | `unknown_pool` | Pool name not configured for tenant |
| 422 | `pool_would_be_empty` | Config write would empty a pool (names pool + constraint) |
| 422 | `zero_retention_requires_declared` | Zero-retention tenant sent the prompt form and no zero-retention classifier is configured (§8.3) |
| 422 | `grader_unsupported` | Corpus load: reserved grader type (test_suite) in v1 |
| 429 | `rate_limited` | Per-key limit; Retry-After set |
| 500 | `internal_error` | Bug; recommendation_id-free, logged with request id |

The four 422 policy codes are contractual (G3): callers are expected to branch on them. The two privacy codes, `422 zero_retention_requires_declared` and `409 no_compliant_columns` (added 2026-09-28 for the Rust port), are contractual too: they are the G1 exception, and SDK cores treat them as terminal, never as fail-open triggers (G14).

### Appendix C — Capabilities and illustrative seed catalog

**Capabilities (v1, fixed):** `code_generation`, `structured_extraction`, `summarization_synthesis`, `multi_step_reasoning`, `instruction_schema_following`, `long_context_qa`, `factual_qa`, `rewriting_editing`, `translation_multilingual`, `critique_and_judging`.

**Seed columns — illustrative data, confirmed at M0; the catalog is data, not code:**

| Provider | model_id | Endpoint | Settings | Notes |
|---|---|---|---|---|
| anthropic | claude-opus-4-8 | direct | `{}` | |
| anthropic | claude-opus-4-8 | direct | `{"thinking": true}` | separate column |
| anthropic | claude-sonnet-4-6 | direct | `{}` | |
| anthropic | claude-haiku-4-5 | direct | `{}` | also classifier seed |
| openai | gpt-5.5 | direct | `{}` | judge pool member |
| openai | gpt-5.5 | direct | `{"reasoning": "high"}` | separate column |
| openai | gpt-5.4 | direct | `{}` | |
| openai | gpt-5.4-mini | direct | `{}` | |
| google | gemini-3.1-pro | direct | `{}` | judge pool member |
| google | gemini-3.1-flash | direct | `{}` | |
| xai | grok-4 | direct | `{}` | via OpenAI-compatible API |
| anthropic | claude-sonnet-4-6 | vertex | `{}` | latency/cost only; quality_source → sonnet/direct |

Smoke-set arithmetic at this catalog (plan M0 validation): 50 tasks × 12 columns × 3 repeats ≈ 1,800 runs/night, $25–$80/night depending on token mix; weekly sweep 4–5×.

---

*End of specification. Measurement methodology — grading dimensions per capability, the high-stakes overlay, difficulty tiers, judge bias handling, statistical grounding, corpus authoring — lives in `specs/capability-matrix-design.md`.*
