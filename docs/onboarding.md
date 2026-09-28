# Onboarding: tenant #1 on a fresh local stack

This is the complete runbook from nothing to a first recommendation. Every
command has been executed verbatim against a fresh stack; copy-paste in
order. Total time: about ten minutes.

## 0. Prerequisites

- **Go** (current stable) — the engine builds and runs with `go run`.
- **Docker** — for Postgres (and for `make test`, which uses
  testcontainers).
- The engine repository (this repo) and the private corpus repository
  checked out side by side:

  ```
  ~/src/modelmaverick          this repo — run everything from its root
  ~/src/modelmaverick-corpus   task corpus: corpus/<capability>/<task_id>.yaml
  ```

No provider API keys are required for this runbook. The stack boots, serves,
and recommends without them — the differences are called out honestly below.

## 1. Postgres

One throwaway container (host port 5433 to avoid colliding with a local
Postgres):

```sh
docker run -d --name maverick-pg \
  -e POSTGRES_USER=maverick -e POSTGRES_PASSWORD=maverick -e POSTGRES_DB=maverick \
  -p 5433:5432 postgres:16
```

Wait until it answers:

```sh
docker exec maverick-pg pg_isready -U maverick
```

## 2. Environment

Three variables are required; everything else defaults (spec §14.1, complete
list in `internal/config/config.go`). Boot fails fast and names every
missing or malformed variable.

```sh
export MAVERICK_DATABASE_URL="postgres://maverick:maverick@localhost:5433/maverick?sslmode=disable"
export MAVERICK_INTERNAL_TOKENS="ops:dev-internal-secret"
export MAVERICK_CAPTURE_MASTER_KEY="$(openssl rand -hex 32)"
```

- `MAVERICK_INTERNAL_TOKENS` is a comma-separated list of `label:secret`
  entries; the label is recorded as the actor in provisioning audit logs.
  The **whole entry** (`ops:dev-internal-secret`) is the bearer token for
  the internal API.
- `MAVERICK_CAPTURE_MASTER_KEY` must be 64 hex characters (a 32-byte key);
  it encrypts debug-capture prompts and never touches the database. Keep the
  value — captures encrypted under it are unreadable without it.

Run every following command from the engine repo root, in this shell (or
re-export the variables).

## 3. Migrate

```sh
go run ./cmd/maverick migrate
```

Applies the schema migrations and River's job-queue migrations to the same
database. Safe to run repeatedly. Ends with `"migrations complete"`.

## 4. Load and activate the corpus

Loading validates every task file and stores the version **inactive**;
activation flips it atomically. `--strict` promotes corpus-shape warnings
(task counts per capability, smoke-set membership) to errors — use it for
real corpora:

```sh
go run ./cmd/maverick corpus load \
  --version 2026-07-07 \
  --dir ../modelmaverick-corpus/corpus \
  --strict
```

Expected output (task count varies with the corpus):

```
loaded corpus version 2026-07-07: 221 tasks (inactive; run `maverick corpus activate --version 2026-07-07`)
```

Activate and confirm:

```sh
go run ./cmd/maverick corpus activate --version 2026-07-07
go run ./cmd/maverick corpus status
```

```
VERSION     TASKS  ACTIVE  CREATED
2026-07-07  221    true    2026-07-08T01:01:53Z
```

The active version is the `matrix_version` stamped on every API response.

## 5. Start the server

In a second terminal (same env vars exported):

```sh
go run ./cmd/maverick serve
```

Two listeners come up: public `:8080`, internal `:8081`, plus the River
workers. Two log lines are **expected** on a fresh stack and are not
failures:

- `classifier column unavailable; prompt-path requests will receive the
  fallback classification` — the default classifier column
  (`claude-haiku-4-5/direct`) is not in the catalog yet and no provider key
  is set. Prompt-form requests still answer (G1); see step 10.
- Nothing about provider clients — with no `MAVERICK_*_API_KEY` set, nightly
  benchmark runs would record provider errors, but the API surface is fully
  functional.

Check liveness (no auth):

```sh
curl -s http://localhost:8080/up
```

```json
{"status":"ok"}
```

## 6. Seed the model catalog

A fresh database has an **empty catalog** — migrations seed no columns (the
catalog is data, not code). Create two columns through the internal API. The
internal bearer token is the full `label:secret` entry from step 2:

```sh
AUTH='Authorization: Bearer ops:dev-internal-secret'

curl -s -X POST http://localhost:8081/internal/v1/columns \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"provider":"anthropic","model_id":"claude-sonnet-4-6","endpoint":"direct",
       "pricing":{"input_per_mtok":3.0,"output_per_mtok":15.0}}'

curl -s -X POST http://localhost:8081/internal/v1/columns \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"provider":"anthropic","model_id":"claude-haiku-4-5","endpoint":"direct",
       "pricing":{"input_per_mtok":1.0,"output_per_mtok":5.0}}'
```

Each answers `201` with a `column_id` (`col_...`) — **capture both ids**.
New columns are always born `status: "cold_start"` (spec §4.1: a dense-run
window that `catalog_sync` later closes). Tenant default pools include only
`active` columns, so for onboarding flip both to active now (in production
this transition happens automatically after the cold-start window):

```sh
curl -s -X PUT http://localhost:8081/internal/v1/columns/<SONNET_COLUMN_ID> \
  -H "$AUTH" -H 'Content-Type: application/json' -d '{"status":"active"}'

curl -s -X PUT http://localhost:8081/internal/v1/columns/<HAIKU_COLUMN_ID> \
  -H "$AUTH" -H 'Content-Type: application/json' -d '{"status":"active"}'
```

The second column is `claude-haiku-4-5/direct` deliberately: it is the
default classifier column (`MAVERICK_CLASSIFIER_COLUMN`), so once an
Anthropic key is configured the prompt-path classifier resolves without any
further setup.

## 7. Provision the tenant

```sh
curl -s -X POST http://localhost:8081/internal/v1/tenants \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"tenant-one"}'
```

`201`:

```json
{
  "tenant": { "id": "ten_01K...", "name": "tenant-one", "status": "active", "...": "..." },
  "pools": [ { "name": "default",
               "model_column_ids": ["col_01K...", "col_01K..."],
               "is_default": true } ],
  "weights": [],
  "constraints": { "max_cost_per_run": null, "max_latency_ms": null, "zero_retention": false },
  "api_keys": []
}
```

The `default` pool was minted automatically with every active catalog column
— which is why step 6 activated the columns first. (If you create a tenant
against an empty/all-cold catalog, no pool is created; fix it later with
`PUT /internal/v1/tenants/{id}/pools`.) **Capture the tenant id.**

## 8. Issue an API key

```sh
curl -s -X POST http://localhost:8081/internal/v1/tenants/<TENANT_ID>/api-keys \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"dev"}'
```

```json
{ "id": "key_01K...", "key": "mk_live_KdVkJFBtDUy0xT5kVgMQWsPV4r2QUZxlLuHeo05Xuf6" }
```

The raw key is returned **once, here, only**. Save it:

```sh
export TENANT_KEY="mk_live_..."
```

## 9. First recommendation (declared workload)

```sh
curl -s -X POST http://localhost:8080/v1/recommend \
  -H "Authorization: Bearer $TENANT_KEY" -H 'Content-Type: application/json' \
  -d '{"workload":{"capability":"structured_extraction","difficulty":3},"mode":"balanced"}'
```

`200` — this is **exactly** what a cold stack returns:

```json
{
  "classification": { "capability": "structured_extraction", "difficulty": 3,
    "domain_tags": [], "high_stakes": false, "confidence": 1, "method": "declared" },
  "recommendations": [
    { "provider": "anthropic", "model": "claude-sonnet-4-6", "endpoint": "direct",
      "column_id": "col_01K...", "score": null, "clears_difficulty": false,
      "reason": "below difficulty ceiling — best available" },
    { "provider": "anthropic", "model": "claude-haiku-4-5", "endpoint": "direct",
      "column_id": "col_01K...", "score": null, "clears_difficulty": false,
      "reason": "fallback candidate" }
  ],
  "pool": "default",
  "mode": "balanced",
  "check_spec": { "type": "json_schema", "ref": "checks/extraction/v1" },
  "constraints_met": true,
  "violations": [],
  "recommendation_id": "rec_01K...",
  "matrix_version": "2026-07-07",
  "measured": false
}
```

Read the cold-start signals honestly (G1: a recommendation is always
produced, never an error):

- `"score": null` — no benchmark aggregates exist yet, so there is no
  quality EWMA to display.
- `"measured": false` — the pick is not backed by measured cells.
- `"clears_difficulty": false` — with no pass-rate data, no column clears
  any difficulty ceiling yet.

These fields flip to real values after the first nightly batches run (which
requires provider API keys and corpus runs — see `docs/operations.md`). The
ordering on a cold stack falls back to pool order, so put your preferred
column first when hand-building pools.

## 10. First prompt-form recommendation

```sh
curl -s -X POST http://localhost:8080/v1/recommend \
  -H "Authorization: Bearer $TENANT_KEY" -H 'Content-Type: application/json' \
  -d '{"workload":{"prompt":"Extract the parties, dates, and obligations from this contract: ACME Corp agrees to deliver 100 units to Widget Inc by 2026-09-01."}}'
```

`200`. Without a configured classifier (no `MAVERICK_ANTHROPIC_API_KEY` in
this runbook), the classification block is the **fallback** — verified, this
is exactly what you get:

```json
"classification": { "capability": "multi_step_reasoning", "difficulty": 3,
  "domain_tags": [], "high_stakes": false, "confidence": 0,
  "method": "fallback_default" }
```

plus `"prompt_hash": "hmac_sha256:..."` (present only on prompt-form
requests) and the same cold-start recommendation list as step 9. The request
is degraded, not refused (G1). To get real classification
(`"method": "classified"`): set `MAVERICK_ANTHROPIC_API_KEY`, keep the
`claude-haiku-4-5/direct` column in the catalog (step 6 did), and restart
`serve`.

## 11. Check the audit trail

```sh
curl -s "http://localhost:8080/v1/history/recommendations?limit=5" \
  -H "Authorization: Bearer $TENANT_KEY"
```

Both recommendations appear newest-first with their pick and flags. Pull the
full decision trace of one — every candidate considered, with rank or
exclusion reason:

```sh
curl -s http://localhost:8080/v1/history/recommendations/<RECOMMENDATION_ID> \
  -H "Authorization: Bearer $TENANT_KEY"
```

## 12. Where to go from here

- Set provider keys (`MAVERICK_ANTHROPIC_API_KEY`,
  `MAVERICK_OPENAI_API_KEY`, `MAVERICK_GOOGLE_API_KEY`,
  `MAVERICK_XAI_API_KEY`) and let the nightly smoke batch (03:00 UTC) start
  measuring; spend caps default to $100/night and $500/week
  (`docs/operations.md`).
- The full endpoint reference, including config writes, the outcomes history
  read surface (outcomes now arrive automatically from MM-authored SDK,
  plugin, and proxy components — nothing here to call by hand), capture
  windows, and the MCP tools: `docs/api.md`.
- MCP for a local agent:
  `go run ./cmd/maverick mcp-stdio --api-key $TENANT_KEY`.

## Teardown

```sh
docker rm -f maverick-pg
```

(Stop `serve` with Ctrl-C.) The container held all state; removing it
removes everything.
