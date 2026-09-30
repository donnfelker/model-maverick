# Self-hosted engine: licensing, sealed environments, and safe upgrades (proposed D48 to D50)

Status: proposed 2026-09-30. The owner asked for customers (financial and health-data companies) to run the engine inside their own networks, for the same build to run Maverick's hosted deployment, and for upgrades that do not break installs. The owner confirms §11 before the spec changes. The beads are under the epic "Self-hosted engine (D48 to D50)".

## 1. Summary of the recommendation

1. **One build, two roles.** The same signed release runs everywhere. The hosted deployment is the **origin**: it holds the private corpus, runs the nightly benchmarks, and publishes the results. A customer install is a **replica**: it has no corpus and runs no corpus benchmarks. It imports signed **matrix bundles** from the origin.
2. **An offline license file**, signed with Ed25519, checked by the engine with public keys compiled into the binary. No network call. The license says who may run the engine, until when, and which releases and bundles they may use. It says nothing about plans or prices.
3. **Expiry never breaks a customer's LLM calls.** A license gates freshness first (new releases and new bundles), then the recommend API, and the SDK and proxy fail-open ladders keep the customer's traffic flowing in every case.
4. **Upgrades follow written compatibility rules**: additive-only migrations, a supported upgrade window with a startup check, version skew rules between engine, proxy, and SDKs, and a CI test that upgrades a populated database from each supported older release.

The engine still knows nothing about billing (owner rule, 2026-09-30). A license is a signed statement about one deployment. How Maverick prices it is the control plane's business.

## 2. Why the replica imports results instead of benchmarking

AGENTS.md: "Task prompts, golden answers, and rubrics must never go into this directory, into a public artifact, or into a benchmarked model's prompt." A customer install is outside Maverick's control. Any corpus shipped there, even encrypted, is readable, because the engine needs the plaintext to run it and the customer holds the machine and its keys.

The benchmark results are not the corpus. A cell aggregate (quality, pass rate, latency, cost, availability per column, capability, and tier) says how a model did, not what it was asked. Shipping aggregates keeps the corpus private and has three more benefits for these customers:

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
| `price_sync` | yes (models.dev) | off by default; prices come from bundles, with local overrides |
| Judge pool, corpus spend caps | required | not used |
| Classifier, capture, telemetry, internal API | yes | yes |

Both roles need a license (§4). Maverick's hosted deployment runs under its own license, so the license code runs in production every day and is never a path only customers take.

## 4. The license (D48)

### 4.1 Format

A license is one text file:

```
mmlic1.<base64url(payload)>.<base64url(signature)>
```

- `mmlic1` is the format version. A different prefix is refused with the version named.
- The signature is Ed25519 over the exact bytes `mmlic1.` + the base64url payload text. There is no algorithm field, so there is no algorithm confusion.
- The payload is JSON:

```json
{
  "license_id": "lic_01J...",
  "kid": "mm-2026-1",
  "licensee": "Example Bank N.A.",
  "deployment": "exbank-prod",
  "roles": ["replica"],
  "issued_at": "2026-10-01T00:00:00Z",
  "not_before": "2026-10-01T00:00:00Z",
  "expires_at": "2027-10-01T00:00:00Z",
  "updates_until": "2027-10-01T00:00:00Z",
  "grace_days": 30
}
```

- `kid` picks the public key. The binary embeds every current and previous public key by `kid`. The private keys live only in Maverick's signing service, never in this repository.
- `roles` lists the roles this license may run. A customer license never lists `origin`.
- `deployment` is a label the customer chose. It appears in logs, health, and alerts so support can match a report to a license. It is not bound to hardware: sealed environments rebuild hosts, and hardware binding breaks disaster recovery.
- Unknown fields are ignored, so a later license with extra fields still loads on an older engine. A missing required field is refused.
- There are no tenant counts, plans, prices, or feature tiers. If one is ever needed, it is a new field added under the rule above, after an owner decision.

### 4.2 Loading

- `MAVERICK_LICENSE_FILE` (a path) or `MAVERICK_LICENSE` (the text). Exactly one is required.
- The engine re-reads the file every 10 minutes and on `SIGHUP`, so a renewal needs no restart. A new license that fails to verify is logged and ignored; the current one stays.
- Clock tolerance: `not_before` accepts up to 24 hours of skew, because sealed hosts often have drifting clocks.
- The raw license text is never logged. Logs show `license_id`, `licensee`, `deployment`, `kid`, and the dates.

### 4.3 States and what each one does

| State | When | Effect |
|---|---|---|
| `valid` | now is before `expires_at` | Normal. `license_expiring` warning alert at 60, 30, and 7 days before `expires_at`. |
| `grace` | from `expires_at` to `expires_at + grace_days` | Everything works. Daily `license_expired` critical alert. |
| `lapsed` | after grace | `POST /v1/recommend` and the MCP recommend tool return `503 license_lapsed`. Every other endpoint works: history, traces, config, health, the internal API. The SDK and proxy fail-open ladders keep the customer's LLM calls working from their last good lists (G14, D46). |
| refused at startup | no license, bad signature, unknown `kid`, wrong role, unknown format version, `not_before` in the future beyond tolerance | The process exits with a message that names the reason. Nothing else starts. |

Separately from those states, `updates_until` gates freshness:

- **Releases.** Each binary carries its release date. A binary whose release date is after `updates_until` refuses to start: `this release (2027-11-04) is newer than the license allows (updates until 2027-10-01); run an earlier release or renew`. The previous binary keeps running, so the customer is never stuck.
- **Bundles.** `maverick matrix import` refuses a bundle published after `updates_until`. The engine keeps ranking on the last imported matrix.

Why `lapsed` stops recommend and not everything: recommend is where Maverick's value is delivered, and the clients already survive its failure. History and config stay readable, because these customers have audit duties (G6) that do not end with a contract.

§11 question 1 offers the alternative of never stopping recommend.

### 4.4 Visibility

- `GET /health/detail` (internal listener) gains a `license` component: state, `license_id`, `licensee`, `deployment`, `expires_at`, `updates_until`, days left.
- `GET /up` does not depend on the license, so a lapsed install is not restarted in a loop by its orchestrator.
- Alerts use the existing `alerts` table, email, and webhook (spec §11, §12). No call to Maverick.
- `maverick license show` prints the same fields from the file, without the process running.

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
signature            Ed25519 over the manifest bytes, kid in the manifest
```

- **No corpus content.** No task ids, prompts, golden answers, rubrics, grader specs, run outputs, or judge text. A CI test builds a bundle from the test corpus and asserts that no task prompt, golden answer, or rubric string from that corpus appears anywhere in it.
- The bundle signing key is separate from the license signing key (different `kid` namespace), so a leaked export key cannot mint licenses.
- Default window: the last 30 nights, enough for EWMAs, trailing availability (7 nights), and regression views.

### 5.2 Import

- `maverick matrix import <path>` verifies the signature, the manifest hashes, the format version, the schema range, and `published_at <= updates_until`. Then, in one transaction: upsert the catalog (columns keep their local latency data and local price overrides), append aggregates (append-only, as the spec requires, marked with the bundle's `matrix_version`), and switch the active `matrix_version`.
- Importing the same bundle twice is a no-op. Importing an older bundle than the active one is refused unless `--allow-older` is passed (for rollback).
- Connected installs can pull instead of copying files: `MAVERICK_MATRIX_PULL_URL`, checked hourly, with the same verification. Sealed installs leave it unset and move files through their own transfer process.
- A replica that has never imported a bundle serves cold-start recommendations (`measured: false`), exactly as a fresh origin does (G1).

### 5.3 Local overrides on a replica

- Prices: `PUT /internal/v1/columns/{id}` can set a local pricing snapshot, for negotiated rates. Imports never overwrite it.
- Endpoints: provider base URLs are already configurable (`MAVERICK_ANTHROPIC_BASE_URL`, `MAVERICK_OPENAI_BASE_URL`, `MAVERICK_GOOGLE_BASE_URL`, `MAVERICK_XAI_BASE_URL`), so a replica can point at private gateways.
- Columns the customer cannot reach: they remove them from their pools. Pools are the hard boundary (G2).

## 6. Sealed-environment checklist

A replica makes no outbound connection unless configured:

| Outbound call | Replica default |
|---|---|
| Model providers (classifier, latency probes) | only the endpoints the customer configures |
| models.dev (`price_sync`) | off |
| Matrix pull | off (file import) |
| Alert webhook, SMTP | off unless configured |
| OTel collector | off unless configured |
| License check | none, offline by design |
| Maverick telemetry or phone-home | none, never |

A CI test starts a replica with every outbound setting at its default, inside a network namespace with no route out, runs a declared recommend, and asserts success and zero outbound connection attempts.

Tenant management without a control plane: `maverick tenant create | list | suspend | activate | key create | key revoke` call the same code as the internal API, so an operator in a sealed network needs no Rails shell. The control plane (hosted) keeps using the internal API.

Release artifacts for regulated customers: a container image and a static binary tarball per release, both signed (cosign for the image, a detached signature for the tarball), with an SBOM (CycloneDX) and the `cargo deny` report. An offline install kit bundles the image tarball, the migrations, and the checksums.

## 7. Upgrades without breaking installs (D50)

### 7.1 Versions

- Engine releases use SemVer. `maverick --version` prints the version, the release date, the git commit, the minimum schema it needs, and the bundle and license format versions it accepts.
- The HTTP API is versioned by path (`/v1`). Within `/v1`, changes are additive only: new optional request fields, new response fields, new endpoints, new enum values only where the spec already says clients must tolerate unknown values. A breaking change is `/v2`, served alongside `/v1` for at least 12 months.
- Every response carries `X-Maverick-Engine-Version: <semver>` and `X-Maverick-API-Features: <comma list>` (for example `affinity`, `settings-header`).

### 7.2 The strict-decoding trap and its fix

The engine rejects unknown request fields with `422 invalid_request` (spec §9.2). Self-hosted engines will lag behind SDK and proxy releases. A new SDK that sends `affinity` to an older engine would break the customer's calls.

Rule: an SDK core or the proxy sends an optional request field only when the engine's last response listed the matching feature in `X-Maverick-API-Features`. The first request after startup sends none of them. This keeps strict decoding (Go parity, and it catches caller bugs) and makes new clients safe against old engines. New conformance vectors cover it.

### 7.3 Database

- Migrations stay up-only and additive (D37). A change that removes or renames uses expand and contract across two minor releases: release N adds the new shape and writes both; release N+1 stops reading the old shape; a later release drops it only after the support window (below) has passed N.
- **Support window.** Any release from the last 12 months upgrades directly to the current one. `maverick migrate` checks the database's schema version first; older than the window, it stops and names the intermediate release to run first.
- **No silent downgrade.** A binary that finds migrations it does not know (the database is newer than the binary) refuses to start and names both versions. Rollback is restore from backup (D37).
- `serve` never migrates. It refuses to start while migrations are pending and names the command to run. Upgrades are an explicit step, which regulated change processes require.

### 7.4 Configuration

- Unknown `MAVERICK_*` variables are logged once at startup as a warning, never an error.
- A renamed variable keeps its old name working for 12 months, with a deprecation warning that names the new one.
- A new required setting needs a safe default or ships only in a major release.

### 7.5 Version skew between components

| Pair | Supported |
|---|---|
| Proxy and engine | any combination within 12 months of each other (the feature header covers the rest) |
| SDK and engine | the same |
| Bundle and replica | bundle format versions listed by `maverick --version`; the origin publishes the two most recent formats during a format change |
| License and engine | license format versions listed by `maverick --version`; the signing service can issue the older format on request |

### 7.6 Tests

- **Upgrade matrix.** CI keeps a database dump from each release in the support window, populated by a fixed scenario (tenants, keys, pools, traces, aggregates). Each CI run upgrades every dump with the new binary, runs the contract tests against it, and checks that existing traces and history still read correctly.
- **Skew matrix.** The conformance harness runs the current SDK core and proxy against the previous engine release, and the previous proxy against the current engine.
- **Bundle and license compatibility.** Fixture bundles and licenses from each supported format version stay in the test data forever and must keep loading.

## 8. Hosted deployment

The hosted deployment is an ordinary install with `MAVERICK_ROLE=origin` and a Maverick-issued license listing `origin`. Its license is renewed by the deploy pipeline. It is the only place the corpus exists, the only place corpus benchmarks run, and the only publisher of bundles. The control plane manages its tenants through the internal API, as today. Customer replicas never talk to it; bundles travel as files or through the pull URL (a static bucket, not the engine).

## 9. Guarantees

| Guarantee | Effect |
|---|---|
| G1 | A replica with no bundle serves cold start. `lapsed` returns 503 on recommend; the SDK and proxy fail-open ladders keep calls working (G14). §11 question 1 is the alternative. |
| G5 | The license and bundles hold no prompts. Zero-retention rules are unchanged on both roles. |
| G6 | Traces, history, and audit stay readable in every license state. |
| G8 | Corpus benchmark spend exists only on the origin. A replica's spend is its classifier (already capped) and latency probes. |
| Corpus is private | Enforced by the bundle content rule and its CI test. |
| No billing in the engine | The license carries no plan, price, or tier. |

## 10. Proposed `DIVERGENCES.md` rows

| # | Area | Go behavior (oracle) | Rust expected | Plan / spec ref |
|---|---|---|---|---|
| D48 | License | None | `MAVERICK_LICENSE_FILE` or `MAVERICK_LICENSE` required; Ed25519 `mmlic1` format verified offline with embedded keys; states `valid`, `grace`, `lapsed`; `503 license_lapsed` on recommend when lapsed; `updates_until` gates release start and bundle import; `license` health component and alerts | `plans/self-hosted-engine-plan.md` §4; owner decision 2026-09-30 |
| D49 | Deployment roles and matrix bundles | One role: every install loads a corpus and benchmarks | `MAVERICK_ROLE=origin\|replica`; replicas run no corpus jobs and import signed `mmbundle1` bundles (no corpus content); origin exports after each rebuild; local latency probes and price overrides on replicas; `maverick tenant` and `maverick matrix` commands | plan §3, §5, §6 |
| D50 | Compatibility | `serve` behavior and no version surface per Go | `serve` refuses pending migrations and unknown (newer) migrations; `maverick migrate` enforces a 12-month upgrade window; `maverick --version`; `X-Maverick-Engine-Version` and `X-Maverick-API-Features` on every response; clients send optional fields only when the feature is advertised | plan §7 |

## 11. Open questions for the owner

1. **Lapsed behavior.** Recommendation: `lapsed` returns `503 license_lapsed` on recommend only (clients fail open; audit stays readable). Alternative: never stop recommend; a lapsed install keeps serving on its last matrix, and only releases and bundles are gated. The alternative is gentler but gives no reason to renew beyond freshness.
2. **Grace and window lengths.** Recommendation: 30 days grace, 12-month upgrade window, 12-month skew window, 30-night bundle window.
3. **Replica benchmarking.** Recommendation: replicas never run corpus benchmarks (the corpus never leaves Maverick). Customer-run benchmarks on their own tasks stay a v2 item.
4. **Pull URL.** Recommendation: offer `MAVERICK_MATRIX_PULL_URL` for connected installs, off by default.
5. **Who issues licenses.** Recommendation: a small signing tool (`maverick-license`, separate binary in this workspace, private key from a file or KMS at run time, never in the repository) used by Maverick staff and later by the control plane. The engine only verifies.

## 12. Build order

1. Owner answers §11. Adopt D48 to D50 into the spec, `DIVERGENCES.md`, the master plan, and `docs/`.
2. Version surface first (`--version`, response headers, feature list), because every other piece reports through it.
3. In parallel: license verification and states; the signing tool; database upgrade rules (`serve` refuses pending or unknown migrations, the window check); the feature-gated optional fields in `maverick-core`.
4. Deployment roles; bundle export on the origin; bundle import on replicas; tenant and matrix CLI commands.
5. Sealed-environment test, release artifacts (signatures, SBOM, offline kit), upgrade and skew matrices in CI.
6. Operations docs: a self-hosted install and upgrade guide, and the hosted runbook changes.
