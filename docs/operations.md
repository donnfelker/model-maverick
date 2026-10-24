# Operations

Running the Rust engine day to day: alerts, spend, the corpus lifecycle, catalog
administration, capture hygiene, retention, backups, the job schedule, observability,
and deployment.

- Endpoint shapes are in `docs/api.md`.
- Configuration is authoritative in spec §14.1 and `crates/maverick-config`. This page names
  the variables an operator touches.
- The proxy has its own page: `docs/features/proxy.md`.
- The deploy images, the Kamal files, and the backup scripts are described in
  [`docker/README.md`](../docker/README.md).

## Deployment role (D49)

Set `MAVERICK_ROLE=origin` for the hosted deployment and `MAVERICK_ROLE=replica`
for a customer install. There is no default. Startup refuses an unset or unknown role.

An origin keeps the corpus and runs the benchmark and discovery jobs. A replica
registers `catalog_sync`, `latency_probes`, `capture_expiry`, and `history_prune`.
Its `price_sync` is off unless `MAVERICK_CRON_PRICE_SYNC` has a schedule. Corpus
commands refuse on a replica. Judge pool and corpus spend caps are unused there.

Replica `catalog_sync` preserves bundle column statuses and warns with `matrix_stale`
when the active bundle is absent or more than seven days old. Health detail reports
`role` and the active bundle version, or `""` before the first import. Probe clients
use the configured provider endpoints on both roles.

## Configuration compatibility (D50)

A new release must start with an older environment file. Three rules make sure it does:

- **Unknown settings.** Every command that loads the configuration logs one `WARN` line,
  `unknown setting ignored`, for each `MAVERICK_*` variable that no setting reads. The
  line names the variable in `var`. It never has the value, because a value can be a
  secret. The command still runs. A warning usually means a typo, or a setting that only
  an older release reads. These names never warn: the proxy's settings
  (`MAVERICK_ENGINE_URL`, `MAVERICK_API_KEY`, `MAVERICK_PROXY_*`, `MAVERICK_UPSTREAM_*`),
  the release stamp (`MAVERICK_RELEASE_DATE`, `MAVERICK_RELEASE_COMMIT`), and the test
  harness's `MAVERICK_TEST_*`.
- **Renamed settings.** A renamed setting keeps its old name for at least 12 months after
  the release that renamed it. When only the old name is set, the engine uses its value
  and warns `renamed setting, use the new name`, with the new name in `replacement`. When
  both are set, the new name wins, and the warning is `renamed setting ignored, the new
  name is set`. The renames are the `RENAMED` table in
  `crates/maverick-config/src/compat.rs`, which is empty today. A test fails when a row
  is past its window, so a row is removed on purpose.
- **New required settings.** A new setting needs a safe default, or it ships only in a
  major release. Today both roles need only `MAVERICK_ROLE`, `MAVERICK_DATABASE_URL`,
  `MAVERICK_INTERNAL_TOKENS`, and `MAVERICK_CAPTURE_MASTER_KEY`. The test
  `todays_required_settings_are_enough` in `crates/maverick/tests/config_compat.rs`
  starts with only these, so a release that adds a required setting fails it.

## Alerts

Alert rows are the durable operational signal. Every alert is first written to the
`alerts` table, then fanned out to the configured destinations. A failed delivery is
logged, but it never blocks or fails the write that created the row.

### Kinds

| `kind` | Severity | Written by | Meaning |
|---|---|---|---|
| `spend_refusal` | critical | batch spend guard | The projected batch spend would exceed a cap. The batch **did not run**. The subject carries the projection, the spend so far, and the caps. |
| `spend_warning` | warning | batch spend guard | The batch ran, but the night or week total reached 80% of a cap or more. |
| `job_failure` | warning or critical | batch and rebuild jobs | A batch completed below 90% (warning), failed mid-batch, or an aggregate rebuild failed (critical). |
| `regression` | warning or critical | `regression_scan` | A cell regressed per spec §6.5: a smoothed drop of more than 3.0 points against the trailing 7-day maximum, or a raw shock. It is critical when the cell falls below a quality bar that it cleared before. One alert per cell per 7 days, re-armed when the drop deepens by 2 points or more. |
| `parse_failure_rate` | warning | `regression_scan` | A column's batch parse-failure rate is above 5% **and** above twice its trailing 7-day rate: the early sign of silent escalation on the caller side. |
| `classifier_degraded` | critical (info for a resolution or restored evidence) | the classifier resolver, its re-resolution, and `PUT /internal/v1/columns/{id}` | More than 5% of classifier calls failed over 15 minutes (at least 20 samples). One alert per episode, re-armed on recovery. Or the classifier column stayed unresolved for 5 minutes (subject `reason: "unresolved"`, D41), and its later resolution writes an `info` alert (`reason: "resolved"`). Prompt-form requests get the fallback classification meanwhile. Or the zero-retention classifier column has no valid zero evidence (`reason: "zero_retention_evidence"`, spec §8.3). Zero-retention tenants' prompt-form requests then get `422 zero_retention_requires_declared` until it is fixed, and valid evidence writes an `info` alert (`reason: "zero_retention_evidence_restored"`). See "Zero-retention evidence". |
| `pool_would_be_empty` | critical | retirement guard, drift check, recommend | A retirement was blocked (spec §4.1), the nightly drift check found a pool with no usable column for some cell, or a request found that drift. The subject names the tenants and pools. |
| `retention_expiring` | warning | `catalog_sync` | A column's or an execution alias's zero-retention evidence expires within 14 days (spec §4.5). Renew it before it expires: expired evidence reads as `unknown`, and zero-retention tenants then lose that column (D1, D15). |
| `new_model_available` | info | `model_discovery` and the models refresh (D51) | A provider lists a text model that no catalog column covers. One alert per provider and base id. Enroll it or leave it (see "New models"). |
| `model_withdrawn` | warning | `model_discovery` and the models refresh (D51) | A non-retired `direct` column's model is no longer in its provider's list. One alert per column. The column keeps its status until you deprecate it. |

Read the feed at `GET /internal/v1/alerts?since=&kind=` (internal listener, a bearer token
from `MAVERICK_INTERNAL_TOKENS`).

### Delivery

Both channels are optional and configured by environment. An unset channel is off; the rows
still land in the table.

- **Email:** `MAVERICK_SMTP_URL`.

  ```
  smtp://user:pass@mail.example.com:587/?from=alerts@example.com&to=ops@example.com,oncall@example.com
  ```

  The port defaults to 587. STARTTLS is used when the server offers it, and the
  credentials authenticate with PLAIN after the upgrade. Each alert is one plain-text mail
  with the subject `[maverick] <severity>: <kind>`.

- **Operator webhook:** `MAVERICK_ALERTS_WEBHOOK_URL` and `MAVERICK_ALERTS_WEBHOOK_SECRET`.
  The secret is **required** when the URL is set: an unsigned webhook is a startup error.
  Each alert is POSTed as JSON:

  ```json
  { "id": "alr_01K...", "kind": "spend_refusal", "severity": "critical",
    "subject": { "...": "..." }, "message": "...",
    "created_at": "2026-07-08T03:00:12.345Z" }
  ```

  The header `X-Maverick-Signature: sha256=<hex>` is the lowercase hex HMAC-SHA256 of the
  **exact request body**, keyed with the secret. Verify over the bytes you received, with a
  constant-time compare:

  ```python
  import hmac, hashlib

  def verify(secret: bytes, body: bytes, header: str) -> bool:
      expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
      return hmac.compare_digest(expected, header)
  ```

  A response other than 2xx is a failed attempt.

Retries, per channel and per destination: one attempt plus 3 retries, 1 s, 2 s, then 4 s
apart, and 30 s per attempt. Deliveries run in the background, and shutdown drains the ones
in flight. Request-time writers (a degraded classifier, the recommend drift alert, a blocked
retirement) deliver through the same path as the jobs, so no critical alert is only stored.

## Spend caps (G8)

Before it runs, every batch (`nightly_smoke`, `weekly_sweep`) projects its cost:

```
projected = sum over task x column x repeats of
            ( input_tokens_est(task) x input_per_mtok
            + task.max_tokens       x output_per_mtok ) / 1e6

input_tokens_est = ceil(chars(prompt + attachments) / 4) + 600   (chars are code points)
```

Prices come from each column's pricing snapshot, in USD per million tokens. The guard adds
the projection to the **actual spend so far** (the sum of the run costs; the night is the
UTC calendar day, the week is the trailing 7 days) and compares the totals with the caps:

- Over a cap: the batch **refuses to run** and writes a critical `spend_refusal` alert with
  the full projection.
- At 80% of a cap or more: the batch runs and writes a `spend_warning`.
- `weekly_sweep` is checked against the weekly cap only, because its projection is several
  nights' worth by design. `nightly_smoke` is checked against both.

Tunables: `MAVERICK_SPEND_NIGHTLY_CAP_USD` (default 100) and `MAVERICK_SPEND_WEEKLY_CAP_USD`
(default 500). Each batch logs its projected and actual spend.

Cold-start columns run the whole corpus every night, not the smoke set, and the projection
prices every task at its full `max_tokens`. So a new column can take a night over the
nightly cap. Size the caps before you add columns (`docs/runbook-go-live.md` section 1).

- A resumed batch (D10) projects only the runs it still has to make, because the earlier
  runs are already in the actual spend.
- The hidden `maverick spend preview` command (D52) prints per-group and per-column
  projections, planned and remaining runs, night and week actual spend, both caps, and
  the verdict. It writes nothing and makes no provider call. A refusal verdict still
  exits 0 because the report succeeded.

After loading and activating the corpus and configuring columns, preview both jobs:

```sh
kamal app exec --roles=web --primary "spend preview --job nightly_smoke --night YYYY-MM-DD"
kamal app exec --roles=web --primary "spend preview --job weekly_sweep --night YYYY-MM-DD"
```

Omit `--night` for today in UTC and `--job` for `nightly_smoke`. Size both configured caps
for the cold-start week, then run the reports again. The weekly actual includes nightly
spend from the trailing seven days. The report does not predict future nights or judge
grading costs; the G8 projection covers benchmark runs only.

The classifier has its own budget (D4): `MAVERICK_CLASSIFIER_DAILY_QUOTA` (default 1000
classifications per tenant per day) and `MAVERICK_CLASSIFIER_DAILY_SPEND_CAP_USD` (default
5.00 per day in total). Past either, prompt-form requests get the fallback classification
with `classification_quota_exceeded: true`. Bead 9.8 retunes both from measured cost.

## Corpus versioning

The corpus is data with an explicit lifecycle: **load, validate, activate**. Loading never
touches the running matrix, and activation is an atomic flip. The commands are the
`maverick` binary's (from a checkout: `cargo run -p maverick -- corpus ...`):

```sh
# validate and store, inactive (validation runs before any database write):
maverick corpus load \
  --version 2026-08-01 \
  --dir <path to the private corpus>/corpus \
  --description "August refresh" \
  --strict

# make it the active version:
maverick corpus activate --version 2026-08-01

# every version and the active flag:
maverick corpus status
```

- `--strict` turns the corpus-shape warnings (20 to 25 tasks and exactly 5 smoke members per
  capability) into errors. Use it for production loads.
- Activation is also available over HTTP for the Rails seam:
  `POST /internal/v1/corpus/activate {"version": "..."}`. Both paths are audited.
- The active version is the `matrix_version` on every API response, and on every aggregate
  row written from then on.
- A batch runs against the version that is active when it starts. Aggregates of older
  versions are kept.

### Tier calibration report

```sh
maverick corpus calibrate
```

It prints per-capability, per-column tables of the tier pass rates from the latest
measured aggregates, and flags:

- **INVERSION:** a harder tier that outperforms an easier one (pass rates should decrease
  with the tier);
- **OUTLIER:** a tier more than 15 points off the linear trend between its measured
  neighbors.

It is a report, never a gate. Use it after a corpus change or a few nights of data to find
mis-tiered tasks. It exits 0 whenever the database is reachable.

## The judge pool and the classifier column

`MAVERICK_JUDGE_POOL` is a comma-separated list of `model_id/endpoint` references (default
`gemini-3.1-pro/direct,gpt-5.5/direct`). Each reference resolves against the catalog and
its provider client. Grading picks the first pool member whose provider **family differs**
from the judged column's, so no output is judged by its own family.

`MAVERICK_CLASSIFIER_COLUMN` (default `claude-haiku-4-5/direct`) and, for zero-retention
tenants, `MAVERICK_CLASSIFIER_ZERO_RETENTION_COLUMN` (D2) resolve the same way. The provider
keys (`MAVERICK_*_API_KEY`) are operator keys, used only for benchmarks and classification.

They resolve when `serve` starts, and again in the background (D41;
`crates/maverick/src/rewatch.rs`). The classifier resolves in the API processes (`serve` and
`serve --api-only`). The judge pool resolves in the worker process (`serve` and
`serve --worker-only`). An unresolved reference is retried every 30 s. A resolved one is
refreshed every 5 min, so a catalog change takes effect without a restart. A failure
degrades and never stops the process:
- The judge pool logs `judge pool unavailable; judge_rubric gradings will fail until it
  resolves`. The deterministic graders still run, and `judge_rubric` gradings fail until the
  pool resolves (their dimensions are absent and the batch raises `job_failure`). Resolution
  logs `judge pool resolved; judge_rubric gradings resume`.
- The classifier logs `classifier column unavailable; prompt-path requests will receive the
  fallback classification`. Its requests get `fallback_default` (G1), and a zero-retention
  tenant's prompt-form request still fails closed. Resolution logs `classifier column
  resolved; prompt-path requests use the classifier`.
- `classifier_degraded` fires on a sustained failure rate of the classifier calls. It also
  fires when the classifier stays unresolved for 5 min: critical, with subject
  `reason: "unresolved"`. A later resolution writes an `info` `classifier_degraded` with
  `reason: "resolved"`.

## Column lifecycle and the retirement guard

The statuses: `cold_start`, `active`, `deprecated`, `retired`.

- A new column (`POST /internal/v1/columns`) is born `cold_start`, with `cold_start_until`
  at creation plus `MAVERICK_CATALOG_COLD_START_NIGHTS` (default 5). Cold-start columns run
  the **full** corpus nightly instead of the smoke set.
- The daily `catalog_sync` job makes the date-driven transitions: `cold_start` columns past
  their window become `active`, and `deprecated` columns past their `deprecation_date` are
  retired, **through the guard**.
- **The retirement guard (spec §4.1):** any transition to `retired`, manual
  (`PUT /internal/v1/columns/{id}`) or automatic, first simulates every tenant pool that
  holds the column, with the column retired. If any pool would drop to zero usable columns,
  the transition is refused (`422 pool_would_be_empty` on the API; skipped and alerted in
  `catalog_sync`), a critical `pool_would_be_empty` alert names the tenants and pools, and
  the column keeps its status. Amend the pools, then retire.
- A retired column leaves `/v1/models` and the candidate sets. Its aggregate history stays.

### Zero-retention evidence

A column's retention evidence (`retention_mode`, `retention_verified_at`,
`retention_valid_until`, `retention_source`) and each execution alias's evidence
(`execution_alias_retention`) are set with `POST` and `PUT /internal/v1/columns`. The
validity window is at most 90 days. Evidence past `retention_valid_until` reads as
`unknown`. A zero-retention tenant then loses that column (`409 no_compliant_columns` when
none is left, D1) or that OpenRouter alias (D15). `catalog_sync` warns 14 days ahead with
`retention_expiring`.

The column that `MAVERICK_CLASSIFIER_ZERO_RETENTION_COLUMN` names is watched like a pool
member (spec §8.3). The API processes check it at boot, every 5 min, and after each
`PUT /internal/v1/columns/{id}`. When it resolves but holds no valid zero evidence, a
critical `classifier_degraded` alert fires with `reason: "zero_retention_evidence"`. Its
subject names `classifier_column`, `column_id`, and `zero_retention_column`. Until you fix
it, every zero-retention tenant's prompt-form request gets
`422 zero_retention_requires_declared`. That fails closed, so no prompt leaves the engine,
but those requests fail. Fix it one of two ways:
- Renew the column's evidence with `PUT /internal/v1/columns/{id}`. The same request writes
  an `info` `classifier_degraded` with `reason: "zero_retention_evidence_restored"`.
- Point `MAVERICK_CLASSIFIER_ZERO_RETENTION_COLUMN` at a column that holds valid zero
  evidence, and restart the API processes (`serve` or `serve --api-only`). The setting is
  read only at startup. The alert state is kept per reference, so no `restored` alert
  follows for the old one.

The usual cause is expired evidence, so renew when `retention_expiring` warns.

### Pricing

Pricing snapshots are operator data. Edit one with
`PUT /internal/v1/columns/{id} {"pricing": {...}}`. The daily `price_sync` job can also
refresh them from models.dev (bead 7.15):

- `MAVERICK_MODELS_DEV_PRICE_URL` (default `https://models.dev/api.json`);
- `MAVERICK_MODELS_DEV_PRICE_MAP`: a JSON array that maps each catalog column
  (`column_provider`, `column_model_id`, `column_endpoint`) to its models.dev provider and
  model. There is no fuzzy matching. Unmapped columns count as `missing_from_source`.
- Without a map, `price_sync` changes nothing, and the internal API is the way prices
  change. A change of more than 10 times the stored price is refused.
- The snapshot can also hold `cache_read_per_mtok` and `cache_write_per_mtok` (D44).
  `price_sync` copies them from the models.dev `cache_read` and `cache_write` fields, and a
  change to one of them alone is a price change. A source entry with a negative cache
  price is skipped. Only cost-aware conversation affinity reads them; run cost never does.

A price edit never restates the cost of past runs.

## New models

The engine notices model releases through each provider's own model-list API, never through
a third-party feed.

- **Daily.** At 06:20 UTC (`MAVERICK_CRON_MODEL_DISCOVERY`), `model_discovery` lists the
  text models of each provider that has an operator key, and compares them with the catalog.
  A provider without a key is skipped.
- **Alerts.** A model the catalog does not have raises one `new_model_available` alert
  (info). A `direct` column whose model its provider no longer lists raises one
  `model_withdrawn` alert (warning). The catalog does not change by itself.
- **On demand.** `POST /internal/v1/models/refresh` through the tunnel runs discovery now
  (`docs/api.md`). An empty body reports. `{"enroll": [...]}` creates the columns, all or
  nothing.
- **Enrolling.** Use the pinned snapshot id that the report names. Provider lists carry no
  prices, so give the list price as `pricing` (see "Pricing"), and the retention evidence
  when you have it (see "Zero-retention evidence").
- **Spend.** A new column is `cold_start`: it runs the whole corpus every night for
  `MAVERICK_CATALOG_COLD_START_NIGHTS` (5) nights, not the smoke set, and its cells become
  measured after `MAVERICK_SCORING_MIN_NIGHTS` (3). So one new column costs far more per
  night than a measured one. The refresh response shows the next nightly projection with the
  new columns. A projection over the nightly cap refuses that whole batch (G8). So check it
  before you enroll several models at once.
- **Withdrawn models.** Deprecate the column (`PUT /internal/v1/columns/{id}` with
  `status: deprecated` and a `deprecation_date`), then retire it later. The retirement guard
  still protects the pools (see "Column lifecycle and the retirement guard").
- **The judge pool and the classifier column stay manual.** Discovery never re-points them.
  A new model joins `MAVERICK_JUDGE_POOL` only after its `critique_and_judging` cells are
  measured and rank above the current members (see "The judge pool and the classifier
  column").

## Debug capture and retention

- Captured prompts are encrypted with AES-256-GCM under a per-capture data key: HKDF of the
  master key, salted with the capture id. The master key never touches the database.
- **The key ring (D18):** `MAVERICK_CAPTURE_MASTER_KEY` (64 hex characters) with
  `MAVERICK_CAPTURE_MASTER_KEY_VERSION` (default 1) is the current key. Each capture stores
  the version that encrypted it, and reads decrypt with that version's key. To rotate:
  1. Set the old key as `MAVERICK_CAPTURE_PREVIOUS_MASTER_KEY` (`<version>:<hex>`).
  2. Set a new current key with a higher version, and restart.
  3. Drop the previous key once `MAVERICK_CAPTURE_MAX_WINDOW_MINUTES` plus
     `MAVERICK_CAPTURE_RETENTION_HOURS` have passed since the rotation (spec §10.4). By then
     every capture under it has expired.
- A window (`POST /v1/debug/capture-windows`) lasts at most
  `MAVERICK_CAPTURE_MAX_WINDOW_MINUTES` (default 1440). A captured row expires at the
  window's end plus `MAVERICK_CAPTURE_RETENTION_HOURS` (default 72). The hourly
  `capture_expiry` job hard-deletes expired ciphertext.
- Every replay, read, and delete of a capture appends to `capture_access_log`. The log rows
  survive the capture's deletion.
- Zero-retention tenants (`constraints.zero_retention: true`) cannot create windows (409),
  **and** the request-time capture path checks the same flag (G5). A replay for a tenant
  that turned zero retention on after a capture follows D2 (question
  `model-maverick-rust-port-3hk.10.12` for the owner).

### Debugging a routing decision

A replay (`POST /v1/debug/replay/{id}`) tells you whether the classifier agrees with itself
on a new run. It cannot tell you whether the routed model was right for the content; that
needs the prompt. The runbook:

1. **Open a window** scoped to the suspect traffic: `POST /v1/debug/capture-windows` with
   `api_key_id` or `confidence_below` to narrow it (for example, only one caller's
   low-confidence classifications).
2. **Reproduce:** have the caller retry, or wait for the next occurrence.
3. **List the captures:** `GET /internal/v1/captures?window_id=` (internal listener, operator
   token) gives the ids, tenants, prompt hashes, classifications, and expiry. It has no
   prompt text.
4. **Read the prompt:** `GET /internal/v1/captures/{id}/prompt` decrypts it in memory and
   returns it with the stored classification.
5. **Compare with the decision trace:** `GET /v1/history/recommendations/{recommendation_id}`
   gives the candidates, the scores, and the pick. Read them against the actual content.
6. **Close the window** when done: `DELETE /v1/debug/capture-windows/{id}`. Captured rows are
   not touched and expire on their own schedule.

The `capture_access_log` row that step 4 writes (also for a 404 or a 410) **is** the audit
trail of who read what and when. There is no separate approval step.

Tenants can do steps 3 and 4 on their own traffic without an operator, through
`GET /v1/debug/captures` and `GET /v1/debug/captures/{id}/prompt` (own captures only,
audited the same way; see `docs/api.md`).

## History retention

The daily `history_prune` job removes recommendations, outcomes, and decision traces older
than `MAVERICK_HISTORY_RETENTION_DAYS` (default 365), with their captures. Aggregates and
calibration rows are kept: they are the product.

## Backups

Postgres is the whole state: tenants and keys (hashes only), the catalog, the corpus, runs,
aggregates, calibration, alerts, capture ciphertext, and the graphile_worker job queue. A
`pg_dump` of the one database is a complete backup.

- `scripts/backup.sh` writes the dump and the exact row count of every table, from one
  snapshot. The Kamal `backup` accessory runs it nightly and copies it off the box when
  `MAVERICK_BACKUP_TARGET` is set; it warns every night while that is unset.
- `scripts/restore-test.sh` restores a dump into a scratch database, checks the row counts,
  and runs `maverick migrate` on it.
- `docker/README.md` has the commands.

Three secrets live **outside** the database. Keep them in the password manager with the
other secrets: `MAVERICK_CAPTURE_MASTER_KEY` (and the previous key during a rotation;
without them captured prompts cannot be read) and `MAVERICK_INTERNAL_TOKENS`. Raw tenant API
keys are stored nowhere: reissue a lost key.

## Schema changes and upgrades (D50)

Migrations are up only and additive (D37). A change that removes or renames uses expand
and contract across two minor releases: release N adds the new shape and writes both,
release N+1 stops reading the old shape, and a later release drops it only after the
12-month upgrade window has passed N.

Every migration from 0012 on starts with two header lines:

```sql
-- class: online-safe
-- tables: upgrades, serve_heartbeats
```

- `online-safe`: the previous release keeps working against the new schema. New tables,
  new nullable or defaulted columns, new indexes built concurrently, and widened checks
  are online-safe.
- `needs-downtime`: the previous release can fail against the new schema, so every
  `serve` stops before it is applied.
- `tables`: every table the migration creates or alters. `maverick upgrade` reads one row
  of each after it applies them.

The test `every_migration_declares_its_class_and_tables` in `maverick-store` refuses a
migration without them. Migrations 0001 to 0011 predate the header and are never edited,
because a changed file changes its sqlx checksum. Their class and tables are in `LEGACY`
in `crates/maverick-store/src/schema.rs`. All of them are online-safe except 0005, which
adds a `not null` column with no default to `debug_captures`.

`maverick migrate` checks the database before it applies anything:

- **Newer database.** A database with migrations this binary does not know, engine or
  job queue, is refused, and the message names both schema versions. Run a release that
  knows them, or restore a backup. Rollback is always a restore (D37).
- **Upgrade window.** Any release from the last 12 months upgrades directly. An older
  database is refused, and the message names the release to run first. The window is
  data: `RELEASES` in the same file lists each release's name, date, and newest migration,
  and the release pipeline appends one row per release. A fresh database is always
  accepted, and the table is empty until the first release.

### `maverick upgrade`

The operator's upgrade command. Install the new release by your normal process, run
`maverick upgrade` from it against the database, then restart (or roll) `serve`. It
downloads nothing, so it works the same in a sealed network.

```sh
maverick upgrade --check   # preflight and plan only: what change control reviews
maverick upgrade           # asks before it applies (in a terminal)
maverick upgrade --yes     # applies without asking (automation)
maverick upgrade --json    # the same report as one JSON object
```

It stops at the first failure and names it:

1. **Preflight** (read only): the schema state and the release that last upgraded the
   database, then the checks in order: newer database, upgrade window, the active
   bundle's format (a warning, not a stop), the upgrade lock (a Postgres advisory lock,
   so two runs never overlap), and serve processes (a `needs-downtime` migration is
   refused while any `serve` heartbeat is younger than 2 minutes).
2. **Plan**: every pending engine and job-queue migration with its class.
3. **Apply**: the same code as `maverick migrate`, while the run holds the lock.
4. **Record**: an `upgrades` row (from and to release, operator `cli:<os user>`, the
   migrations, start and finish times).
5. **Verify**: the checks again with nothing pending, then one row from each table an
   applied migration created or altered. A failure exits 1 and is recorded in the row's
   `verify_failed`.

Without `--yes` and without a terminal, it applies nothing and exits 1. It exits 0 when
the database is upgraded, already current, or (with `--check`) would upgrade.

`serve` never migrates. At startup it refuses a database with pending engine or
job-queue migrations (`run maverick upgrade`) or one newer than the release. When it
cannot reach the database within 5 s it starts anyway, with lazy connections, as before.
While it runs, each `serve` process refreshes a `serve_heartbeats` row every 30 s (its
release and host) and removes it on a clean shutdown. A row older than 2 minutes counts as
stopped.

## Jobs and schedules

The jobs run on graphile_worker, in Postgres (no Redis). The periodic jobs use five-field
cron expressions evaluated in **UTC**. The workers run inside `maverick serve`: skip them
with `--api-only`, or run them alone with `--worker-only` (exactly one such process).

| Job | Default | Variable | What it does |
|---|---|---|---|
| `prediction_lock` | `50 2 * * *` | `MAVERICK_CRON_PREDICTION_LOCK` | Stores the pass probabilities that the EWMAs imply for tonight's cells, before the results exist. |
| `nightly_smoke` | `0 3 * * *` | `MAVERICK_CRON_NIGHTLY` | The smoke tasks for every non-retired column, 3 repeats. Cold-start columns run the full corpus. |
| `weekly_sweep` | `0 4 * * 0` | `MAVERICK_CRON_WEEKLY` | The full active corpus for every non-retired column, 3 repeats. |
| `latency_probes` | `0 * * * *` | `MAVERICK_CRON_LATENCY_PROBES` | Light pings for columns with a `quality_source_column_id`. They feed latency cells only. |
| `aggregate_rebuild` | chained | | After each batch: recompute the cells, fold the EWMAs, append aggregates, update `measured`. |
| `regression_scan` | chained | | After each rebuild: the spec §6.5 regressions and the parse-failure watch. |
| `calibration_score` | chained | | After each rebuild: Brier score and overconfidence against the locked predictions. |
| `price_sync` | `0 6 * * *` | `MAVERICK_CRON_PRICE_SYNC` | Refreshes the pricing snapshots (see Pricing). |
| `catalog_sync` | `10 6 * * *` | `MAVERICK_CRON_CATALOG_SYNC` | The lifecycle transitions, the pool-drift check, and `retention_expiring`. |
| `model_discovery` | `20 6 * * *` | `MAVERICK_CRON_MODEL_DISCOVERY` | Lists the text models of each provider with an operator key and raises `new_model_available` and `model_withdrawn` (see "New models"). It never changes the catalog. |
| `capture_expiry` | `0 * * * *` | `MAVERICK_CRON_CAPTURE_EXPIRY` | Hard-deletes expired captures. |
| `history_prune` | `0 5 * * *` | `MAVERICK_CRON_HISTORY_PRUNE` | Enforces the history retention. |

- **Chaining:** `nightly_smoke` and `weekly_sweep` enqueue `aggregate_rebuild`, which
  enqueues `regression_scan` and `calibration_score`. A partly failed batch still rebuilds
  from what completed, and a completion below 90% raises `job_failure`. Keep
  `prediction_lock` before the nightly batch: both key the same UTC date.
- **One execution per window (D10):** the engine owns the uniqueness, not the queue. Each
  handler first takes a lease on its `batches` row (`MAVERICK_BATCH_LEASE_TTL`, default
  60 s, renewed every `MAVERICK_BATCH_HEARTBEAT_INTERVAL`, default 15 s). A completed row
  makes a repeat a no-op, and a crashed worker's batch resumes under the same id without
  repeating the runs it stored.
- **Timeouts and attempts:** batches run under `MAVERICK_BATCH_JOB_TIMEOUT` (default 6 h, at
  most 24 h). The chained and hourly jobs run under `MAVERICK_ANALYTICS_JOB_TIMEOUT`
  (default 15 min), and the other jobs under 60 s. Batches get 3 attempts, the other jobs 5.
- **No lost failure tail (D11):** the failure alert and the follow-up rebuild are written in
  the same transaction that records the batch outcome.

## Live provider smoke test

Spec §15 allows one test to call real providers: an opt-in, spend-capped live smoke. It is
`crates/maverick-runner/tests/live_smoke.rs`:

```sh
export MAVERICK_ANTHROPIC_API_KEY=...   # keys for the two columns' providers
export MAVERICK_OPENAI_API_KEY=...
scripts/live-smoke.sh
```

- **Opt-in twice.** The test is ignored by default, and it runs only with
  `MAVERICK_LIVE_SMOKE=1` (the script sets it) **and** the providers' keys. `cargo test` and
  CI never reach a real provider. CI runs its twin, which points the real clients at a local
  scripted server.
- **What it runs:** 2 built-in tasks for 2 columns, 1 repeat: 4 real calls. The tasks are
  built in, not taken from the corpus, and the test needs no database. The default columns
  are `anthropic/claude-haiku-4-5` and `openai/gpt-5.4-mini`. Override them with
  `MAVERICK_LIVE_SMOKE_COLUMNS`: exactly two comma-separated
  `provider:model_id:input_per_mtok:output_per_mtok` entries.
- **Spend cap:** before any call, the projection uses the batch spend formula; above
  **$1.00** it stops with no call made. The actual cost is added after every call and
  checked against the same cap.
- **Shape only:** it checks that each call succeeds, the output is not empty, the tokens and
  the latency are above 0, and the cost is under the cap. It never checks scores or quality.
- Bead 9.9 runs it once with the owner's keys before go-live. Nothing schedules it; run it
  from a machine with a checkout, outside the deployment it tests.

## Process shapes and observability

- One binary, three shapes: `serve` (API, MCP, and workers), `serve --api-only` (stateless;
  run several), and `serve --worker-only` (exactly one). Every shape serves the internal
  listener.
- **Health:** `GET /up` (public, no auth; the deploy health check) and `GET /health/detail`
  (internal listener). The detail has the components `db`, `job_queue` (`ok`,
  `no_workers`, `stale`, or `unknown`; D35), and `provider_client_config`, plus the active
  matrix version and the last batch times. The shape is in `docs/api.md`.
- **Logs:** one JSON line per event on stdout, in the Go `slog` shape: every request with
  method, path, status, duration, and ids; every batch with its projected and actual spend
  and its run counts; every provisioning write with `audit=true` and the token label.
  Prompts, completions, request bodies, and credentials are never logged (G5).
- **OpenTelemetry (D19):** with `MAVERICK_OTEL_EXPORTER_ENDPOINT` set, the engine exports
  its spans and these metrics over OTLP/HTTP: `maverick_recommend_duration_ms`,
  `maverick_recommendations_total`, `maverick_classifier_duration_ms`,
  `maverick_classifier_cache_total`, `maverick_classifier_fallback_total`,
  `maverick_provider_calls_total`, `maverick_provider_retries_total`,
  `maverick_queue_depth`, `maverick_queue_oldest_job_age_seconds`,
  `maverick_job_duration_ms`, `maverick_job_failures_total`,
  `maverick_job_recoveries_total`, `maverick_batch_spend_usd`, and
  `maverick_affinity_decisions_total` (D44). There is no metrics endpoint to scrape. The proxy exports its own records with `MAVERICK_PROXY_OTLP_ENDPOINT`
  (D31).
- **The proxy** (`crates/maverick-proxy`) is a separate binary and deployable. It has
  no database and no migrations. See "The proxy in production" below and
  `docs/features/proxy.md`.

## The proxy in production

The proxy is a production path (D46). It fails open: during an engine outage it serves the
tenant key's last good list for the route, then the surface's default model, then
`503 engine_unavailable` (spec §8.1).

- **Health.** `GET /up` answers 200 without calling the engine. An engine outage does not
  take a replica out of rotation while fail-open serves traffic.
- **Replicas.** The proxy is stateless apart from its in-memory last good store, so it can
  run more than one replica (`config/deploy-proxy.yml`). Each replica fills its own store
  from its own traffic. A new or restarted replica starts cold: until it has served a
  route, an engine outage on that route goes to the default model or to the `503`.
- **Availability target** (proposed 2026-10-24; the owner confirms the number): 99.9% of
  proxy requests in a 30-day window get an answer that is not `503 engine_unavailable` and
  not a `502` the proxy made itself (`engine_error`, `no_executable_candidates`). A
  provider's own error, relayed to the caller, does not count against it.
- **Alerts** on `maverick_proxy_fallbacks_total`, exported over OTLP when
  `MAVERICK_PROXY_OTLP_ENDPOINT` is set. Its only label is `step`.
  - Any `step="unavailable"` pages: callers got `503 engine_unavailable`. Check the engine
    first (`/up`, `/health/detail`), then whether the proxy has a default model.
  - `step="last_known_good"` rising for more than 10 minutes warns: the engine is down or
    failing, and callers run on cached lists that expire after
    `MAVERICK_PROXY_LKG_MAX_AGE`.
  - Any `step="default_model"` warns: a cold route fell back to the configured default,
    which is not a recommendation.
- **Settings** (spec §14.1): `MAVERICK_PROXY_RATELIMIT_RPM` (default 600 per tenant key; a
  routed request also counts against the engine's per-key limit, `MAVERICK_RATELIMIT_RPM`,
  300 by default), `MAVERICK_PROXY_LKG_MAX_AGE` (default `24h`),
  `MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES` and `MAVERICK_PROXY_DEFAULT_MODEL_CHAT` (unset),
  and `MAVERICK_PROXY_DEFAULT_MODEL_ZERO_RETENTION` (`false`). Set it to `true` only when
  both default models have verified zero-retention evidence: a zero-retention tenant may
  then fall open to them.
- **Logs.** Each engine failure is one ERROR line, `maverick-proxy: engine recommend
  failed`, with the error and, when the proxy fell open, the step. The drill is in
  `docs/runbook-go-live.md` §8.

## Conversation affinity (D44)

`POST /v1/recommend` can carry an `affinity` key that ties the turns of one conversation
together (spec §7.6). The proxy sends it for `X-Maverick-Conversation`, and the SDK cores
for `conversation_id`, but only to an engine that lists `affinity` in
`X-Maverick-API-Features` (D50).

- **State.** One `conversation_affinity` row per tenant and conversation, keyed by the
  per-tenant HMAC of the caller's key. The key itself is never stored or logged. Each
  recommendation that carried `affinity` also stores its decision in
  `recommendations.affinity` (`GET /v1/history/recommendations/{id}` returns it).
- **Settings** (spec §14.1), read at startup:
  - `MAVERICK_AFFINITY_ENABLED` (default `true`). `false` still validates the field but
    reads and writes no row and answers `status: "new"` with the normal ranking.
  - `MAVERICK_AFFINITY_TTL` (default `24h`, at most `720h`): a row's lifetime after its
    last use.
  - `MAVERICK_AFFINITY_CACHE_WARM_SECONDS` (default `300`): how long the pinned column's
    provider cache counts as warm.
  - `MAVERICK_AFFINITY_INCUMBENT_BONUS` (default `1.5` quality points; `0` turns the bonus
    off and keeps the cache-aware turn cost).
- **Tenant setting.** `affinity_cost_aware` in the constraints payload (default
  `false`). With it off, affinity never changes a pick; it only reports `kept` or
  `switched`.
- **Metric.** `maverick_affinity_decisions_total` by `status` (`new`, `kept`, `switched`,
  `broken`) and `cost_aware`. No key, hash, or column id is a label. A rising share of
  `switched` with `cost_aware="false"` shows how often tenants would gain from turning
  the setting on; a rising `broken` share points at pool or retention changes.
- **Telemetry follow.** A 2xx telemetry item from a chain entry after the pick moves the
  row to that entry's column (spec §9.10). A failed move is a WARN line,
  `telemetry: following the served column failed`, with the recommendation id only.
- **Expired rows** read as missing. The hourly `affinity_prune` job that deletes them is
  bead gcd2.8 and does not run yet; until it does, expired rows stay in the table.

## Hot-path latency

Spec §2.3 sets the budget: the declared recommend path reads precomputed aggregates, inserts
one trace row, and calls no provider, with a **p95 below 50 ms** in the region. A
prompt-form request with a classification-cache hit must meet the same budget. On a cache
miss the classifier call dominates. That target is a **p95 below 1.8 s** (D20; the Go
target was 2.5 s), inside the SDK's 2 s timeout, and the classifier deadline is 1.5 s (D3).

`scripts/latency.sh` measures the hot path (bead 9.3). It builds release binaries, seeds a
realistic catalog (12 columns, a week of aggregates per cell), and runs `serve --api-only`
and the proxy against the fake provider. It prints p50, p95, and p99 for:

- the declared path, with one client and with several;
- the uncached prompt path, with a fake classifier latency you set
  (`MAVERICK_LATENCY_CLASSIFIER_MS`), plus one call past the classifier deadline;
- the proxy's added time to first byte.

The report goes to `target/latency/<timestamp>/report.json`. It also shows an environment
floor (a Postgres round trip, a commit, and `/up`) and the API's CPU time and database
transactions per request, so a busy host can be told apart from engine cost. By default the
script only reports. `MAVERICK_LATENCY_ASSERT=1` asserts the targets; run that gate on a
quiet, production-like host.

The first measurement (2026-09-29, on the shared development host at load average 15 to 30)
met the prompt-path target: p95 1.26 s with a 600 ms classifier. It missed the declared
target: p95 236 ms with one client. The declared path costs about 16 to 19 ms of API CPU and
10 database transactions per request. Bead `model-maverick-e3x` cuts that cost.

## Local checks

- `scripts/docker-smoke.sh` builds both images and runs Postgres, the engine, the hosted
  proxy, and a fake upstream: the migration, the health checks, one proxy request, and the
  backup and restore test. No real provider is called.
- `maverick demo-run` (hidden) seeds `scripts/demo/seed.sql` and runs one batch against the
  deterministic fake provider (`scripts/demo/fixtures`). It is the quickest check of the
  measurement path.

## Deployment

Production is one server deployed with Kamal 2. `config/deploy.yml` (the engine) and
`config/deploy-proxy.yml` (the hosted proxy) are commented walkthroughs, and
`docker/README.md` has the steps.

- `docker/Dockerfile` builds static binaries into distroless nonroot images. The engine
  image runs every shape: `serve`, `serve --api-only`, `serve --worker-only`, and `migrate`.
- kamal-proxy routes the **public** listener only (`app_port: 8080`), with automatic HTTPS
  and the `GET /up` check. The web role runs `serve --api-only`, and one `worker` role runs
  `serve --worker-only`.
- The **internal** listener (:8081) has no proxy route, and no container publishes it on the
  host. Open a tunnel with `scripts/internal-tunnel.sh <user>@<host>` and call
  `http://127.0.0.1:8081/internal/v1/...` through it. The script finds the running web
  container's address on the `kamal` network, so run it again after each deploy.
- Postgres 16 is the `postgres` accessory on the same host. Its data is in the host
  directory `~/maverick-postgres/postgres-data` (a Kamal accessory directory, not a named
  volume), and its port is on the host loopback only. `kamal deploy` does not touch
  accessories. `kamal accessory remove postgres` deletes the data.
- **Migrations:** the `.kamal/hooks/pre-app-boot` hook runs `maverick migrate` from the
  version being deployed before traffic moves. `serve` never migrates, so any path that
  skips the hook (a plain `docker run`, a manual restart) must run `maverick migrate` first.
  `kamal migrate` runs it by hand. `migrate` loads the same required configuration as
  `serve` (`MAVERICK_DATABASE_URL`, `MAVERICK_INTERNAL_TOKENS`,
  `MAVERICK_CAPTURE_MASTER_KEY`), though it only touches the database.
- **Secrets** come from `.kamal/secrets`, a committed file of environment references with
  **no values**. Export the values in the deploying shell, or fetch them with
  `kamal secrets fetch` from a password manager.

The first deploy follows `docs/runbook-go-live.md` step by step. It boots Postgres on its
own before `kamal setup`, because the first start of the image restarts once, and a
migration during that window fails. Releases: `kamal deploy` (and
`kamal deploy -c config/deploy-proxy.yml` for the proxy).
After each deploy:
- check that `https://<host>/up` answers 200 through kamal-proxy;
- check that port 8081 cannot be reached from outside;
- watch the first production `nightly_smoke` complete under the spend caps (`kamal app
  logs` and the alerts feed).

Bead 9.5 runs `docs/runbook-go-live.md` on the real host with the owner.
