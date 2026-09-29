# Testing and parity

This is a port of a Go engine, so the testing strategy is built around parity: the Rust behavior must match the recorded Go behavior, and every difference must be approved and written down.

## The parity data

The `parity/` directory holds the recorded Go behavior:

- `parity/manifest/*.json` lists every Go test, grouped by area (api, decision, measurement-core, measurement-io, proxy, store).
- `parity/fixtures/` holds the recorded inputs and outputs a Rust test replays.
- `parity/DIVERGENCES.md` records every approved difference from the Go behavior, as numbered rows (D1 to D43). A `QUIRK:` line in `reference/engine-reference.md` is Go behavior that the Rust code reproduces unless a divergence row says otherwise.

The rule: every Go test in the manifest has a Rust test that replaces it, or a `DIVERGENCES.md` row that explains why not. A work package is complete only when its Rust tests pass the parity data for its area; code that compiles is not enough. A new intended difference is never added without the owner's approval and a new divergence row.

## The coverage checker

`parity-support` holds the coverage checker (`parity_coverage`). A Rust test claims a Go test with a `// parity: <go_test>` tag, and the checker maps each manifest row to the test that claims it or to its divergence row.

The final audit (bead 9.1, `parity/final-audit.md`) ran the checker in strict mode across all six areas. All 836 rows (675 Go tests plus the table subtests the manifests list one by one) have a tagged Rust test, and none is missing. Where Rust differs from Go, the test still claims the Go test, asserts the approved Rust value, and names the divergence row.

The strict check is a gate in the fast tier (`parity-coverage`). The build fails on a manifest row with no tag, a tag outside test code, a divergence row that no test names, or a citation of a row that does not exist.

The replays are complete too. The API replay has 617 exchanges: 507 are byte-exact, 110 are explained by divergence rows, and none is unexplained (`parity/closeout-package-2d-api.md`). The proxy replay passes all 54 scenarios in `parity/fixtures/proxy/scenarios.json`.

## The test tiers

All tests run through the tiers in `scripts/ci.sh`. CI (`.github/workflows/ci.yml`) runs `fast`, `db`, `parity`, and `e2e` on each push to `main` and on each pull request.

| Tier | What it runs |
|---|---|
| `fast` | `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace`, `cargo deny`, the sqlx offline check, the strict manifest audit (`parity-coverage`), and the instrumentation lint (`instrument`, D19). The store and job tests on a real Postgres (testcontainers) run here, inside `cargo test --workspace`. |
| `db` | A placeholder. It has no separate suite, because the store and job tests already run in the fast tier. |
| `parity` | The proxy replay, the API replay, and the SDK conformance scenarios (proxy and core-direct), each checked against the reviewed `DIVERGENCES.md` allowlist. Logs land under `target/ci/parity/`. |
| `e2e` | `scripts/e2e.sh` (bead 9.10): the real `maverick` and `maverick-proxy` binaries as separate processes on Postgres, against the fake provider over HTTP. It runs `maverick migrate`, `serve --worker-only`, `serve --api-only`, the corpus CLI, and the proxy in self-hosted and hosted modes. |
| `mutants` / `mutants-diff` | `cargo-mutants` on `maverick-domain`: a full run (or one shard), or only the mutants on changed lines. |

## Determinism and no real providers

CI never calls a real provider. Every test uses the deterministic fake provider (`maverick-providers`), and the harnesses (`maverick-acceptance`, `maverick-e2e`, `sdk-conformance`) drive recording fakes or mock servers. Store and job tests use one testcontainers Postgres per test binary (`maverick-testdb`), with an isolated database per test. CI compiles with `SQLX_OFFLINE=true` against the query cache that `scripts/sqlx-prepare.sh` writes, so it needs no database to build.

## The acceptance suites and harnesses

- **Privacy acceptance (`maverick-acceptance`, bead 9.2).** It runs the real engine and the real trial proxy (hosted mode) on real Postgres. Every provider is a recording fake, and a mock OTLP collector keeps every export. `tests/canary.rs` puts a canary in each prompt, in the other body fields the caller controls, and in each provider credential, across a prompt-form recommend, the proxy's chat and messages surfaces, and an MCP `recommend_model` call. It then checks that no canary reaches a log line, a span field, an exported span, an exported log record, or a metric label. `tests/privacy.rs` covers the other privacy tests of spec §15, including zero retention end to end and the telemetry opt-out. Both run in the fast tier.
- **Latency (`scripts/latency.sh`, bead 9.3).** It builds both binaries in release and runs `crates/e2e/tests/latency.rs` against a seeded catalog and the fake provider. It reports p50, p95, and p99 for the declared recommend path (target: p95 under 50 ms), the uncached prompt path (target: p95 under 1.8 s), and the proxy's added time to first byte. By default it only reports. `MAVERICK_LATENCY_ASSERT=1` also asserts the targets, for the M6 gate run on a quiet, production-like host. It is not a CI tier.
- **Container smoke (`scripts/docker-smoke.sh`, bead 9.4).** It builds the deploy images and checks the migration, the health endpoints, one hosted-proxy request against a fake upstream, and backup and restore.
- **Live smoke (`scripts/live-smoke.sh`).** It is the one test that calls real providers. It is opt-in, skipped by default, and capped at $1.00.

## Mutation testing

`cargo-mutants` runs on `maverick-domain`. It mutates the code and checks that a test fails, which catches assertions that never really exercised the logic. It copies the workspace to a scratch directory and mutates the copy, so it needs disk for a second build.

The full run is about 1,900 mutants, which is longer than one CI job allows. So each push and pull request runs `mutants-diff` (only the mutants on changed lines), and the full run is eight shards, nightly at 02:17 UTC and on demand. Write tests that pin exact values so a mutant is caught.

## Float parity

Some fixtures were recorded on hardware that fused a multiply-add. For exact-float parity in scoring, EWMA, prediction, and ranking, the Rust code uses `f64::mul_add` where the Go code fused `a*b+c`. Pure adds and comparisons do not need it.

## Conformance

The `sdk-conformance` crate holds the frozen cross-language scenarios (39 scenarios in twelve groups) and test vectors, plus a mock engine and mock provider. Two runners replay them. The proxy runner drives the real trial proxy through 21 scenarios. The core-direct runner drives the `maverick-core` pieces the way an SDK core does, through the other 18. So every scenario runs against the Rust code, and the proxy and the SDK core meet the same contract. See [Request path](request-path.md).
