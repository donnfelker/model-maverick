# Parity data

This directory has the test data for the Rust port. The data comes from the earlier Go engine and its 675 tests (extracted 2026-09-28). The exact behavior that goes with this data is in `reference/engine-reference.md`. The Go source is not available, and you do not need it.

Nothing here is Go code. Data and contracts (SQL, fixture files, prompts, schemas, error strings, headers, wire bytes) are copied byte for byte.

## Contents

| Path | Content |
|---|---|
| `DIVERGENCES.md` | The intended differences between Go and Rust (D1 to D43 on 2026-09-29). If a parity diff matches a row, the diff is correct. All other diffs are bugs. |
| `fixtures/<area>/` | Test cases with inputs and full-precision expected outputs, recorded wire exchanges, and all original `testdata/` files. |
| `manifest/<area>.json` | Each Go test, with the fixture case or reference section that replaces it. |
| `schema/` | The migrations, the final `schema.sql` (from `pg_dump` after all migrations), the old River queue schema (for a kept database only), the 98 sqlc queries, and `queries.md`. |
| `blackbox/` | 619 recorded HTTP exchanges from the API tests (`api-recorded.jsonl`), the 1,498 SQL statements that those tests used to seed and check state (`api-seed-sql.jsonl`), and the replay and masking rules (`README.md`). |

## Areas

| Area | Reference part | Go tests |
|---|---|---|
| `store` | Part 1: store, schema, config, CLI | 47 |
| `measurement-io` | Part 2: corpus loader, graders, runner, providers | 144 |
| `measurement-core` | Part 3: scoring, jobs, predictions, alerts, catalog | 136 |
| `decision` | Part 4: policy, recommend, classify, auth, tracing, history | 105 |
| `api` | Part 5: HTTP API, MCP | 209 |
| `proxy` | Part 6: trial proxy | 34 |
| **Total** | | **675** |

The counts include the `TestMain` harnesses. 35 test names occur in more than one package (640 unique names).

## How to use this data

1. Port the pure domain crates first. Run them against `fixtures/`. The numeric expected values have full precision. The reference gives the tolerance where Go results can change between runs (for example, map-order summation: compare at 1e-12).
2. Postgres stores `real` columns as float32. Many expected values (quality bars, pass rates, EWMAs) are float32 values that were widened to float64. The reference identifies each one.
3. For the API, apply `blackbox/api-seed-sql.jsonl` and send `blackbox/api-recorded.jsonl` in order, on one database. Mask the fields that `blackbox/README.md` lists.
4. For the proxy, replay `fixtures/proxy/scenarios.json` against a mock engine and mock providers.
5. Before you make an assertion, read `DIVERGENCES.md`. If a row applies, assert the Rust expected behavior, not the recorded Go value.

## Known gaps

- The 13 database tests in the `store` area were written from their test code and a manual run against local Postgres 16. They did not run in their original harness.
- Two jobs tests need the private corpus (`modelmaverick-corpus`). They are given/when/then cases only.
- Six property tests used Go's random generator. They are written as properties, because Rust cannot reproduce Go's random stream.
- The MCP stdio transport is described in prose only. The MCP HTTP transport is recorded.

These gaps are in the recorded data. `final-audit.md` records how the Rust tests cover each one.
