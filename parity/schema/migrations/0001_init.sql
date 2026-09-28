-- +goose Up

create table tenants (
  id               text primary key,            -- ten_<ulid>
  name             text not null,
  status           text not null default 'active'
                   check (status in ('active','suspended')),
  prompt_hmac_key  bytea not null,              -- 32 random bytes, generated at creation
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
  --             'latency_p50_ms', 'latency_p95_ms', 'cost_per_run_usd'
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
                                        'latency_exceeded','provider_error','other')),
  escalated_to       text,
  details            jsonb,
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
  nonce              bytea not null,
  captured_at        timestamptz not null default now(),
  expires_at         timestamptz not null
);
create index on debug_captures (expires_at);

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
  -- 'spend_refusal','pool_would_be_empty','classifier_degraded'
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

-- +goose Down

drop table classification_cache;
drop table alerts;
drop table capture_access_log;
drop table debug_captures;
drop table capture_windows;
drop table calibration;
drop table predictions;
drop table outcomes;
drop table recommendations;
drop table tenant_constraints;
drop table tenant_weights;
drop table pools;
drop table aggregates;
drop table scores;
drop table runs;
drop table tasks;
drop table corpus_versions;
drop table model_columns;
drop table api_keys;
drop table tenants;
