# Self-hosted engine: deployment roles, public matrix bundles, and safe upgrades (proposed D49 and D50)

Status: proposed 2026-09-30, reworked 2026-10-24 after the owner's decisions (§11), adopted into the spec on 2026-10-24 (§14.4 and §14.5; bead xq7v.2). The owner asked for customers (financial and health-data companies) to run the engine inside their own networks, for the same build to run Maverick's hosted deployment, and for upgrades that do not break installs. On 2026-10-24 the owner decided that the engine is open source with no license key, so the offline license (D48) is dropped, and that the origin publishes signed matrix bundles publicly for free. The beads are under the epic model-maverick-xq7v.

## 1. Summary of the recommendation

1. **One build, two roles.** The same signed release runs everywhere. The hosted deployment is the **origin**: it holds the private corpus, runs the nightly benchmarks, and publishes the results. A customer install is a **replica**: it has no corpus and runs no corpus benchmarks. It imports signed **matrix bundles** from the origin.
2. **No license key.** The engine is open source (the repository's `LICENSE` file is the MIT license). Anyone may run any release. The engine has no license file, no entitlement check, and no call home. Revenue comes from the hosted service.
3. **Public, signed bundles.** After each rebuild the origin publishes a signed bundle to a public HTTPS location, free of charge. A replica verifies each bundle with public keys compiled into its release. A bundle holds global aggregates, never corpus content or tenant data.
4. **Upgrades follow written compatibility rules**: additive-only migrations, a supported upgrade window with a startup check, version skew rules between engine, proxy, and SDKs, and a CI test that upgrades a populated database from each supported older release.

The engine knows nothing about billing (owner rule, 2026-09-30). Nothing a replica does depends on payment or a contract.

## 2. Why the replica imports results instead of benchmarking

AGENTS.md: "Task prompts, golden answers, and rubrics must never go into this directory, into a public artifact, or into a benchmarked model's prompt." A customer install is outside Maverick's control. Any corpus shipped there, even encrypted, is readable, because the engine needs the plaintext to run it and the customer holds the machine and its keys.

The benchmark results are not the corpus. A cell aggregate (quality, pass rate, latency, cost, availability per column, capability, and tier) says how a model did, not what it was asked. Bundles are public, so everything in one is public: §5.1 limits a bundle to data that is safe to publish. Shipping aggregates keeps the corpus private and has three more benefits for these customers:

- No benchmark spend inside the customer's environment. G8 caps stay with the origin.
- No provider traffic for benchmarks leaves a sealed network.
- Every install ranks on the same matrix version, so support can reproduce a recommendation.

What stays local on a replica: classification (with the customer's own classifier column and key), policy, ranking, decision traces, debug capture, telemetry and outcomes, and hourly latency probes against the customer's own endpoints (thin pings with a fixed synthetic prompt, not corpus data). Quality transfers per model (spec §4.2); latency and price are properties of the path, so the replica measures latency itself and can override prices.

A replica that wants to benchmark its own tasks is "tenant-private corpora", already a v2 non-goal (spec §1.2). This plan does not change that.

## 3. Deployment roles (D49)

Config `MAVERICK_ROLE`: `origin` or `replica`. There is no default; an unset value is a startup error, so nobody runs the wrong role by accident.

| | `origin` (Maverick hosted) | `replica` (customer install) |
|---|---|---|
| Corpus load and activate | yes | refused: `corpus commands need MAVERICK_ROLE=origin` |
| `nightly_smoke`, `weekly_sweep`, `aggregate_rebuild`, `prediction_lock`, `calibration_score`, `regression_scan` | yes | not registered |
| `matrix_export` (new) | after each `aggregate_rebuild` | not registered |
| `maverick matrix import` (new) | refused | yes |
| `latency_probes` | yes | yes, for columns with a local endpoint |
| `price_sync` | yes (models.dev) | off unless `MAVERICK_CRON_PRICE_SYNC` is set; prices come from bundles, with local overrides |
| `model_discovery` | yes | not registered (the catalog comes from bundles) |
| `catalog_sync` | yes | yes, without the lifecycle transitions (bundles set column status); raises `matrix_stale` |
| `matrix_pull` (new) | not registered | hourly, only when `MAVERICK_MATRIX_PULL_URL` is set |
| `capture_expiry`, `history_prune` | yes | yes |
| Judge pool, corpus spend caps | required | not used |
| Classifier, capture, telemetry, internal API | yes | yes |

Neither role needs a license key. The hosted origin runs the same release a customer downloads. The `origin` role is not restricted: anyone may set it. An origin without the private corpus has nothing to benchmark, and a stock replica accepts only bundles signed with Maverick's keys (§5.3).

## 4. No license (D48 dropped)

The owner decided on 2026-10-24 that the engine is open source with no license key. The 2026-09-30 draft of this plan had an offline license file in this section. It is gone, and so is everything that depended on it:

- No `MAVERICK_LICENSE_FILE` or `MAVERICK_LICENSE` setting, no license file format, and no license signing tool.
- No license states, no `503 license_lapsed`, no `license_expiring` or `license_expired` alerts, no `license` component in health detail, and no `maverick license` command.
- Recommend never stops for a license or contract reason. A replica keeps ranking on its last imported matrix for as long as it runs. The `matrix_stale` alert (§5.4) tells the operator when it falls behind.
- Releases are not gated by date. Any release starts, subject only to the database rules in §7.3.

The number D48 is not reused, so older beads and mail that cite it stay readable.

## 5. Matrix bundles (D49)

### 5.1 Contents

A bundle is a signed archive published by the origin after each `aggregate_rebuild`:

```
mmbundle1/
  manifest.json      format version, matrix_version, published_at, origin release,
                     schema range it needs, content hashes of each file
  catalog.json       columns: provider, model_id, endpoint, settings, status,
                     pricing snapshot, retention fields, quality_source_column_id,
                     param_constraints, deprecation_date
  aggregates.jsonl   one row per cell per night in the window: the fields of the
                     aggregates table (quality, pass rate, ambiguity, latency,
                     cost, availability EWMAs and variances, measured)
  calibration.json   Brier and overconfidence per capability (the tenant-visible series)
  checks.json        check specs by capability (the references recommend returns)
  signatures/<kid>   Ed25519 over the manifest bytes, one file per signing key
```

- **No corpus content.** No task ids, prompts, golden answers, rubrics, grader specs, run outputs, or judge text.
- **No tenant data.** A bundle holds only global data. No tenant ids or names, API keys or key hashes, pools, constraints, traces, telemetry, outcomes, or tenant-local blends (G11) go into it.
- A CI test builds a bundle from the test corpus and a populated test scenario. It asserts that no task id, prompt, golden answer, rubric, or grader spec string from that corpus, and no tenant id or tenant name from that scenario, appears anywhere in the bundle. Bundles are public, so this test is the only barrier, and it runs in every CI run.
- The private signing key lives only in the origin's deploy secrets and is read at run time. It never enters this repository.
- Default window: the last 30 nights, enough for EWMAs, trailing availability (7 nights), and regression views.

### 5.2 Publishing

- `matrix_export` writes each bundle to `MAVERICK_MATRIX_EXPORT_DIR` on the origin as one archive file named by its format and `published_at`, and rewrites `index.json` in the same directory. One `matrix_version` spans many nights, so `published_at` (the completion time of the rebuild) identifies a bundle, and a retried export writes the same file.
- `index.json` lists the published bundles, newest first: `matrix_version`, `published_at`, format, file name, size, and SHA-256.
- The origin's deploy pipeline copies new files from the export directory to a public, static HTTPS location: an object-storage bucket or a static site, never the engine itself. There is no account, key, or charge. `docs/self-hosted.md` gives the URL.
- A published bundle file never changes. `index.json` is the only file that changes, and it is not signed: a replica trusts only the bundle signatures (§5.3), and the older-bundle rule (§5.4) means an edited or stale index cannot move a replica backward.
- The location keeps every bundle for at least 12 months (the skew window, §7.6), so a replica on any supported release finds a bundle in a format it reads. During a format change the origin publishes both formats.

### 5.3 Signing keys

- Each release compiles in the bundle public keys by `kid`: the current key, one standby key, and every older key that signed a bundle still inside the 12-month window. No setting adds or replaces a trusted key, so a changed environment file cannot make a replica accept a forged bundle.
- A replica accepts a bundle when at least one file in `signatures/` names a compiled-in `kid` and verifies over the exact manifest bytes. Then it checks every file's hash against the manifest. Signatures with an unknown `kid` are ignored, so a bundle signed by a newer key and an older key imports on releases that know either one.
- **Rotation.** A release adds the new public key. From then on the origin signs each bundle with both the old and the new key. When every release inside the 12-month window carries the new key, the origin stops signing with the old one, and a later release removes it.
- **Compromise.** The private half of the standby key is kept offline. If the current key leaks, the origin signs with the standby key at once, so replicas keep importing without an upgrade. The next release removes the leaked key and adds a new standby key. A replica that has not upgraded still trusts the leaked key; that is the cost of offline verification, and the security notice says so.
- A fork that runs its own origin builds its own release with its own public keys. The engine is open source, so this is a source change, not a setting.

### 5.4 Import

- `maverick matrix import <path>` verifies the signatures (§5.3), the manifest hashes, the format version, and the schema range. Then, in one transaction: upsert the catalog (columns keep their local latency data and local price overrides), append aggregates (append-only, as the spec requires, marked with the bundle's `matrix_version`), and switch the active `matrix_version`.
- Importing the same bundle twice is a no-op. A bundle whose `published_at` is earlier than the active bundle's is refused unless `--allow-older` is passed (for rollback).
- Connected installs can pull instead of copying files: `MAVERICK_MATRIX_PULL_URL` names the public location or a mirror. The replica reads `index.json` hourly, downloads the newest bundle in a format it reads, and imports it with the same checks. Pull never passes `--allow-older`. The setting is off by default.
- Sealed installs leave the pull URL unset. An operator downloads the bundle on a connected machine and moves the file through their own transfer process.
- A replica that has never imported a bundle serves cold-start recommendations (`measured: false`), exactly as a fresh origin does (G1).
- `catalog_sync` raises the `matrix_stale` warning alert when no bundle is active or the active bundle's `published_at` is more than 7 days old.

### 5.5 Local overrides on a replica

- Prices: `PUT /internal/v1/columns/{id}` can set a local pricing snapshot, for negotiated rates. Imports never overwrite it.
- Endpoints: provider base URLs are already configurable (`MAVERICK_ANTHROPIC_BASE_URL`, `MAVERICK_OPENAI_BASE_URL`, `MAVERICK_GOOGLE_BASE_URL`, `MAVERICK_XAI_BASE_URL`), so a replica can point at private gateways.
- Columns the customer cannot reach: they remove them from their pools. Pools are the hard boundary (G2).

## 6. Sealed-environment checklist

A replica makes no outbound connection unless configured:

| Outbound call | Replica default |
|---|---|
| Model providers (classifier, latency probes) | only the endpoints the customer configures |
| models.dev (`price_sync`) | off |
| Matrix pull from the public location | off (file import) |
| Alert webhook, SMTP | off unless configured |
| OTel collector | off unless configured |
| Maverick telemetry or phone-home | none, never |

A CI test starts a replica with every outbound setting at its default, inside a network namespace with no route out, runs a declared recommend, and asserts success and zero outbound connection attempts.

**A replica is single-tenant** (owner direction 2026-09-30). A self-hosted customer is one organization using the app. They download a release, install it on their infrastructure, and start it. There is no license key to buy or set. They never create or manage tenants. The engine keeps its multi-tenant data model, because the hosted origin needs it, but on a replica:

- **Bootstrap.** On first start with an empty database (after `maverick upgrade`), a replica creates exactly one tenant, with the default pool (every active column) and default constraints, and one API key. `MAVERICK_TENANT_NAME` names the tenant (default `default`); it is read only at bootstrap. The key is printed once to the process output and written to the file named by `MAVERICK_BOOTSTRAP_KEY_FILE` (mode 0600) when set. Nothing is created on later starts.
- **API keys.** `maverick key create | list | revoke` manage the tenant's keys (rotation, one key per internal app). These call the same code as the internal API. The tenant is implicit, so no command takes a tenant id.
- **Configuration.** The customer's admin configures pools, constraints, weights, and zero retention through the normal tenant API (`PUT /v1/config/...`) with one of their keys, exactly as a hosted tenant does.
- **Refused on a replica.** `POST /internal/v1/tenants` (a second tenant), tenant suspend and activate, and tenant limit overrides return `409 single_tenant_install`. The internal listener still serves health detail and column administration (local prices and endpoints).
- **No suspension.** Nothing replaces suspension on a replica. On the hosted origin, suspension serves the control plane (billing and abuse). On a replica the operator owns the whole install: to cut off one app they revoke its key with `maverick key revoke`, and to stop all traffic they stop `serve`. Suspending the only tenant would lock the customer out of their own install.
- **The proxy.** A customer who wants base-URL integration also runs `maverick-proxy` in its self-hosted mode, pointed at their engine, with one of their keys as `MAVERICK_API_KEY` (the existing self-hosted proxy form, D13).

Release artifacts for regulated customers: a container image and a static binary tarball per release, both signed (cosign for the image, a detached signature for the tarball), with an SBOM (CycloneDX) and the `cargo deny` report. An offline install kit bundles the image tarball, the migrations, and the checksums.

## 7. Upgrades without breaking installs (D50)

### 7.1 Versions

- Engine releases use SemVer. `maverick --version` prints the version, the release date, the git commit, the minimum schema it needs, and the bundle format versions it reads.
- The HTTP API is versioned by path (`/v1`). Within `/v1`, changes are additive only: new optional request fields, new response fields, new endpoints, new enum values only where the spec already says clients must tolerate unknown values. A breaking change is `/v2`, served alongside `/v1` for at least 12 months.
- Every response carries `X-Maverick-Engine-Version: <semver>` and `X-Maverick-API-Features: <comma list>` (for example `affinity`, `settings-header`).

### 7.2 The strict-decoding trap and its fix

The engine rejects unknown request fields with `422 invalid_request` (spec §9.2). Self-hosted engines will lag behind SDK and proxy releases. A new SDK that sends `affinity` to an older engine would break the customer's calls.

Rule: an SDK core or the proxy sends an optional request field only when the engine's last response listed the matching feature in `X-Maverick-API-Features`. The first request after startup sends none of them. This keeps strict decoding (Go parity, and it catches caller bugs) and makes new clients safe against old engines. New conformance vectors cover it.

### 7.3 Database

- Migrations stay up-only and additive (D37). A change that removes or renames uses expand and contract across two minor releases: release N adds the new shape and writes both; release N+1 stops reading the old shape; a later release drops it only after the support window (below) has passed N.
- **Support window.** Any release from the last 12 months upgrades directly to the current one. `maverick upgrade` (§7.4) checks the database's schema version first; older than the window, it stops and names the intermediate release to run first.
- **No silent downgrade.** A binary that finds migrations it does not know (the database is newer than the binary) refuses to start and names both versions. Rollback is restore from backup (D37).
- `serve` never migrates. It refuses to start while migrations are pending and says `run maverick upgrade`. Upgrades are an explicit step, which regulated change processes require.

### 7.4 The upgrade command

Owner decision 2026-09-30: upgrading is one command, `maverick upgrade`. The operator installs the new release (image or binary) by their normal process, then runs `maverick upgrade` from the new release against the database, then restarts `serve`. The command does not download anything, so it works the same in a sealed network.

`maverick upgrade` runs these steps in order and stops at the first failure, naming it:

1. **Preflight** (read only):
   - print the current state: the database's schema version, the release that last upgraded it, and this binary's `--version` output;
   - the database is not newer than this binary (no unknown migrations);
   - the database is inside the 12-month upgrade window; otherwise name the intermediate release to run first;
   - the active matrix bundle's format is one this release reads; otherwise say which bundle format to import after the upgrade;
   - no other `maverick upgrade` holds the upgrade lock (a Postgres advisory lock);
   - every running `serve` process is stopped, or the pending migrations are marked online-safe (below). Each `serve` process reports its release in a heartbeat row, so the command can see them.
2. **Plan**: list the migrations it will apply (engine and job queue), each marked `online-safe` or `needs-downtime`.
3. **Apply**: the engine migrations, then graphile_worker's, the same code `maverick migrate` runs, inside the upgrade lock.
4. **Record**: write an `upgrades` row (from release, to release, started, finished, operator, migrations applied). This is the upgrade history support asks for.
5. **Verify**: re-run the preflight checks and a read of each table the release touches.

Flags:

- `--check`: run step 1 and step 2 only, change nothing, exit 0 when an upgrade would succeed. Change-control boards ask for this output before approving.
- `--yes`: skip the confirmation prompt after the plan (for automation). Without it, the command asks before step 3 when it runs in a terminal, and refuses to apply without `--yes` when it does not.
- `--json`: machine-readable output of every step.

**Online-safe migrations.** A migration is online-safe when the previous release keeps working against the new schema (additive columns with defaults, new tables, new indexes built concurrently). Expand and contract (§7.3) keeps nearly every migration online-safe, so a multi-replica install can upgrade without downtime: run `maverick upgrade`, then roll the `serve` processes. A `needs-downtime` migration needs all `serve` processes stopped, and the preflight enforces that. Each migration file declares its class in a header comment, and a CI check refuses a migration without one.

`maverick migrate` stays as the low-level command (Go parity, development, CI). It shares the newer-database and window checks, so it can never do something unsafe that `upgrade` would refuse, but it skips the bundle, lock, record, and heartbeat checks. The docs point operators to `maverick upgrade` only.

### 7.5 Configuration

- Unknown `MAVERICK_*` variables are logged once at startup as a warning, never an error.
- A renamed variable keeps its old name working for 12 months, with a deprecation warning that names the new one.
- A new required setting needs a safe default or ships only in a major release.

### 7.6 Version skew between components

| Pair | Supported |
|---|---|
| Proxy and engine | any combination within 12 months of each other (the feature header covers the rest) |
| SDK and engine | the same |
| Bundle and replica | bundle format versions listed by `maverick --version`; the origin publishes the two most recent formats during a format change, and the public location keeps each bundle for 12 months (§5.2) |
| Bundle signing key and replica | a replica verifies any bundle signed by a key its release carries; the origin signs with the old and new keys for the whole 12-month window of a rotation (§5.3) |

### 7.7 Tests

- **Upgrade matrix.** CI keeps a database dump from each release in the support window, populated by a fixed scenario (for origin: several tenants; for replica: the single bootstrapped tenant; keys, pools, traces, aggregates). Each CI run upgrades every dump with `maverick upgrade --yes`, runs the contract tests against it, and checks that existing traces and history still read correctly.
- **Skew matrix.** The conformance harness runs the current SDK core and proxy against the previous engine release, and the previous proxy against the current engine.
- **Bundle compatibility.** Fixture bundles from each supported format version stay in the test data forever and must keep loading. A fixture signed only by the test standby key also loads.

## 8. Hosted deployment

The hosted deployment is an ordinary install with `MAVERICK_ROLE=origin`. It is the only place the corpus exists, the only place corpus benchmarks run, and the only publisher of Maverick-signed bundles. Its deploy pipeline holds the bundle signing key and copies each new bundle to the public location (§5.2). The control plane manages its tenants through the internal API, as today. Customer replicas never talk to the engine: they fetch bundles from the public location or receive them as files.

## 9. Guarantees

| Guarantee | Effect |
|---|---|
| G1 | A replica with no bundle serves cold start. Recommend never stops for a license or contract reason: a replica ranks on its last imported matrix. |
| G5 | Bundles hold no prompts and no tenant data. Zero-retention rules are unchanged on both roles. |
| G6 | Traces, history, and audit stay readable on both roles. No replica state locks them. |
| G8 | Corpus benchmark spend exists only on the origin. A replica's spend is its classifier (already capped) and latency probes. |
| Corpus is private | Enforced by the bundle content rule and its CI test (§5.1). |
| No billing in the engine | There is no license key, plan, price, or tier anywhere in the engine. |

## 10. Proposed `DIVERGENCES.md` rows

D48 (the offline license) was proposed in the first draft of this plan and dropped by the owner on 2026-10-24. The number is not reused.

| # | Area | Go behavior (oracle) | Rust expected | Plan / spec ref |
|---|---|---|---|---|
| D49 | Deployment roles and matrix bundles | One role: every install loads a corpus and benchmarks | `MAVERICK_ROLE=origin\|replica`; replicas run no corpus jobs and import signed `mmbundle1` bundles (no corpus or tenant content); the origin exports after each rebuild and publishes bundles at a public location with an `index.json`; replicas verify with compiled-in keys by `kid` (any one valid signature; a standby key for compromise); optional `MAVERICK_MATRIX_PULL_URL`, off by default; `matrix_stale` alert; local latency probes and price overrides on replicas; single-tenant replicas (bootstrap one tenant named by `MAVERICK_TENANT_NAME` and one key on first start; second tenant, suspend, activate, and limit overrides refused with `409 single_tenant_install`); `maverick key` and `maverick matrix` commands | plan §3, §5, §6; owner decision 2026-10-24 (bead model-maverick-xq7v.1) |
| D50 | Compatibility | `serve` behavior and no version surface per Go | `serve` refuses pending migrations and unknown (newer) migrations; `maverick upgrade` (preflight, plan, apply, record, verify; `--check`, `--yes`, `--json`) is the upgrade command and enforces the 12-month window, bundle format, lock, and online-safe rules; an `upgrades` history table; `maverick --version`; `X-Maverick-Engine-Version` and `X-Maverick-API-Features` on every response; clients send optional fields only when the feature is advertised | plan §7 |

## 11. Open questions for the owner

Decided 2026-10-24 by the owner (bead xq7v.1): the engine is open source and has **no license**. D48 is dropped, so questions 1 and 5 and the 30-day grace period no longer apply. The origin publishes signed matrix bundles **publicly and for free**; revenue comes from the hosted service. Accepted as recommended: the 12-month upgrade window, the 12-month skew window, the 30-night bundle window, question 3 (replicas never run corpus benchmarks), and question 4 (`MAVERICK_MATRIX_PULL_URL`, off by default). Bundle signing stays. Bead xq7v.15 removed the license from the rest of this plan on 2026-10-24.

The rework made these choices, which the owner can still change before xq7v.2 adopts them into the spec:

- The replica's tenant name comes from `MAVERICK_TENANT_NAME` (default `default`), in place of the license's `licensee` (§6).
- Nothing replaces suspension on a replica; suspend and activate stay refused with `409 single_tenant_install`, and the operator revokes keys or stops `serve` instead (§6).
- Bundles are published to a static HTTPS location with an unsigned `index.json`, and each bundle file is kept for 12 months (§5.2).
- Replicas trust only compiled-in keys. Rotation uses two signatures for a whole skew window, and a standby key covers a leak (§5.3).
- Bundles also exclude all tenant data, enforced by the same CI test as the corpus rule (§5.1).

The original questions, kept for the record:

1. **Lapsed behavior.** (Moot: no license.) Recommendation: `lapsed` returns `503 license_lapsed` on recommend only (clients fail open; audit stays readable). Alternative: never stop recommend; a lapsed install keeps serving on its last matrix, and only releases and bundles are gated. The alternative is gentler but gives no reason to renew beyond freshness.
2. **Grace and window lengths.** (Grace is moot; the windows are accepted.) Recommendation: 30 days grace, 12-month upgrade window, 12-month skew window, 30-night bundle window.
3. **Replica benchmarking.** (Accepted.) Recommendation: replicas never run corpus benchmarks (the corpus never leaves Maverick). Customer-run benchmarks on their own tasks stay a v2 item.
4. **Pull URL.** (Accepted.) Recommendation: offer `MAVERICK_MATRIX_PULL_URL` for connected installs, off by default.
5. **Who issues licenses.** (Moot: no license.) Recommendation: a small signing tool (`maverick-license`, separate binary in this workspace, private key from a file or KMS at run time, never in the repository) used by Maverick staff and later by the control plane. The engine only verifies.

## 12. Build order

1. Owner answers §11 (done 2026-10-24). Adopt D49 and D50 into the spec, `DIVERGENCES.md`, the master plan, and `docs/`.
2. Version surface first (`--version`, response headers, feature list), because every other piece reports through it.
3. In parallel: database upgrade rules (`serve` refuses pending or unknown migrations, the window check) and the `maverick upgrade` command; the feature-gated optional fields in `maverick-core`; the configuration rules (§7.5).
4. Deployment roles; bundle export and publishing on the origin; bundle import and pull on replicas; bundle key custody and rotation; single-tenant bootstrap and the key and matrix CLI commands.
5. Sealed-environment test, release artifacts (signatures, SBOM, offline kit), upgrade and skew matrices in CI.
6. Operations docs: a self-hosted install and upgrade guide, and the hosted runbook changes.
