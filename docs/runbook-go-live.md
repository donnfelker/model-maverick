# Go-live runbook

This runbook takes the Rust engine and the hosted trial proxy live on the deploy host.
- Bead `model-maverick-rust-port-3hk.9.12` wrote it.
- Bead `model-maverick-rust-port-3hk.9.5` runs it with the owner.
- It covers master plan §7.0 step 4.

Go-live is a **fresh install** (owner decision, bead 10.4). Rust starts with a new, empty
database:
- no Go data to back up, and no adopt command;
- no `batches` backfill, and no River tables;
- no cutover and no rollback to Go, because the Go engine never shipped.

**Every step marked [owner] is outward-facing:** it touches the host, the registry, DNS, or
a provider. The owner confirms each one before it runs. The other steps run on the deploying
machine and change nothing outside it.

This file holds no real host, key, or corpus content. `<HOST>`, `<USER>`,
`<REGISTRY_USERNAME>`, `maverick.example.com`, and `proxy.example.com` are placeholders.

Related documents:
- `docker/README.md`: the images, Kamal, and the backups.
- `config/deploy.yml` and `config/deploy-proxy.yml`: the commented configurations.
- `docs/operations.md`: day-to-day operations.
- `docs/api.md`: the request bodies.

## 0. Gates

Do not start until every gate is true.

| Gate | Where |
|---|---|
| The deploy host and Kamal access | bead 10.3 |
| The provider keys (Anthropic, OpenAI, Google, xAI) and the approved spend | bead 10.2 |
| Read access to the private corpus repo `donnfelker/model-maverick-corpus` | bead 10.1 (done) |
| DNS A records for the engine name and the proxy name, pointing at the host | owner |
| `main` is green in CI | GitHub Actions |
| Bead 9.5 has no open blocker except the owner inputs | `br show model-maverick-rust-port-3hk.9.5` |
| The live smoke has passed once with the owner's keys | bead 9.9, `scripts/live-smoke.sh` |
| The local image smoke passes | `scripts/docker-smoke.sh` |

The deploying machine needs:
- Kamal 2 (this runbook was checked against the Kamal 2.12 source);
- Docker, `ssh` to the host, and push access to the registry;
- an SSH user on the host that can run `docker`.

**The host firewall.** It admits only SSH (for the operators), 80, and 443.
- Docker-published ports bypass a host firewall such as ufw.
- In these configurations, only kamal-proxy publishes public ports (80 and 443). The Postgres
  accessory publishes on the host loopback only.
- Never add a `publish` option to an app role (bead 9.13).

## 1. Secrets and configuration

1. **Placeholders.** Replace them in `config/deploy.yml` and `config/deploy-proxy.yml` on the
   deploying machine: `<HOST>`, `<REGISTRY_USERNAME>`, `maverick.example.com`, and
   `proxy.example.com`. Keep the replaced values out of commits.
   - Kamal connects as `root` unless the file sets `ssh: { user: ... }`.
   - `<USER>` below is that user. Paths such as `.kamal/apps/...` on the host are relative
     to its home directory.
2. **Spend caps.** In `env.clear` of `config/deploy.yml`, set the caps the owner approved
   (bead 10.2):
   - `MAVERICK_SPEND_NIGHTLY_CAP_USD` (default 100);
   - `MAVERICK_SPEND_WEEKLY_CAP_USD` (default 500).

   Each new column runs the **full** corpus every night for its first 5 nights (section 5).
   Size the nightly cap for that. A batch whose projection is over a cap does not run, and it
   raises a critical `spend_refusal` alert (`docs/operations.md` "Spend caps").
3. **Classifier limits.** They stay at their defaults, which are the owner's placeholders.
   Bead 9.8 retunes them.
   - `MAVERICK_CLASSIFIER_DAILY_QUOTA`: 1000 classifications per tenant per day;
   - `MAVERICK_CLASSIFIER_DAILY_SPEND_CAP_USD`: $5.00 per day in total.
4. **Alert channels** (optional; `docs/operations.md` "Alerts"):
   - `MAVERICK_SMTP_URL` (a secret);
   - `MAVERICK_ALERTS_WEBHOOK_URL` (clear), with `MAVERICK_ALERTS_WEBHOOK_SECRET` (a secret,
     required when the URL is set).
5. **New secrets.** Generate them, and store every value in the password manager **before**
   the first deploy. The capture master key and the internal tokens are not in the database,
   so no backup can restore them.

   ```sh
   openssl rand -hex 24                 # POSTGRES_PASSWORD
   openssl rand -hex 32                 # MAVERICK_CAPTURE_MASTER_KEY (64 hex characters)
   echo "ops:$(openssl rand -hex 32)"   # a MAVERICK_INTERNAL_TOKENS entry, label:secret
   ```

6. **Export.** Export these values in the deploying shell. `.kamal/secrets` reads each one
   from the environment and holds no value itself.

   | Variable | Value |
   |---|---|
   | `KAMAL_REGISTRY_PASSWORD` | a registry token that can push the images |
   | `POSTGRES_PASSWORD` | from item 5 |
   | `MAVERICK_DATABASE_URL` | `postgres://maverick:<POSTGRES_PASSWORD>@maverick-postgres:5432/maverick` |
   | `MAVERICK_INTERNAL_TOKENS` | from item 5; separate several entries with commas |
   | `MAVERICK_CAPTURE_MASTER_KEY` | from item 5; `deploy.yml` sets key version 1 |
   | `MAVERICK_CAPTURE_PREVIOUS_MASTER_KEY` | empty |
   | `MAVERICK_ANTHROPIC_API_KEY`, `MAVERICK_OPENAI_API_KEY`, `MAVERICK_GOOGLE_API_KEY`, `MAVERICK_XAI_API_KEY` | the operator keys from bead 10.2, for benchmarks and classification only, never customer traffic |
   | `MAVERICK_SMTP_URL`, `MAVERICK_ALERTS_WEBHOOK_SECRET` | from item 4, or empty |
   | `MAVERICK_BACKUP_TARGET`, `RCLONE_CONFIG_OFFSITE_*` | the off-box backup remote (section 4) |

7. **Check:** both commands print the resolved configuration without contacting the host:

   ```sh
   kamal config
   kamal config -c config/deploy-proxy.yml
   ```

   No placeholder is left, and no app role has a `publish` option. With these files, the
   output lists the secret names but not their values (checked with Kamal 2.12).

## 2. Postgres [owner]

Boot Postgres by itself first. On its first start, the image initializes the data directory
with a temporary server, and then restarts. A migration that runs during that window fails.

```sh
kamal server bootstrap            # installs Docker on the host when it is missing
kamal accessory boot postgres
kamal accessory exec postgres --reuse "pg_isready -h 127.0.0.1 -U maverick -d maverick"
```

- Repeat the `pg_isready` line until it reports `accepting connections`.
- `-h 127.0.0.1` checks over TCP. The temporary init server listens on the Unix socket only,
  so a TCP answer comes from the real server.
- `--reuse` runs the command in the running container. Without it, Kamal starts a second
  container with the same options.
- The data is in the host directory `~/maverick-postgres/postgres-data`, a Kamal accessory
  directory. **`kamal accessory remove postgres` deletes it.**

## 3. The engine [owner]

```sh
kamal setup
```

`kamal setup` runs these steps:
1. It builds and pushes the image, and boots kamal-proxy.
2. It boots the `backup` accessory. It skips `postgres`, which already exists.
3. It runs `.kamal/hooks/pre-app-boot`: `maverick migrate` with the new image, which applies
   the sqlx migrations and the graphile_worker schema.
4. It boots the `web` role (`serve --api-only`) and the one `worker` role
   (`serve --worker-only`).

**Checks:**

1. The public listener, through kamal-proxy with automatic HTTPS:

   ```sh
   curl -sS https://maverick.example.com/up
   ```

2. The internal listener. It has no route and no published port. Open a tunnel in a second
   terminal (run it again after every deploy):

   ```sh
   scripts/internal-tunnel.sh <USER>@<HOST>
   ```

   Then read the component health. `$MAVERICK_TOKEN` is the token from section 1, item 5:
   the whole `label:secret` entry.

   ```sh
   curl -sS http://127.0.0.1:8081/health/detail -H "Authorization: Bearer $MAVERICK_TOKEN"
   ```

   Expect `status: "ok"`, with these components:
   - `db` is `ok`;
   - `job_queue` is `ok` (the worker is registered and has a heartbeat newer than 60 s;
     `no_workers` or `stale` means the worker role is not running);
   - `provider_client_config` names the four providers.

   `active_matrix_version` is `null` until section 6.

The later examples use `INTERNAL=http://127.0.0.1:8081/internal/v1`, and
`AUTH="Authorization: Bearer $MAVERICK_TOKEN"`.

## 4. Prove the backup [owner]

The `backup` accessory runs `scripts/backup.sh --daemon`:
- a dump every night at 02:30 UTC, into the host directory
  `~/maverick-backup/maverick-backups` (mounted at `/backups`);
- 7 dumps are kept;
- an rclone copy off the box, when `MAVERICK_BACKUP_TARGET` is set.

Prove it now, on the migrated schema. Do not wait for the first night.

1. `kamal accessory logs backup` shows `backup daemon started`. It must not show
   `cannot install rclone`.
2. Take one dump now, in the running container:

   ```sh
   kamal accessory exec backup --reuse "sh /usr/local/bin/maverick-backup"
   ```

   It logs `backup written`, and then `backup copied off the box`. If it logs
   `backup target unset`, the dump exists on the host only. Set the target before go-live.
3. Run the restore test on the host. It restores the newest dump into a scratch database,
   compares every table's row count with the dump's `.counts` file, runs `maverick migrate`
   on the result, and drops the scratch database.
   - Copy the script to the host: `scp scripts/restore-test.sh <USER>@<HOST>:`
   - Then run these commands on the host, in the SSH user's home directory:

   ```sh
   web=$(docker ps --quiet --filter label=service=maverick --filter label=role=web | head -n 1)
   image=$(docker inspect --format '{{.Config.Image}}' "$web")
   tmp=$(docker create "$image") && docker cp "$tmp":/usr/local/bin/maverick ./maverick && docker rm "$tmp"
   docker run --rm --network kamal \
     --env-file .kamal/apps/maverick/env/roles/web.env \
     -e MAVERICK_BACKUP_DIR=/backups -e MAVERICK_BIN=/usr/local/bin/maverick \
     -v "$PWD/maverick-backup/maverick-backups:/backups" \
     -v "$PWD/restore-test.sh:/restore-test.sh:ro" \
     -v "$PWD/maverick:/usr/local/bin/maverick:ro" \
     postgres:16-alpine sh /restore-test.sh
   ```

   It ends with `restore test passed`.
   - `.kamal/apps/maverick/env/roles/web.env` is the secrets file that Kamal wrote for the
     web role, under the SSH user's home.
   - It has the database URL, the internal tokens, and the capture master key, which
     `maverick migrate` needs.

## 5. The catalog [owner]

Create one catalog column per model on the owner's launch list, for all four providers,
through the tunnel. The body is in `docs/api.md` "POST /internal/v1/columns":

```sh
curl -sS -X POST "$INTERNAL/columns" -H "$AUTH" -H 'Content-Type: application/json' -d '{
  "provider": "anthropic", "model_id": "<model id>", "endpoint": "direct", "settings": {},
  "pricing": { "input_per_mtok": <USD>, "output_per_mtok": <USD> },
  "retention_mode": "standard",
  "retention_verified_at": "<RFC 3339>", "retention_valid_until": "<RFC 3339>",
  "retention_source": "<where the evidence is>",
  "execution_aliases": { "openrouter": "<provider>/<model id>" }
}'
```

- **Pricing** is the list price in USD per million tokens. The daily `price_sync` job changes
  it only through a `MAVERICK_MODELS_DEV_PRICE_MAP` (`docs/operations.md` "Pricing").
- **Retention evidence.** `retention_mode` is `zero` only with current evidence, and the
  window is at most 90 days (`docs/operations.md` "Zero-retention evidence").
  - `execution_alias_retention` carries the same fields for each OpenRouter alias.
  - A zero-retention tenant never reaches a column or an alias without valid evidence.
- **Parameter constraints** (`param_constraints`) go on the columns that need them, where
  known (D29).
- **Lifecycle.** Every new column starts as `cold_start` for 5 nights
  (`MAVERICK_CATALOG_COLD_START_NIGHTS`), and it runs the full corpus every night until then.

**The classifier and the judge pool must resolve.** The catalog must hold the columns that
these references name:
- `MAVERICK_CLASSIFIER_COLUMN` (default `claude-haiku-4-5/direct`);
- `MAVERICK_JUDGE_POOL` (default `gemini-3.1-pro/direct,gpt-5.5/direct`; two families);
- `MAVERICK_CLASSIFIER_ZERO_RETENTION_COLUMN`, when it is set (D2).

If the launch list differs, set those variables in `env.clear` to `model_id/endpoint`
references that exist.

**Restart after the catalog.** Until bead `model-maverick-yl6` lands, the web role resolves
the classifier column only at startup, and the worker role resolves the judge pool only at
startup. Reboot both after the columns exist:

```sh
kamal app boot
```

Then check the logs. Neither line may appear:
- `classifier column unavailable`;
- `judge pool unavailable`.

```sh
kamal app logs --roles=web
kamal app logs --roles=worker
```

Reopen the tunnel after the reboot, because the web container has a new address.

## 6. The corpus [owner]

1. On the host, clone the corpus **outside** any checkout of this repository. Make its files
   readable by all users: the image runs as the distroless `nonroot` user.

   ```sh
   git clone git@github.com:donnfelker/model-maverick-corpus.git /srv/maverick-corpus
   ```

2. Load one version. Run this on the host, in the SSH user's home directory. Loading
   validates every task before it writes to the database, and `--strict` turns the shape
   warnings into errors. The command uses the web role's image and secrets, and it mounts
   the corpus read-only:

   ```sh
   web=$(docker ps --quiet --filter label=service=maverick --filter label=role=web | head -n 1)
   image=$(docker inspect --format '{{.Config.Image}}' "$web")
   docker run --rm --network kamal --env-file .kamal/apps/maverick/env/roles/web.env \
     -v /srv/maverick-corpus:/corpus:ro "$image" \
     corpus load --version <version> --dir /corpus/corpus --description "<text>" --strict
   ```

   - **Expect:** `loaded corpus version <version>: 221 tasks (inactive; ...)`, with no
     `warning:` and no `error:` line.
   - Any `error:` line means that nothing was written.

3. Activate the version, and check it. These commands need no corpus files, so the `kamal`
   commands work from the deploying machine:

   ```sh
   kamal app exec --roles=web --primary "corpus activate --version <version>"
   kamal app exec --roles=web --primary "corpus status"
   ```

   - `corpus status` lists the version with 221 tasks and `true` in the ACTIVE column.
   - `/health/detail` now reports it as `active_matrix_version`.

## 7. Tenant #1 [owner]

1. Create the owner's tenant:

   ```sh
   curl -sS -X POST "$INTERNAL/tenants" -H "$AUTH" -H 'Content-Type: application/json' \
     -d '{"name": "<owner tenant name>"}'
   ```

   A new tenant's `default` pool holds every `active` column. At go-live every column is
   still `cold_start`, so the tenant has **no pool** yet.
2. Give it a default pool with the launch columns (the `column_id` values from section 5):

   ```sh
   curl -sS -X PUT "$INTERNAL/tenants/<tenant id>/pools" -H "$AUTH" \
     -H 'Content-Type: application/json' \
     -d '{"pools": [{"name": "default", "model_column_ids": ["<col id>", "..."], "is_default": true}]}'
   ```

3. Issue an API key. **The raw key appears once, in this response.** Store it in the
   password manager straight away. No endpoint can return it again.

   ```sh
   curl -sS -X POST "$INTERNAL/tenants/<tenant id>/api-keys" -H "$AUTH" \
     -H 'Content-Type: application/json' -d '{"name": "owner"}'
   ```

4. **Check** with a declared recommendation. The declared form makes no classifier call.
   Before the first nightly, every cell is unmeasured, so the chain carries
   `measured: false`.

   ```sh
   curl -sS https://maverick.example.com/v1/recommend -H "Authorization: Bearer $TENANT_KEY" \
     -H 'Content-Type: application/json' \
     -d '{"workload": {"capability": "structured_extraction", "difficulty": 3}}'
   ```

## 8. The hosted trial proxy [owner]

The proxy runs in hosted mode, as its own Kamal service (`config/deploy-proxy.yml`):
- it holds no MM key and no provider key (D12, D13);
- its per-tenant rate limit is `MAVERICK_PROXY_RATELIMIT_RPM` (default 60);
- it calls the engine's **public** URL.

```sh
kamal setup -c config/deploy-proxy.yml
curl -sS https://proxy.example.com/up
```

**Smoke.** Send one chat request and one Messages request. Each carries the tenant key and a
caller provider key (the owner's own keys, from the environment). They are real provider
calls, so the owner confirms them.
- The commands print only the response headers.
- `x-maverick-model-used` names the routed model.
- `x-maverick-recommendation-id` names the recommendation in the tenant's history.

```sh
curl -sS -D - -o /dev/null https://proxy.example.com/v1/chat/completions \
  -H "X-Maverick-API-Key: $TENANT_KEY" -H "Authorization: Bearer $CALLER_OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model": "maverick/auto", "max_tokens": 16, "messages": [{"role": "user", "content": "Reply with OK."}]}'

curl -sS -D - -o /dev/null https://proxy.example.com/v1/messages \
  -H "X-Maverick-API-Key: $TENANT_KEY" -H "x-api-key: $CALLER_ANTHROPIC_KEY" \
  -H 'anthropic-version: 2023-06-01' -H 'Content-Type: application/json' \
  -d '{"model": "maverick/auto", "max_tokens": 16, "messages": [{"role": "user", "content": "Reply with OK."}]}'
```

- Both requests answer `200`.
- The chat surface runs OpenAI and OpenAI-compatible entries. The Messages surface runs
  Anthropic entries only. So the pool needs a column of each kind.
- The proxy strips `X-Maverick-API-Key` before any upstream call, and it never translates
  between wire formats.

## 9. The nightly schedule

The worker registers its cron triggers in graphile_worker. The schedules are in UTC:
- `prediction_lock` at 02:50;
- `nightly_smoke` at 03:00;
- the full table is in `docs/operations.md` "Jobs and schedules".

1. **Check them before the first night:**

   ```sh
   kamal accessory exec postgres --reuse \
     "psql -U maverick -d maverick -c 'select identifier, known_since, last_execution from graphile_worker._private_known_crontabs order by identifier'"
   ```

   Each scheduled kind is listed, including `prediction_lock` and `nightly_smoke`.

2. **After the first night:**
   - `/health/detail` shows `last_batch_times.nightly_smoke`.
   - `GET $INTERNAL/alerts` has no `spend_refusal` and no `job_failure`.
   - `kamal app logs --roles=worker` has the batch's projected and actual spend.

The first live nightly under the spend cap is bead 9.6.

## Rollback

There is no Go fallback. A failed step is fixed forward in Rust, and then deployed again.

- **Before section 7,** no tenant data exists. Stop the app with `kamal app stop`, fix the
  cause, and deploy again.
- **After section 7,** restore the newest dump [owner]: the one from section 4, or a nightly
  one. The dumps are `~/maverick-backup/maverick-backups/maverick-<UTC time>.dump` on the
  host.
  1. Stop the app with `kamal app stop`.
  2. On the host, in the SSH user's home directory, restore the dump into the `maverick`
     database:

     ```sh
     docker run --rm --network kamal --env-file .kamal/apps/maverick/env/roles/web.env \
       -v "$PWD/maverick-backup/maverick-backups:/backups:ro" postgres:16-alpine \
       sh -c 'pg_restore --clean --if-exists --no-owner --dbname "$MAVERICK_DATABASE_URL" /backups/<dump file>'
     ```

  3. Deploy the fixed version with `kamal deploy`.
- The capture keys and the internal tokens come from the password manager, not from the
  dump.

When the steps are done, record the result in the close reason of bead 9.5: the date, the
engine and proxy versions, and the corpus version.
