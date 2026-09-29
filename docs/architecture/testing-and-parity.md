# Testing and parity

This is a port of a Go engine, so the testing strategy is built around parity: the Rust behavior must match the recorded Go behavior, and every difference must be approved and written down.

## The parity data

The `parity/` directory holds the recorded Go behavior:

- `parity/manifest/*.json` lists every Go test, grouped by area (api, decision, measurement-core, measurement-io, proxy, store).
- `parity/fixtures/` holds the recorded inputs and outputs a Rust test replays.
- `parity/DIVERGENCES.md` records every approved difference from the Go behavior, as numbered rows (D1 to D39 and on). A `QUIRK:` line in `reference/engine-reference.md` is Go behavior that the Rust code reproduces unless a divergence row says otherwise.

The rule: every Go test in the manifest has a Rust test that replaces it, or a `DIVERGENCES.md` row that explains why not. A work package is complete only when its Rust tests pass the parity data for its area; code that compiles is not enough. A new intended difference is never added without the owner's approval and a new divergence row.

## The coverage checker

`parity-support` holds a coverage checker that maps each manifest entry to its Rust replacement or its divergence row and reports what is missing. The final audit turns the checker to strict mode across all areas and proves that every manifest entry is accounted for.

## The test tiers

All tests run through the tiers in `scripts/ci.sh`:

| Tier | What it runs |
|---|---|
| `fast` | `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace`, `cargo deny`, and the sqlx offline check. |
| `db` | The store and job tests against a real Postgres (testcontainers). |
| `parity` | The proxy replay, the API replay, and the SDK conformance scenarios (proxy and core-direct), each checked against the reviewed `DIVERGENCES.md` allowlist. Logs land under `target/ci/parity/`. |
| `e2e` | The full-stack script: the real binaries on Postgres against the fake provider. |
| `mutants` / `mutants-diff` | `cargo-mutants` on the domain crates, a full shard run or only the changed lines. |

## Determinism and no real providers

CI never calls a real provider. Every test uses the deterministic fake provider (`maverick-providers`), and the harnesses (`maverick-acceptance`, `maverick-e2e`, `sdk-conformance`) drive recording fakes or mock servers. The one live smoke test is opt-in and has a spend cap. Store and job tests use one testcontainers Postgres per test binary (`maverick-testdb`), with an isolated database per test. CI compiles with `SQLX_OFFLINE=true` against the query cache that `scripts/sqlx-prepare.sh` writes, so it needs no database to build.

## Mutation testing

`cargo-mutants` runs on the domain crates. It mutates the code and checks that a test fails, which catches assertions that never really exercised the logic. The full run is large, so CI shards it on a schedule and checks each push with `mutants-diff` (only the mutants on changed lines). Do not run `cargo test` during a mutants run: it mutates in place. Write tests that pin exact values so a mutant is caught.

## Float parity

Some fixtures were recorded on hardware that fused a multiply-add. For exact-float parity in scoring, EWMA, prediction, and ranking, the Rust code uses `f64::mul_add` where the Go code fused `a*b+c`. Pure adds and comparisons do not need it.

## Conformance

The `sdk-conformance` crate holds the frozen cross-language scenarios (twelve groups) and test vectors, plus a mock engine and mock provider. Two runners replay them: one drives the real trial proxy, and one drives the `maverick-core` pieces directly the way an SDK core does. Together they cover all the scenarios and prove the proxy and the four SDKs will behave the same way. See [Request path](request-path.md).
