# Data model and store

`maverick-store` is the only crate with SQL. It holds the sqlx pool, the migrations, the compile-time checked queries, and the transactions. Postgres holds all state, including the job queue. There is no Redis.

## Migrations

Migrations live in `crates/maverick-store/migrations/` and run in order through the sqlx `Migrator`.

| Migration | Purpose |
|---|---|
| `0001_baseline.sql` | The frozen Go schema head (the goose migrations concatenated). sqlx checksums it, so it is never edited. |
| `0002_batches.sql` | The `batches` lease table and `model_columns.execution_aliases`. |
| `0003_model_columns_retention.sql` | Retention columns on `model_columns` and `endpoint_used` on outcomes. |
| `0004_model_columns_param_constraints.sql` | Parameter constraints on `model_columns` (D29). |
| `0005_debug_captures_capture_key_version.sql` | The capture key version, for key rotation. |
| `0006_outcomes_completed_unchecked.sql` | The `completed_unchecked` outcome value (D16). |
| `0007_tenants_classification_quota_override.sql` | A per-tenant classifier quota override. |
| `0008_classifier_usage.sql` | Daily per-tenant classifier quota and spend counters. |
| `0009_tenant_constraints_telemetry_opt_out.sql` | The telemetry opt-out flag (D39). |

`maverick migrate` runs these, then runs `graphile_worker`'s own migrations on the same database.

## The main tables

**Tenants and access**

- `tenants`: the tenant record (status, the prompt HMAC key, the classifier quota override).
- `api_keys`: public API keys (a sha256 hash, the last four characters, a status).
- `pools`, `tenant_weights`, `tenant_constraints`: the tenant's model pools, per-capability value weights and quality bar, and the max cost, max latency, zero-retention flag, and telemetry opt-out.

**The catalog and the corpus**

- `model_columns`: the model catalog. Each column is a provider, model, and endpoint with its settings, status lifecycle, pricing snapshot, retention mode, parameter constraints, and execution aliases.
- `corpus_versions`: the corpus snapshots, with exactly one active.
- `tasks`: the authored benchmark tasks (the prompt, the grader specs, the dimension weights, the pass threshold, the tier).

**The benchmark results**

- `runs`: one row per (batch, task, column, repeat) with the raw output, tokens, latency, cost, and status.
- `scores`: the per-run, per-grader, per-dimension grade values (0 to 1) with detail.
- `aggregates`: the append-only folded cells (the EWMA, the Welford variance, the count, the measured flag) keyed by (column, capability, tier, dimension, as-of). The current value is the row with the greatest as-of.
- `predictions` and `calibration`: the pre-batch locked pass probabilities per cell, and the Brier score and overconfidence per (batch, capability).

**Serving and outcomes**

- `recommendations`: the verbatim decision records (the candidates and the response, with the prompt text excluded).
- `outcomes`: the telemetry-derived outcome rows, each with a foreign key to a recommendation.
- `classification_cache`: cached prompt classifications.

**Operations**

- `batches`: the engine-owned lease and uniqueness row per (kind, batch window). See [Jobs and scheduling](jobs-and-scheduling.md).
- `classifier_usage`: the daily per-tenant classifier quota and spend counters.
- `capture_windows`, `debug_captures`, `capture_access_log`: the debug-capture subsystem, with AES-256-GCM ciphertext, a key version, and an access audit trail.
- `alerts`: operator alerts (kind, severity, subject, message, acknowledged-at).

## Compile-time checked queries and offline mode

Queries use `sqlx::query!` and `query_as!`, so the compiler checks every SQL statement against the schema. The checks read a cache in `.sqlx/` at the workspace root (over a hundred query JSON files), written by `scripts/sqlx-prepare.sh` (`cargo sqlx prepare --workspace` against a migrated database, spinning up a throwaway Postgres 16 when none is configured). CI and the scripts set `SQLX_OFFLINE=true`, so the workspace compiles with no database. `scripts/sqlx-prepare.sh --check` verifies the cache is current, and the fast tier fails when it is stale.

Store and job tests run against a real Postgres through `maverick-testdb`: one testcontainers Postgres 16 per test binary, a migrated template, and an isolated database per test.
