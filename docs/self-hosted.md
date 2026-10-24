# Self-hosted install and upgrade

This guide installs Model Maverick on your own infrastructure and keeps it current. It is
for the operator of a self-hosted install. Bead `model-maverick-xq7v.13` wrote it. Spec
§14.4 (roles, bundles, single-tenant replicas) and §14.5 (versions and upgrades) are the
authority, and D49 and D50 in `parity/DIVERGENCES.md` record the decisions.

A self-hosted install is a **replica** (`MAVERICK_ROLE=replica`):

- It serves one organization. It has one tenant, created on first start, and you never
  create or manage tenants.
- It runs no benchmarks. Maverick's hosted origin benchmarks every model against a private
  corpus each night and publishes the results as signed **matrix bundles**. The replica
  imports them and ranks models from them. No corpus ever reaches your install.
- It needs no license key. The engine is open source, and the bundles are free and public.
- It makes no outbound connection that you did not configure (section 9).

Other pages this guide points to:

- `docs/operations.md`: day-to-day operations for both roles, in more detail.
- `docs/api.md`: the request and response bodies.
- `docs/matrix-bundles.md`: the bundle format.
- `docker/README.md` "Releases": the release files and how to verify them.

## 1. What you need

- **A Linux host** on x86_64, or a container platform. The images are built for
  `linux/amd64`.
- **Postgres 16**, with one database for Maverick. It holds all state, including the job
  queue. There is no Redis.
- **A release** (section 2): the container images, the static binary tarball, or the
  offline install kit.
- **Provider keys, if you want them.** The engine calls a model provider only for
  prompt-form recommends (the classifier) and latency probes, and only with keys you set.
  Declared-workload recommends need no provider key. Your applications keep their own
  provider credentials: the engine never executes inference.
- **The matrix bundle files**, or HTTPS access to the public bundle location (section 6).

The engine listens on two ports:

| Listener | Default | Serves | Who reaches it |
|---|---|---|---|
| Public | `:8080` (`MAVERICK_PUBLIC_ADDR`) | `/v1/*`, `/mcp`, `/up` | your applications, with an API key |
| Internal | `:8081` (`MAVERICK_INTERNAL_ADDR`) | `/health/detail`, `/internal/v1/*` | operators only, with an internal token |

Keep the internal listener off any network your applications share.

## 2. Get and verify a release

Each release has these files (`docker/README.md` "Releases" lists them all):

- the images `maverick:<v>` and `maverick-proxy:<v>`;
- `maverick-<v>-x86_64-unknown-linux-musl.tar.gz`: the static `maverick` and
  `maverick-proxy` binaries;
- `maverick-offline-kit-<v>.tar`: both image tarballs, the migrations, and their checksums,
  for a host with no registry access;
- CycloneDX SBOMs, the `cargo deny` report, and `SHA256SUMS`.

Every image and file is signed with cosign keyless signing by the release workflow. Verify
before you install. The `cosign verify` and `cosign verify-blob` commands are in
`docker/README.md` "Releases". Then check the files:

```sh
sha256sum -c SHA256SUMS
```

For a sealed host, verify on a connected machine, move the offline kit by your own transfer
process, then load the images:

```sh
tar -xf maverick-offline-kit-<v>.tar
cd maverick-offline-kit-<v>
sha256sum -c SHA256SUMS
docker load -i images/maverick-<v>.tar.gz
docker load -i images/maverick-proxy-<v>.tar.gz
```

The kit also holds the release's migrations in `migrations/`, for a change control review.
`maverick upgrade` applies them from the binary, so you never run them by hand.

Check what you installed:

```sh
maverick --version
```

```
maverick 0.1.0
release date: 2026-11-02
commit: 3f2a9c1
minimum schema: 12
bundle formats: mmbundle1
```

`minimum schema` is the newest migration the release ships, so the schema the database
needs before `serve` runs. `bundle formats` lists the bundle formats the release imports.

In the image, `maverick` is the entrypoint, so `docker run <image> --version` prints the
same text. Every `maverick <command>` below works as `docker run ... <image> <command>`.

## 3. Configure

Four settings are required. Everything else has a default.

| Setting | Value |
|---|---|
| `MAVERICK_ROLE` | `replica`. There is no default, and startup refuses an unset or unknown role. |
| `MAVERICK_DATABASE_URL` | `postgres://<user>:<password>@<host>:5432/<database>` |
| `MAVERICK_INTERNAL_TOKENS` | `label:secret` pairs, comma-separated. An operator sends one whole entry to the internal listener: `Authorization: Bearer <label>:<secret>`. The label is the actor in the audit log. |
| `MAVERICK_CAPTURE_MASTER_KEY` | 64 hex characters (32 bytes). It encrypts debug captures. |

Generate the secrets:

```sh
openssl rand -hex 32                 # MAVERICK_CAPTURE_MASTER_KEY
echo "ops:$(openssl rand -hex 32)"   # one MAVERICK_INTERNAL_TOKENS entry
```

**Store both in your password manager now.** They are not in the database, so no database
backup restores them. Without the capture master key, captured prompts cannot be read.

Settings you may want on a replica:

| Setting | Default | Use |
|---|---|---|
| `MAVERICK_TENANT_NAME` | `default` | Your tenant's name. Read only on the first start. |
| `MAVERICK_BOOTSTRAP_KEY_FILE` | unset | Where the first start writes the first API key (mode 0600). |
| `MAVERICK_ANTHROPIC_API_KEY`, `MAVERICK_OPENAI_API_KEY`, `MAVERICK_GOOGLE_API_KEY`, `MAVERICK_XAI_API_KEY` | unset | Your provider keys for the classifier and latency probes. A provider with no key gets no client and no call. |
| `MAVERICK_ANTHROPIC_BASE_URL`, `MAVERICK_OPENAI_BASE_URL`, `MAVERICK_GOOGLE_BASE_URL`, `MAVERICK_XAI_BASE_URL` | the provider's public API | Point a provider at your own gateway. |
| `MAVERICK_CLASSIFIER_COLUMN` | `claude-haiku-4-5/direct` | The column that classifies prompt-form recommends. |
| `MAVERICK_MATRIX_PULL_URL` | unset | Pull bundles hourly (section 6). |
| `MAVERICK_SMTP_URL`, `MAVERICK_ALERTS_WEBHOOK_URL` | unset | Alert delivery (`docs/operations.md` "Alerts"). |
| `MAVERICK_OTEL_EXPORTER_ENDPOINT` | unset | Traces and metrics over OTLP. |

The full list is spec §14.1. Settings a replica does not use are ignored: the judge pool,
the corpus spend caps, and the export settings.

A `MAVERICK_*` variable that no setting reads logs one warning, `unknown setting ignored`,
and the command still runs. A renamed setting keeps working under its old name for 12
months, with a warning that names the new one. So an older environment file keeps working
after an upgrade (`docs/operations.md` "Configuration compatibility").

## 4. Create the schema

Run `maverick upgrade` from the release against the empty database. It applies the engine
migrations and the job queue's schema.

```sh
maverick upgrade --check   # shows the plan and changes nothing
maverick upgrade --yes     # applies it
```

The report names each migration and its class, then `applied: ...`, `verify: ok`, and
`result: upgraded`. `serve` never migrates. It refuses to start while migrations are pending, and the message
says `run maverick upgrade`.

## 5. First start

Start the engine. One process can do everything, or you can split the API and the workers:

```sh
maverick serve                  # API and workers in one process
maverick serve --api-only       # only the listeners (run as many as you need)
maverick serve --worker-only    # only the jobs (run one)
```

On the first start with no tenant, the replica creates your tenant and its first API key,
named `bootstrap`. The raw key is printed **once**, as one plain line on stderr, and never
in a log line:

```
maverick: created tenant "default" (ten_...) and its first API key, shown only once: mk_live_...
```

When `MAVERICK_BOOTSTRAP_KEY_FILE` is set, the key is also written to that file with mode
0600. Later starts create nothing. If you lose the key, issue another one with
`maverick key create` (section 7).

Check the install:

```sh
curl -sS http://localhost:8080/up
curl -sS -H "Authorization: Bearer ops:<secret>" http://localhost:8081/health/detail
maverick matrix status
```

- `/up` answers `{"status":"ok"}`.
- `/health/detail` reports `"role": "replica"` and `"active_matrix_version": ""`. The empty
  version means no bundle is imported yet. With no provider key, its
  `provider_client_config` component says `none_configured`, which is expected.
- `maverick matrix status` prints `no matrix bundle is active`.

**Before the first bundle**, the catalog is empty, so there is no model to recommend: a
recommend answers `422 unknown_pool`. The first import (section 6) brings the catalog,
and it gives the tenant its default pool with every active column. It does that only
while the tenant has no pool, so a pool you wrote yourself is never changed. From then
on the install ranks from the bundle's measured results. A column with no measured data
yet ranks the cold start way, as on a fresh hosted install (G1), and says
`"measured": false`.

## 6. Import the matrix

The origin publishes a new bundle after each nightly rebuild. Each bundle is one immutable
file, `mmbundle1-<published_at>.tar`, at the public bundle location, next to an
`index.json` that lists them newest first with their SHA-256 hashes. The public location
is `https://bundles.example.com/` until the owner names it (bead
`model-maverick-xq7v.25`).

### File import (sealed and connected installs)

Download the newest bundle and its index on any connected machine, check the hash against
the index, and move the file to the install by your own process:

```sh
curl -fsSO https://bundles.example.com/index.json
curl -fsSO https://bundles.example.com/mmbundle1-<published_at>.tar
sha256sum mmbundle1-<published_at>.tar    # equals the entry's sha256 in index.json
```

Then import it:

```sh
maverick matrix import mmbundle1-<published_at>.tar
maverick matrix status
```

The import checks the file before it changes anything:

- at least one signature from a key compiled into your release verifies over the manifest
  (no setting adds a trusted key, so a changed environment file cannot make the install
  accept a forged bundle);
- every file's hash matches the manifest;
- the format is one `maverick --version` lists;
- the bundle supports your database's schema.

It then installs the catalog, the aggregates, and the calibration in one transaction, and
makes the bundle active. It prints `matrix bundle imported` and exits 0. Any failure
exits 1 and changes nothing. An origin refuses the command.

- Importing the active bundle again changes nothing: it prints
  `matrix bundle already active` and exits 0.
- A bundle older than the active one (an earlier `published_at`) is refused. To roll back
  on purpose, pass `--allow-older`.
- Your local overrides survive every import: local prices (section 7) and the latency your
  own probes measured.

### Connected installs: the hourly pull

Set `MAVERICK_MATRIX_PULL_URL` to the directory that holds `index.json`: the public
location, or your own mirror of it. HTTP and HTTPS both work. The URL takes no
credentials, query, or fragment. At minute zero of each UTC hour the `matrix_pull` job
reads `index.json`, picks the newest bundle in a format your release reads, downloads it,
checks its size and hash against the index, and imports it with the same checks as a file
import. When that bundle is already active, it downloads nothing. The pull never rolls
back: an edited or stale index cannot move your install to an older bundle. The setting
is off by default, and then the worker registers no pull at all.

A mirror may redirect at most three times, and only to HTTPS. Limits and timeouts are in
`docs/matrix-bundles.md` "Optional hourly pull"

Each run logs one line. Success is INFO `matrix pull complete`, with `outcome` set to
`imported` or `already_active`. A failure is WARN `matrix pull failed`, with a `kind`:
`http` when the location is unreachable or answers an error, `format` when it lists no
bundle your release reads, `bundle` when a file fails its checks, and `import` when the
import refuses it. The active matrix stays in service, and the job queue retries the run.
Log lines never hold the URL or a response body.

## 7. Keys and configuration

`maverick key` manages your tenant's API keys. No command takes a tenant id. The output is
JSON on stdout.

```sh
maverick key create --name billing-app   # {"id":"key_...","key":"mk_live_..."}, shown once
maverick key list                        # every key, never a raw key
maverick key revoke --id key_...         # revoking twice changes nothing
```

Give each application its own key, so you can revoke one without touching the others.
Rotate a key by creating the new one, moving the application to it, and revoking the old
one.

Configure the tenant through the public API with one of your keys, as a hosted tenant does
(`docs/api.md` "Tenant configuration"):

- `PUT /v1/config/pools`: the models each pool may use. Pools are hard boundaries (G2): a
  request can make a pool smaller, never larger. Leave out columns your network cannot
  reach.
- `PUT /v1/config/constraints`: standing limits on cost and latency, zero retention, and
  telemetry opt-out.
- `PUT /v1/config/weights`: per-capability weights for quality, cost, and latency, and the
  quality bar.

Column administration is on the internal listener:

- **Local prices.** `PUT /internal/v1/columns/{id}` with a `pricing` body stores your
  negotiated rates as a local snapshot, and imports never overwrite it
  (`docs/operations.md` "Pricing").
- **Endpoints.** The provider base URLs in section 3 point every call for a provider at
  your gateway.

A replica refuses tenant administration with `409 single_tenant_install`: creating a second
tenant, suspending or activating the tenant, and tenant limit overrides. To cut off one
application, revoke its key. To stop all traffic, stop `serve`.

## 8. The proxy (optional)

`maverick-proxy` gives your applications base-URL integration: they point their provider
SDK at the proxy, and the proxy asks your engine for a ranked list and calls the chosen
model. Run it in self-hosted mode, the default:

| Setting | Value |
|---|---|
| `MAVERICK_PROXY_MODE` | `self-hosted` (the default) |
| `MAVERICK_ENGINE_URL` | your engine's public listener, for example `http://maverick:8080` |
| `MAVERICK_API_KEY` | one of your keys, from `maverick key create --name proxy` |
| `MAVERICK_PROXY_ADDR` | the listen address, default `:8090` |
| `MAVERICK_UPSTREAM_ANTHROPIC`, `MAVERICK_UPSTREAM_OPENAI`, `MAVERICK_UPSTREAM_GOOGLE`, `MAVERICK_UPSTREAM_XAI` | the providers' public APIs by default; set them to your gateways |

The proxy executes inference with the caller's provider credentials. It translates no wire
format. During an engine outage it falls open to the last good list, then to the default
model you configure, then answers `503 engine_unavailable`. `docs/features/proxy.md` and
`docs/operations.md` "The proxy in production" have the details.

An engine and a proxy work together when their releases are at most 12 months apart. A
newer proxy sends an optional field only after the engine advertises it in
`X-Maverick-API-Features`, so you can upgrade either one first.

## 9. Sealed networks

A replica makes no outbound connection unless you configure one:

| Outbound call | Default on a replica | Turned on by |
|---|---|---|
| Model providers (classifier, latency probes) | none | a `MAVERICK_*_API_KEY`, sent to the matching `MAVERICK_*_BASE_URL` |
| models.dev prices (`price_sync`) | off | `MAVERICK_CRON_PRICE_SYNC` |
| Matrix pull | off | `MAVERICK_MATRIX_PULL_URL` |
| Alert webhook, SMTP | off | `MAVERICK_ALERTS_WEBHOOK_URL`, `MAVERICK_SMTP_URL` |
| OpenTelemetry collector | off | `MAVERICK_OTEL_EXPORTER_ENDPOINT` |
| Telemetry or phone-home to Maverick | none, ever | nothing turns it on |

`maverick upgrade` downloads nothing, so upgrades work the same on a sealed host. Bundles
arrive as files (section 6).

CI proves the defaults in this table on every run. A replica with every setting at its
default bootstraps, imports a bundle, serves a recommend, and runs each job it schedules,
inside a network namespace with no route out, and any outbound connection fails the run
(`docs/operations.md` "The sealed-environment test").

If you set `MAVERICK_CRON_PRICE_SYNC`, the job writes every column in
`MAVERICK_MODELS_DEV_PRICE_MAP`, a local price included. Leave columns with negotiated
rates out of the map.

## 10. Upgrades

Any release from the last 12 months upgrades directly to the current one. Run every upgrade
the same way:

1. **Read the release notes**, and check that the new release is signed (section 2).
2. **Back up the database**: `pg_dump` of the one database is a complete backup
   (`docs/operations.md` "Backups"). Rollback is a restore, so take one every time.
3. **Install the new release** by your normal process. Leave the old `serve` processes
   running for now.
4. **Check the plan** with the new release's binary:

   ```sh
   maverick upgrade --check
   ```

   It changes nothing. It prints the database's schema, the release that last upgraded it,
   the checks, and every migration it would apply, each marked `online-safe` or
   `needs-downtime`. `--json` prints the same report as one JSON object, for a change
   control record.
5. **Upgrade:**

   ```sh
   maverick upgrade           # asks before it applies
   maverick upgrade --yes     # for automation
   ```

6. **Restart or roll `serve`** onto the new release, then check `maverick --version`,
   `maverick matrix status`, and `/health/detail`.

**Online-safe and needs-downtime.** An `online-safe` migration leaves the previous release
working against the new schema, so you can upgrade while the old `serve` processes run and
then roll them one at a time, with no downtime. A `needs-downtime` migration can break the
previous release. `maverick upgrade` refuses it while any `serve` process is running (each
one writes a heartbeat every 30 s, and a heartbeat older than 2 minutes counts as stopped).
For such a release, stop every `serve`, upgrade, then start the new release. Nearly every
migration is online-safe, because removals and renames are spread across two releases.

**What `maverick upgrade` refuses:**

- **A database from a newer release.** It names both schema versions. Run a release that
  knows them, or restore a backup. The engine never downgrades a schema.
- **A database older than the 12-month window.** It names the release to run first.
  Upgrade to that release, then to the current one.
- **A second upgrade at the same time.** A Postgres advisory lock lets one run at a time.

It also warns when the active bundle's format is one the new release does not read. The
install keeps serving on its current data; import a bundle in a format the new release
reads (section 6).

Every upgrade writes a row to the `upgrades` table: the from and to release, the operator
(`cli:<os user>`), the migrations, and the start and finish times. Support asks for this
history (section 13). `docs/operations.md` "Schema changes and upgrades" has the details.

## 11. Rollback

Migrations only move forward, so rolling back the release means restoring the database:

1. Stop every `serve` process.
2. Restore the backup you took before the upgrade into the database.
3. Start the previous release.

To roll back only the matrix, import an older bundle file with
`maverick matrix import --allow-older <file>`. The pull never does this for you.

## 12. A stale matrix

The daily `catalog_sync` job (06:10 UTC by default, `MAVERICK_CRON_CATALOG_SYNC`) raises the
`matrix_stale` warning alert when no bundle is active, or when the active bundle was
published more than 7 days ago. `maverick matrix status` shows the same thing in its
`DAYS_BEHIND` column.

A stale matrix never stops recommends. The install keeps ranking on the last bundle it
imported, for as long as it runs. The rankings just stop following new model releases,
price changes, and quality changes. To clear the alert, import a newer bundle (section 6).
With the pull on, look for `matrix pull failed` in the worker's log: its `kind` says why
(section 6).

Alerts go to the channels you configure (section 3). With no channel, each alert is still
written to the database. `docs/operations.md` "Alerts" lists every kind.

## 13. What support needs

When you ask for help, send:

- `maverick --version`;
- `maverick matrix status`;
- the `/health/detail` response;
- the upgrade history: `select * from upgrades order by started_at;`
- the log lines around the problem.

Never send an API key, an internal token, the capture master key, or a database dump. Log
lines hold no prompt, completion, or credential, so they are safe to send.
