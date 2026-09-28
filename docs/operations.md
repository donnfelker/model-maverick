# Operations

Running Model Maverick day to day: alerts, spend, the corpus lifecycle,
catalog administration, capture hygiene, retention, backups, the job
schedule, and deployment. Endpoint shapes referenced here are in
`docs/api.md`; variable defaults are authoritative in
`internal/config/config.go`.

## Alerts

Alert rows are the durable operational signal. Every row is written to the
`alerts` table first, then fanned out to the configured destinations —
delivery failure logs loudly but never blocks or fails the write that
created the row.

### Kinds

| `kind` | Severity | Written by | Meaning |
|---|---|---|---|
| `spend_refusal` | critical | batch spend guard | Projected batch spend would exceed a cap; the batch **did not run**. Subject carries the projection, spent-so-far, and caps. |
| `spend_warning` | warning | batch spend guard | Batch ran but night/week total reached ≥80% of a cap. |
| `job_failure` | warning / critical | batch + rebuild jobs | Batch completion below 90% (warning), a mid-batch failure, or an aggregate-rebuild failure (critical). |
| `regression` | warning / critical | `regression_scan` | A cell regressed per §6.5 (smoothed drop >3.0 points vs the trailing 7-day max, or a raw shock). Critical when the cell falls below a quality bar it previously cleared; deduped per cell per 7 days, re-armed if the drop deepens ≥2 points. |
| `parse_failure_rate` | warning | `regression_scan` | A column's batch parse-failure rate >5% **and** >2× its trailing 7-day rate — the leading indicator of caller-side silent escalation. |
| `classifier_degraded` | critical | classify resolver | >5% of classify calls failing over 15 minutes (min 20 samples); fires once per episode and re-arms on recovery. Prompt requests receive fallback classifications while degraded. |
| `pool_would_be_empty` | critical | retirement guard / drift check / recommend | A retirement was blocked (§4.1), the nightly drift check found a pool with no usable columns for some cell, or a request-time evaluation detected drift. Subject names the tenants and pools. |

Read the feed at `GET /internal/v1/alerts?since=&kind=` (internal listener,
bearer token from `MAVERICK_INTERNAL_TOKENS`).

### Delivery

Configured entirely by environment (both optional; unset means that channel
is off — the rows still land in the table):

- **Email** — `MAVERICK_SMTP_URL`:

  ```
  smtp://user:pass@mail.example.com:587/?from=alerts@example.com&to=ops@example.com,oncall@example.com
  ```

  Port defaults to 587; STARTTLS is used when the server offers it;
  credentials authenticate PLAIN after the upgrade. One plain-text mail per
  alert, subject `[maverick] <severity>: <kind>`.

- **Operator webhook** — `MAVERICK_ALERTS_WEBHOOK_URL` +
  `MAVERICK_ALERTS_WEBHOOK_SECRET` (the secret is **required** when the URL
  is set; unsigned webhooks are a boot error). Each alert is POSTed as JSON:

  ```json
  { "id": "alr_01K...", "kind": "spend_refusal", "severity": "critical",
    "subject": { "...": "..." }, "message": "...",
    "created_at": "2026-07-08T03:00:12.345Z" }
  ```

  with header `X-Maverick-Signature: sha256=<hex>` — the HMAC-SHA256 of the
  **raw request body** keyed with the secret. Verify over the exact bytes
  received, constant-time compare:

  ```python
  import hmac, hashlib

  def verify(secret: bytes, body: bytes, header: str) -> bool:
      expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
      return hmac.compare_digest(expected, header)
  ```

  Any non-2xx response is a failed attempt.

Retry policy (both channels, per destination, independent): 1 initial
attempt + 3 retries, exponential backoff 1s/2s/4s, 30s per attempt.
Deliveries run in the background; shutdown drains in-flight deliveries.

Request-time writers (sustained classifier failure, the recommend-path drift
alert, a blocked retirement) fan out through the same notifying store as
background jobs — insert and delivery are coupled everywhere an alert row is
written, so no critical signal is ever merely stored-and-logged.

## Spend caps (G8)

Before executing, every batch (`nightly_smoke`, `weekly_sweep`) projects its
cost:

```
projected = Σ over task × column × repeats of
            ( input_tokens_est(task) × input_per_mtok
            + task.max_tokens       × output_per_mtok ) / 1e6

input_tokens_est = ceil(chars(prompt + attachments) / 4) + 600
```

Prices come from each column's pricing snapshot (USD per million tokens).
The guard then compares **actual spend already accumulated** (summed from
run costs; night = the UTC calendar day, week = trailing 7 days) plus the
projection against the caps:

- Over a cap → the batch **refuses to run** and writes a critical
  `spend_refusal` alert with the full projection.
- At ≥80% of a cap → the batch runs and writes a `spend_warning`.
- `weekly_sweep` is checked against the weekly cap only (its projection is
  several nights' worth by design); `nightly_smoke` is checked against both.

Tunables: `MAVERICK_SPEND_NIGHTLY_CAP_USD` (default 100),
`MAVERICK_SPEND_WEEKLY_CAP_USD` (default 500). Actual spend accumulates on
the batch record and each batch logs projected vs actual.

## Corpus versioning

The corpus is data with an explicit lifecycle: **load → (strict) validate →
activate**. Loading never touches the running matrix; activation is an
atomic flip.

```sh
# validate + store, inactive (validation runs before any DB write):
go run ./cmd/maverick corpus load \
  --version 2026-08-01 \
  --dir ../modelmaverick-corpus/corpus \
  --description "August refresh" \
  --strict

# make it the active version:
go run ./cmd/maverick corpus activate --version 2026-08-01

# see all versions and the active flag:
go run ./cmd/maverick corpus status
```

- `--strict` promotes corpus-shape warnings (20–25 tasks and exactly 5 smoke
  members per capability) to errors; use it for production loads.
- Activation is also available over HTTP:
  `POST /internal/v1/corpus/activate {"version": "..."}` — for the Rails
  seam. Both paths are audited.
- The active version is the `matrix_version` on every API response and on
  every aggregate row written from that point.
- Batches run against the version active when they start; aggregates keyed
  to old versions are retained indefinitely.

### Tier calibration report

```sh
go run ./cmd/maverick corpus calibrate
```

Prints per-capability, per-column tier pass-rate tables from the latest
measured aggregates and flags:

- **INVERSION** — a harder tier outperforming an easier one (pass rates
  should be monotone decreasing in tier);
- **OUTLIER** — a tier more than 15 points off the linear trend between its
  measured neighbors.

It is a report, never a gate — use it after a corpus change or a few nights
of data to spot mis-tiered tasks. Exits 0 whenever the database is
reachable.

## Judge pool re-pointing

`MAVERICK_JUDGE_POOL` is a comma-separated list of `model_id/endpoint` refs
(default `gemini-3.1-pro/direct,gpt-5.5/direct`). At boot, each ref is
resolved against the model catalog and its provider client; grading picks
the first pool member whose provider **family differs** from the judged
column's, so no output is ever judged by its own family.

To re-point: change the variable and restart `serve` (or the
`--worker-only` process). Failure semantics are degradation, never a boot
failure — if the pool cannot be wired (column missing from the catalog,
provider credentials absent), the deterministic graders still run and
`judge_rubric` gradings fail per §5.2 (their dimensions are absent, failure
counters rise, and the batch `job_failure` alert fires) until the pool is
fixed. The boot log names the reason:
`judge pool unavailable; judge_rubric gradings will fail until it resolves`.

The same restart-to-apply rule holds for `MAVERICK_CLASSIFIER_COLUMN`
(default `claude-haiku-4-5/direct`): the column must exist in the catalog
and its provider must have credentials, otherwise prompt-path requests serve
the fallback classification and sustained failure raises
`classifier_degraded`.

## Column lifecycle and the retirement guard

Statuses: `cold_start → active → deprecated → retired`.

- New columns (`POST /internal/v1/columns`) are born `cold_start` with
  `cold_start_until` = creation + `MAVERICK_CATALOG_COLD_START_NIGHTS`
  (default 5). Cold-start columns run the **full** corpus nightly (dense
  runs) instead of the smoke set.
- The daily `catalog_sync` job performs the date-driven transitions:
  `cold_start` columns past their window become `active`; `deprecated`
  columns past their `deprecation_date` are retired — **through the guard**.
- **Retirement guard (§4.1):** any transition to `retired` — manual
  (`PUT /internal/v1/columns/{id}`) or automatic — first simulates every
  tenant pool containing the column with the column treated as retired. If
  any pool would drop to zero usable columns, the transition is rejected
  (`422 pool_would_be_empty` on the API; skipped-and-alerted in
  `catalog_sync`), a critical `pool_would_be_empty` alert names the affected
  tenants/pools, and the column keeps its status. Amend the tenants' pools,
  then retire.
- Retired columns disappear from `/v1/models` and from candidate sets, but
  their aggregate history remains readable forever.

Pricing is operator-maintained data: edit snapshots with
`PUT /internal/v1/columns/{id} {"pricing": {...}}`. Note for the current
build: the daily `price_sync` job has no external price source wired, so it
leaves snapshots unchanged — the internal API is the way prices change in
v1. Price edits never restate historical run costs.

## Debug capture and retention

- Capture is enabled by `MAVERICK_CAPTURE_MASTER_KEY` (required at boot).
  Prompts captured inside a window are encrypted with AES-256-GCM under a
  per-capture data key derived via HKDF(master key, salt = capture id); the
  master key never touches the database. Rotating the master key orphans
  existing captures (they can no longer be decrypted) but breaks nothing
  else.
- Windows (`POST /v1/debug/capture-windows`) are capped at
  `MAVERICK_CAPTURE_MAX_WINDOW_MINUTES` (default 1440). Captured rows expire
  at `window.ends_at + MAVERICK_CAPTURE_RETENTION_HOURS` (default 72); the
  hourly `capture_expiry` job hard-deletes expired ciphertext.
- Every replay/read/delete of a capture appends to `capture_access_log`,
  and log rows survive capture deletion.
- Zero-retention tenants (`constraints.zero_retention: true`) cannot create
  windows (409) **and** the request-time capture path is short-circuited by
  the same flag — belt and suspenders (G5).

### Debugging a routing decision

Replay (`POST /v1/debug/replay/{id}`) tells you whether the classifier
agrees with itself on re-run. It can't tell you whether the routed model was
actually *correct* for the content — that judgment needs the prompt. Runbook:

1. **Open a window** scoped to the suspect traffic: `POST
   /v1/debug/capture-windows` with `api_key_id` and/or `confidence_below` to
   narrow it (e.g. only this caller's low-confidence classifications).
2. **Reproduce** — have the caller retry, or wait for the next occurrence.
3. **List captures**: `GET /internal/v1/captures?window_id=` (internal
   listener, operator token) to find the capture id — no prompt text yet,
   just id/tenant/prompt_hash/classification/expiry.
4. **Read the prompt**: `GET /internal/v1/captures/{id}/prompt` decrypts it
   in memory and returns it alongside the stored classification.
5. **Compare against the decision trace**: `GET
   /v1/history/recommendations/{recommendation_id}` for the full candidate
   list, scores, and the pick — now readable against the actual content
   instead of just its fingerprint.
6. **Close the window** early once done: `DELETE
   /v1/debug/capture-windows/{id}`. Already-captured rows are unaffected and
   still expire on their own schedule.

The `capture_access_log` row written by step 4 (success or not — a 410 or
404 attempt is logged too) **is** the audit trail for who read what and
when; there is no separate approval step to configure.

Tenants can self-serve this same flow on their own traffic without an
operator: steps 3–4 via `GET /v1/debug/captures` and
`GET /v1/debug/captures/{id}/prompt` (own captures only, reads audited the
same way — see `docs/api.md` "Debug capture").

## History retention

Recommendations, outcomes, and decision traces are pruned daily past
`MAVERICK_HISTORY_RETENTION_DAYS` (default 365) by `history_prune`.
Aggregates and calibration rows are retained indefinitely — they are the
product.

## Backups

Postgres is the entire state: tenants and keys (hashes only), the catalog,
the corpus, runs, aggregates, calibration, alerts, capture ciphertext, and
the River job queue all live in the one database. `pg_dump` of that database
is a complete backup. Two secrets live **outside** the database and must be
backed up/rotated separately as environment configuration:
`MAVERICK_CAPTURE_MASTER_KEY` (without it, captured prompts are
unrecoverable) and `MAVERICK_INTERNAL_TOKENS`. Raw tenant API keys are
recoverable nowhere — reissue on loss.

## Jobs and schedules

All periodic jobs are River periodic jobs (Postgres-backed, no Redis),
scheduled by five-field cron expressions evaluated in **UTC**, idempotent
per (job, window) via unique jobs. Workers run inside `maverick serve`
(skip with `--api-only`; run alone with `--worker-only`).

| Job | Default | Env var | What it does |
|---|---|---|---|
| `prediction_lock` | `50 2 * * *` | `MAVERICK_CRON_PREDICTION_LOCK` | Snapshot EWMA-implied pass probabilities for tonight's cells before results exist (hindsight-proof calibration). |
| `nightly_smoke` | `0 3 * * *` | `MAVERICK_CRON_NIGHTLY` | Smoke tasks × all non-retired columns × 3 repeats; cold-start columns run the full corpus. |
| `weekly_sweep` | `0 4 * * 0` | `MAVERICK_CRON_WEEKLY` | Full active corpus × all non-retired columns × 3 repeats. |
| `latency_probes` | `0 * * * *` | `MAVERICK_CRON_LATENCY_PROBES` | Thin pings for columns with `quality_source_column_id`; feeds latency cells only. |
| `aggregate_rebuild` | chained | — | After each batch: recompute cells, fold EWMAs, append aggregates, update `measured`. |
| `regression_scan` | chained | — | After each rebuild: §6.5 regressions + the parse-failure watch → alerts. |
| `calibration_score` | chained | — | After each rebuild: Brier + overconfidence vs the locked predictions → calibration rows. |
| `price_sync` | `0 6 * * *` | `MAVERICK_CRON_PRICE_SYNC` | Refresh pricing snapshots (no source wired in the current build — see above). |
| `catalog_sync` | `10 6 * * *` | `MAVERICK_CRON_CATALOG_SYNC` | Lifecycle transitions (cold-start activation, guarded retirement) + the pool-drift check. |
| `capture_expiry` | `0 * * * *` | `MAVERICK_CRON_CAPTURE_EXPIRY` | Hard-delete expired captures. |
| `history_prune` | `0 5 * * *` | `MAVERICK_CRON_HISTORY_PRUNE` | Enforce history retention. |

Chaining: `nightly_smoke`/`weekly_sweep` enqueue `aggregate_rebuild` on
completion, which enqueues `regression_scan` and `calibration_score`. A
partially failed batch still rebuilds from what completed; completion below
90% raises `job_failure`. Keep `prediction_lock` strictly before the nightly
batch — both key the same UTC date.

**Job timeouts.** River's own default job timeout is 1 minute — far too
short for a real batch (a full `weekly_sweep` runs thousands of provider
calls over hours). `nightly_smoke` and `weekly_sweep` override it via
`MAVERICK_BATCH_JOB_TIMEOUT` (default `6h`, sanity-capped at 24h); the four
chained/hourly jobs (`aggregate_rebuild`, `regression_scan`,
`calibration_score`, `latency_probes`) use a fixed 15-minute override with no
env var, since none of them place provider calls at batch scale. Even so, a
batch large enough to exceed its budget anyway still gets its `job_failure`
alert and its `aggregate_rebuild` enqueue: both run on a short-lived context
detached from the (possibly already-expired) job context, so they survive
independently of it.

## Live provider smoke test

Spec §15 allows exactly one test to touch real providers: "a nightly *live*
smoke test (opt-in, spend-capped) is the only test allowed to touch real
providers." That harness is `TestLiveSmoke` in `internal/livesmoke`:

```sh
export MAVERICK_ANTHROPIC_API_KEY=...   # keys for the selected columns' providers
export MAVERICK_OPENAI_API_KEY=...
make live-smoke
```

- **Opt-in, twice over.** The test runs only when `MAVERICK_LIVE_SMOKE=1`
  (the Make target sets it) **and** the selected columns' provider keys are
  exported. Missing either → it skips with a message, so `go test ./...`
  and CI never touch the network. The live path is operator-only.
- **What it runs.** 2 embedded tiny tasks × 2 columns × 1 repeat — 4 real
  completion calls. The tasks are embedded, not drawn from the corpus, so
  the harness is deterministic and runs with nothing but keys (no database,
  no private corpus checkout). Default columns are the cheap pair
  `anthropic/claude-haiku-4-5` and `openai/gpt-5.4-mini`; override with
  `MAVERICK_LIVE_SMOKE_COLUMNS` — exactly two comma-separated
  `provider:model_id:input_per_mtok:output_per_mtok` entries.
- **Spend cap.** Before any call, the projection runs through the same
  `jobs.ProjectSpend` formula the batch spend guard uses (§ Spend caps
  above); projected > **$1** aborts with zero calls made. Actual cost is
  accumulated per call from the pricing snapshot and re-checked against the
  same $1 cap after every call. The default run projects well under a cent.
- **Shape and sanity only.** Assertions are: the call succeeds, output text
  is non-empty, tokens in/out > 0, latency > 0, accumulated cost under the
  cap. It **never** asserts scores or output quality — a live model
  drifting is the product's job to measure, not this harness's to gate.
- **Scheduling (optional).** Nothing is scheduled by default. For the
  nightly cadence, put the keys in an env file readable only by the
  operator user and add a crontab entry on any machine with the repo and
  Go, e.g.:

  ```cron
  # nightly live smoke at 06:30 UTC; output to the operator log
  30 6 * * *  cd /srv/modelmaverick && set -a && . /etc/maverick/live-smoke.env && set +a && make live-smoke >> /var/log/maverick-live-smoke.log 2>&1
  ```

  A failed run exits non-zero; wire the cron mail or your scheduler's
  alerting to that. This is deliberately not a River job — it must stay
  independent of the deployment it is smoke-testing.

## Process shapes and observability

- One binary, three run shapes: `serve` (API + MCP + workers),
  `serve --api-only` (stateless, horizontally scalable), `serve
  --worker-only` (exactly one). The internal listener is served in every
  shape.
- Health: `GET /up` (public, unauthenticated — the deploy health check);
  `GET /health/detail` (internal) for db, provider-config, River queue
  health, the active matrix version, and last batch times — full shape in
  `docs/api.md`.
- Logs are slog JSON on stdout. Every request logs method, path, status,
  ms, and ids; every batch logs batch_id, projected vs actual spend, ok/error
  run counts, and duration; every provisioning write logs `audit=true` with
  the acting token label. Metrics endpoints are deferred in v1 — the alerts
  table plus logs are the operational surface.
- `cmd/maverick-proxy` is a separate binary and a separate deployable — the
  Track 3 trial proxy (`plans/model-maverick-integration-surfaces-plan.md`
  §4). It has no database and no migrations; see
  `cmd/maverick-proxy/README.md` for its env vars and base-URL swap
  recipes.

## Hot-path benchmarks

Spec §2.3 sets the budget for the declared recommend path: one Postgres
round trip for aggregates, one insert, no provider calls, **target p95 <
50 ms intra-region**. The prompt path on a classification-cache hit skips
the model call and must land in the same regime (the classifier call only
dominates on a cache miss, target p95 < 2.5 s).

`make bench` runs the load harness in `internal/api/bench_test.go`: the
real chi router served on a live loopback listener (`httptest.NewServer`),
hit by a keep-alive HTTP client, against a migrated throwaway Postgres 16
(testcontainers) seeded like production traffic — one tenant, twelve active
columns with measured quality/pass-rate/cost/latency EWMAs across
`structured_extraction` × all 5 tiers, a default pool over all twelve,
tenant weights, and standing constraints. Each scenario warms up with 200
requests, then times 3,000 requests in one pass; percentiles are
client-observed request latencies (nearest-rank). The prompt scenario
primes the `(tenant, prompt_hash, classifier_version)` cache once, proves
the second request reports `method: "cached"`, and asserts the stub
classifier is called **zero** times during measurement.

Recorded 2026-07-07 on an Apple M3 Max (16 cores, 128 GB RAM), macOS,
go1.26.4, Postgres 16-alpine under Docker 29.5.2 (loopback port mapping —
every store round trip crosses the Docker VM boundary, so these numbers are
if anything pessimistic vs. an intra-region deployment):

| Scenario | Concurrency | p50 | p95 | p99 | vs 50 ms target |
|---|---|---|---|---|---|
| Declared path | 1 | 2.36 ms | 2.79 ms | 3.12 ms | **PASS** (17.9× headroom) |
| Declared path | 16 | 5.49 ms | 7.53 ms | 10.21 ms | **PASS** (6.6× headroom) |
| Prompt path, cache hit | 1 | 2.86 ms | 3.35 ms | 3.77 ms | **PASS** (14.9× headroom) |

At concurrency 16 the harness sustained ~2,800 req/s through one server
process (357 µs/op wall). A repeat run reproduced every cell within ~0.3 ms,
so no profiling or hot-spot fixes were needed. Allocation counts are not
reported: the harness measures the server from the outside, so `-benchmem`
would describe the bench client, not the hot path. Re-record this table
(same command) after any change to the recommend pipeline, the policy
engine, or the aggregates queries.

## Deployment

Production is one server deployed with Kamal 2; `config/deploy.yml` is the
authoritative, heavily commented walkthrough. The shape:

- The `Dockerfile` builds a static `maverick` binary (CGO off) into a
  distroless nonroot image — `serve` is the default command and `migrate`
  is the same binary, so one image covers every run shape.
- kamal-proxy fronts the **public** listener only (`app_port: 8080`) with
  automatic HTTPS and `GET /up` healthchecks. The **internal** listener
  (`:8081`) has no proxy route and is published to the host loopback only —
  reach it with `ssh -N -L 8081:127.0.0.1:8081 root@<host>` and call
  `http://localhost:8081/internal/v1/...` through the tunnel.
- Postgres 16 runs as the `postgres` Kamal accessory on the same host, data
  on the named Docker volume `postgres-data`, port bound to the host
  loopback for `pg_dump` (see Backups). Accessories are not touched by
  `kamal deploy`; `kamal accessory remove postgres` deletes the data.
- Migrations run automatically under Kamal: the `.kamal/hooks/pre-app-boot`
  hook executes `maverick migrate` in a one-off container from the version
  being deployed, before traffic cuts over. `serve` itself never migrates —
  the hook is the only automatic trigger, so any deploy path that bypasses
  it (a bare `docker run`, a manual restart) must run `maverick migrate`
  first. Run one manually with `kamal migrate` (an alias for
  `kamal app exec --primary "migrate"`); either way, `migrate` loads the same
  full required env as `serve` (`MAVERICK_DATABASE_URL`,
  `MAVERICK_INTERNAL_TOKENS`, `MAVERICK_CAPTURE_MASTER_KEY`) even though it
  only touches the database — the two commands share one config loader.
- Secrets come from `.kamal/secrets` — a committed template of env-var
  passthroughs holding **no real values**; export the real values in the
  deploying shell/CI or wire in `kamal secrets fetch` from a password
  manager.

First deploy: fill the placeholders in `config/deploy.yml`, point DNS at
the box, export the secrets, then `kamal setup` (bootstraps Docker, boots
the accessory, deploys). Routine releases: `kamal deploy`. After each
deploy confirm `https://<host>/up` returns 200 via the proxy and that port
8081 is unreachable from outside, and watch the first production
`nightly_smoke` complete under the spend caps (`kamal app logs`, the
alerts feed).
