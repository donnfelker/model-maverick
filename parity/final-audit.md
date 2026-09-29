# Final manifest audit

- Bead: `model-maverick-rust-port-3hk.9.1`
- Date: 2026-09-29
- Proves: HANDOFF §5 item 1. Every Go test in `parity/manifest/` has a passing Rust test or an
  approved `parity/DIVERGENCES.md` row.
- Reproduce:
  - `cargo run -p parity-support --bin parity_coverage -- --strict --list-rows`
  - `scripts/ci.sh fast` runs the same strict check as a gate.

## The manifest

The six manifests list 675 top-level Go tests (the sum of their `go_test_count` fields). With
table subtests that the manifests list one by one, they have 836 rows. The checker counts rows.

| area | Go tests | rows | tagged | divergence | missing |
|------|----------|------|--------|------------|---------|
| api | 209 | 256 | 256 | 0 | 0 |
| decision | 105 | 105 | 105 | 0 | 0 |
| measurement-core | 136 | 173 | 173 | 0 | 0 |
| measurement-io | 144 | 144 | 144 | 0 | 0 |
| proxy | 34 | 66 | 66 | 0 | 0 |
| store | 47 | 92 | 92 | 0 | 0 |
| TOTAL | 675 | 836 | 836 | 0 | 0 |

A row is tagged when a Rust test carries `// parity: <go_test>`. No row relies on a divergence
row alone. Where Rust differs from Go, the Rust test still claims the Go test and asserts the
approved Rust value, and it names the row (next section).

## What the strict checker enforces

`parity_coverage --strict` fails when any of these holds:

1. A manifest row has no tag and no DIVERGENCES row.
2. A tag names a Go test that is in no manifest (a typo).
3. A tag sits outside test code, so it claims nothing. Test code is a `tests/` directory, or
   the part of a source file after its first `#[cfg(test)]`.
4. A test or the replay allowlist cites a DIVERGENCES row that does not exist. A test cites a
   row with a `parity_support::divergence!("D<n>")` marker or a `D<n>` in a test comment.
   `parity/blackbox/replay-allowlist.json` cites rows in its rules and entries.
5. A DIVERGENCES row is named by no test. Every approved difference is asserted somewhere.

Checks 3 to 5 are new in this bead. The strict run is a step of the CI fast tier.

## Divergence rows

Every row in `DIVERGENCES.md` is named by at least one test: 42 rows at this audit (D1 to
D41, and D43). A row added later (D42 is in flight) is held to the same check by the CI step. `--list-rows` prints each
row's citation count and its first citation. D20, the latency target, is named by the release-mode latency
harness `crates/e2e/tests/latency.rs` (bead 9.3). That harness runs through
`scripts/latency.sh`, not in `cargo test`, because debug-build latencies do not measure the
target.

The `parity_support::divergence!` macro checks its id against `DIVERGENCES.md` when the test
runs, so a marker cannot name a row that does not exist.

## The other checklist items

- **Property tests.** The six scoring properties exist as `proptest!` properties over random
  inputs, and each carries its tag:
  - `TestEWMABoundsProperty` and `TestResumeMatchesDirectFoldProperty`
    (`scoring_observe_ewma.rs`);
  - `TestVariancePenaltySigmaMatchesDirectFormula`, `TestSharedDimensionSpreadBoundsProperty`,
    and `TestPenaltyProperties` (`scoring_penalties.rs`);
  - `TestRunQualityBoundsProperty` (`scoring_run_quality.rs`);
  - all six again in `scoring_properties.rs`.

  The two decision properties are also properties: `TestEvaluateSurvivorsNeverEmptyProperty`
  (`policy_evaluate.rs`) and `TestRankPermutationProperty` (`ranking.rs`).
- **Store DB tests.** All 92 store rows are tagged, including the hand-written DB tests
  (`parity/closeout-package-2b.md`, bead 3.14). They run on real Postgres through
  testcontainers.
- **Corpus tests.** The owner supplied the private corpus (decision 10.1). It is checked out
  outside this repository.
  - `crates/maverick-jobs/tests/corpus_gates.rs` (bead 7.13) holds the two corpus rows:
    `TestRealCorpusSimulatedNight` and `TestFullMatrixWeeklySweep`.
  - Both pass with `MAVERICK_CORPUS_DIR` set: this audit ran them on 2026-09-29 against the
    owner's checkout (340 s). They skip without it, which is how CI runs them: the corpus
    never enters CI or this repository.
- **MCP stdio.** The transport is prose-only in the parity data. `crates/maverick-api/tests/mcp.rs`
  `the_stdio_transport_binds_the_run_identity` drives `mcp::serve_lines` in process: it
  checks the identity bound at start, the newline framing, and a parse error. The CLI row
  `mcp-stdio-unreachable-db` runs in `crates/maverick/tests/cli.rs`.

## Package closeout records

- Package 2a (domain): `parity/closeout-package-2a.md`
- Package 2b (store and CLI): `parity/closeout-package-2b.md`
- Package 2d-API: `parity/closeout-package-2d-api.md`
- Package 2e (jobs): `parity/closeout-package-2e.md`
- Package 2c and 2d-Proxy closed with their beads (4.9 and 6.13).
