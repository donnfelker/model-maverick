# Handoff: build the Rust engine

This document is your start point. It tells you what exists, what to read, and what to build first. Read `AGENTS.md` before this document.

## 1. The task

Build the Model Maverick engine in Rust. The engine includes the trial proxy and the SDK conformance harness.

An earlier Go engine implemented most of v1. Its exact behavior and all its 675 tests are in this directory, as documents and as test data. Your Rust engine must give the same results as that Go engine, except for the approved differences in `parity/DIVERGENCES.md`.

The Go source is not in this directory, and you do not need it. Do not try to find it, run it, or rebuild it.

## 2. What is complete

| Item | State |
|---|---|
| Product and architecture plan | Complete: `plans/master-plan.md` |
| Specifications | Complete. The Rust changes were adopted on 2026-09-28. |
| Exact behavior of the Go engine | Complete: `reference/engine-reference.md` |
| Test data for all 675 Go tests | Complete: `parity/`. `parity/manifest/` maps each test. |
| Approved differences from Go | Complete: `parity/DIVERGENCES.md` (D1 to D31; D26 to D31 added 2026-09-28 by owner approval) |
| Rust source code | Not started |

## 3. Read these documents, in this order

1. `AGENTS.md`: rules, stack, and order of authority.
2. `plans/master-plan.md`: read all of it. §7.0 is the port program. §8 is the build order.
3. `parity/DIVERGENCES.md`: the 31 places where Rust must not do what Go did.
4. `parity/README.md`: how to use the test data.
5. `specs/model-maverick-v1-spec.md`: the engine contract. Read the parts for each work package when you start that package.
6. `reference/engine-reference.md`: the exact behavior. Read the part for each work package when you start that package. It is long (about 4,300 lines). Search it, and do not read it from start to end.

Read these when you need them:
- `specs/capability-matrix-design.md`: the measurement method.
- `specs/model-maverick-openrouter-execution-spec.md`: the OpenRouter execution backend.
- `specs/model-maverick-v1.1-spec.md`: v1.1 (the Rails shell and the engine upgrades). This is not part of the first work.
- `plans/model-maverick-sdk-v0-plan.md` and `plans/model-maverick-integration-surfaces-plan.md`: the SDK core, the conformance harness, and the proxy.
- `docs/features/`, `docs/api.md`, `docs/operations.md`: feature and operation notes from the Go engine.
- `docs/market-brief.md`: market data (2026-09) that the plan refers to.
- `docs/agency-product-framework.md`: the owner's framework for the Rails shell.

## 4. Work packages, in order

These packages come from `plans/master-plan.md` §7.0 step 2. Each package is complete only when its parity gate passes.

| # | Package | Content | Parity gate | Reference |
|---|---|---|---|---|
| 1 | Workspace | Cargo workspace, CI gates from `AGENTS.md`, `crates/` layout | CI is green on an empty build | Plan §6 |
| 2a | Domain crates | IDs, task and grader types, graders, scoring, EWMA and Welford, ranking, policy, ceilings, alias grammar, capture crypto | `parity/fixtures/` for these areas | Parts 2, 3, 4 |
| 2b | Schema and store | One sqlx baseline migration, the new `batches` table, all queries, config loading, CLI | `parity/schema/schema.sql` comparison; store tests on real Postgres; `parity/fixtures/store/` | Parts 1, 7 |
| 2c | Providers | Anthropic, OpenAI, Google, and xAI clients; the fake provider; the runner | `parity/fixtures/measurement-io/providers_wire.json`, `fake_provider.json`, `runner.json` | Part 2 |
| 2d | API, MCP, seam, proxy | Public and internal listeners, all endpoints, MCP, the trial proxy | Replay of `parity/blackbox/`; replay of `parity/fixtures/proxy/scenarios.json` | Parts 5, 6 |
| 2e | Jobs | `graphile_worker` jobs, the `batches` lease, the spend guard, alerts, catalog jobs | `parity/fixtures/measurement-core/`; the job tests in plan §3.1 | Part 3 |

Packages 2a, 2b, and 2c can run at the same time. Package 2d needs 2a and 2b. Package 2e needs 2a, 2b, and 2c.

For each package:
1. Read the plan section, the spec sections, the reference part, and the `DIVERGENCES.md` rows for that area.
2. Write the Rust tests from the parity data first.
3. Write the code until the tests pass.
4. Make sure that each Go test in the manifest for that area has a Rust test that replaces it.
5. Commit.

## 5. Definition of done for the port

- Each entry in `parity/manifest/` has a passing Rust test, or an approved row in `DIVERGENCES.md`.
- The API replay and the proxy replay have zero unexplained diffs.
- The CI gates pass: fmt, clippy with `-D warnings`, tests, `cargo deny`, and `cargo-mutants` on the domain crates.
- The first live nightly on Rust completes below the spend cap. This step needs the owner (section 6).

## 6. Items that the owner must supply

You do not need these items to start. Stop and ask the owner when a task needs one of them.

| Item | Needed for |
|---|---|
| The private corpus repository (`modelmaverick-corpus`, about 220 tasks) | Corpus loads, two jobs tests, and real benchmarks. Never copy it into this directory. |
| Provider API keys (Anthropic, OpenAI, Google, xAI) | The live smoke test and real benchmarks. Not for CI. |
| The deploy host (Hetzner) and Kamal access | Deployment |
| The decision to keep or replace the existing Go-era database | The go-live step (plan §7.0 step 4) |

## 7. Open decisions

These decisions belong to the owner. Do not decide them yourself. If one blocks your work, stop and ask.

- The number of paid pilots and the price for the M6b commercial gate.
- The trial proxy execution default. The plan recommends direct execution, with OpenRouter as an opt-in.
- The default values for the classifier quota and the classifier spend cap. The spec says "set from M6 cost measurement".
- The name of the OpenRouter zero-data-retention request parameter. The spec uses `provider.zdr`, but nobody has confirmed that name against the OpenRouter API.
- The other open rows in `plans/master-plan.md` §10.

## 8. Known gaps in the parity data

See `parity/README.md`, section "Known gaps". In summary:
- 13 store database tests were written from the test code and a manual run, not from their original harness.
- 2 jobs tests need the private corpus.
- 6 property tests are written as properties.
- The MCP stdio transport is described in prose only.

## 9. Directory map

| Path | Content |
|---|---|
| `AGENTS.md`, `CLAUDE.md` | Agent rules. `CLAUDE.md` imports `AGENTS.md`. |
| `HANDOFF.md` | This document |
| `plans/` | The master plan and the drill-down plans |
| `specs/` | The normative specifications |
| `reference/engine-reference.md` | The exact behavior of the Go engine |
| `parity/` | Test data, the test manifest, and `DIVERGENCES.md` |
| `docs/` | Feature docs, API and operations docs, the market brief, and the owner's framework |
| `evals/` | The classifier accuracy evaluation set and plan |
| `scripts/`, `examples/` | The Claude Code launcher script, demo seed data, and a TypeScript agent example |
| `config/deploy-go-reference.yml` | The Kamal config of the Go engine, for reference only |
| `crates/` | The Rust source code (you create this) |
