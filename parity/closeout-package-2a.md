# Package 2a parity closeout

- Bead: `model-maverick-rust-port-3hk.2.28`
- Date: 2026-09-29
- Basis: committed `main` plus the tag and test changes in this same commit.
- Reproduce: `cargo run -p parity-support --bin parity_coverage -- --list-missing`

## What this is

Package 2a is the pure domain crate `maverick-domain`. This report proves its parity bar
before later packages build on it. It uses the manifest coverage checker (`parity-coverage`,
bead 1.5) over `parity/manifest/*.json`. A manifest row is covered when a Rust test carries a
`// parity: <area>/<go_test>` tag for it, or a `parity/DIVERGENCES.md` row explains why not.

## Coverage summary

| area | total | tagged | divergence | missing |
|------|-------|--------|------------|---------|
| api | 256 | 96 | 0 | 160 |
| decision | 105 | 82 | 0 | 23 |
| measurement-core | 173 | 162 | 0 | 11 |
| measurement-io | 144 | 143 | 0 | 1 |
| proxy | 66 | 62 | 0 | 4 |
| store | 92 | 73 | 0 | 19 |
| TOTAL | 836 | 618 | 0 | 218 |

The checker reports zero unknown tags: no tag names a Go test outside the manifest. These counts
are a snapshot at this commit. The tagged count grows fast as later packages (2b, 2d, 2e) land
their own tests, so re-run the command for the current state. The per-area counts in the ledger
below are from when 2a closed and were larger; the ownership mapping is what stays true.

## The 2a bar is met

Package 2a owns the pure, fixture-backed rows of these areas: decision (policy, ceilings,
bands, checks, reasons, capture crypto, key format), measurement-io (graders and the tasks
loader), measurement-core (scoring, predictions, catalog fixtures), store (ids), proxy
(`unit.json` parse_alias and extract_last_user_text), and api (telemetry derivation). Every
one of those pure-fixture rows has a Rust test. The missing rows are all reference-doc,
database, HTTP, jobs, or live-provider rows that a later package owns. The ledger below names
the owner of each group.

## Fixes applied in this closeout

The checker found tests that exist but did not claim their manifest row, plus scoring property
tests that were never ported. This commit fixes them. No domain source logic changed; the
changes are test tags and new tests.

1. Bands (bead 2.18). `tests/ceilings.rs::banded_policy_table` runs every case in `bands.json`.
   It claimed one of the nine `recommend.TestResolveBands*` rows. Added the other eight tags.
2. IDs (bead 2.1). `src/ids.rs::new_format` and `new_unique_concurrent` cover `ids.TestNewFormat`
   and `ids.TestNewUnique`. Added the two tags.
3. Capture crypto (bead 2.23). `src/capture.rs::open_rejects_tampered_ciphertext` flips a
   ciphertext byte and asserts the open fails, and also rejects a wrong capture id. It claimed
   only the wrong-id row. Added the `tracing.TestOpenRejectsTamperedCiphertext` tag.
4. Spend tags (bead 2.14). `tests/spend.rs` tagged three `jobs.TestProjectSpendGolden/<subtest>`
   names that are in no manifest. The parent `jobs.TestProjectSpendGolden` row is covered by the
   same test. Removed the three stray tags.
5. Scoring property tests (beads 2.9, 2.10, 2.11). The reference (Part 3, "Property tests")
   says to port six Go property tests "as properties over any RNG". They were not ported. Added
   six proptests, tagged to their rows:
   - `scoring.TestEWMABoundsProperty` and `scoring.TestResumeMatchesDirectFoldProperty` in
     `tests/scoring_observe_ewma.rs`.
   - `scoring.TestVariancePenaltySigmaMatchesDirectFormula`,
     `scoring.TestSharedDimensionSpreadBoundsProperty`, and `scoring.TestPenaltyProperties` in
     `tests/scoring_penalties.rs`.
   - `scoring.TestRunQualityBoundsProperty` in `tests/scoring_run_quality.rs`.

Tooling: `parity-coverage` gained a `--list-missing` mode. It prints each uncovered row with its
manifest `where` pointer, so later package closeouts sort missing rows the same way.

## Missing-row ledger (owned by later packages)

Every missing row needs a database, HTTP, jobs, or the live provider. Each has an owner. Nothing
is orphaned.

### api (196): Package 2d-API (epic `3hk.5`)
- Endpoint rows (`reference/...`): auth requirement, capture read, recommend, policies, models,
  and telemetry endpoints. Owned by the 5.x endpoint beads.
- `middleware.*` (auth, request id, logger redaction, rate limit, body limit) and `apierrors.*`
  (error envelope, code constants): owned by the API middleware and error-envelope beads (5.x).
- `mcp.*`: the MCP server, bead 5.13.

### proxy (38): Package 2d-Proxy (epic `3hk.6`)
- `scenarios.json` rows (chat and messages surfaces, streaming, header stripping, BYOK, OTel)
  and `unit.json` `rewrite_model`: owned by the proxy replay harness (bead 6.11) and the chat and
  messages surface beads (6.7, 6.9). The `unit.json` parse_alias and extract_last_user_text rows
  are 2a and are covered.

### measurement-core (43 at this commit): Package 2e-Jobs (epic `3hk.7`)
- `jobs.*` (aggregate rebuild, finish batch, nightly smoke, weekly sweep, regression scan, spend
  guard, price sync, catalog sync, prediction lock, latency probes): owned by the 7.x job beads.
  The spend projection math (2.14), the catalog pure logic (2.20), and the regression math (2.12)
  are 2a and are covered; these rows are the job wiring around them.
- `scoring.TestSaveScoresRoundTrip`: needs the store; owned by the store or scoring job.
- `scoring.TestFoldObservationsMultipleDimensions` and `TestFoldObservationsThreeNights`: the
  rebuild-job fold; bead 7.5.
- `catalog.TestClientsFor` and `catalog.TestColumnLookupByIDAndLifecycleState`: need the store and
  provider clients; catalog store beads.

### decision (23)
- `classify.*` (15): the online classifier (retries, timeout, cache round-trip, store-backed
  invalidation). Needs an HTTP model call and the store. Owned by the classify runtime bead. The
  pure classifier logic (2.25) is 2a and is covered.
- `auth.TestVerifier*` (8): API key verification against the store. Owned by Package 2d-API auth
  (5.x). The pure key minting and hashing (2.24) is 2a and is covered.

### store (19): Package 2b (epic `3hk.3`)
- `main.TestRunCorpus*` and the `*-unreachable-db` CLI rows: the corpus and migrate CLI. Beads
  3.10 and 3.11.
- `calibrate.json` rows (`main.TestTierFindings*`, `TestCraftedGrid*`, `TestRunCorpusCalibrate*`):
  the calibrate report. Bead 3.10.

### measurement-io (1)
- `livesmoke.TestLiveSmoke`: the one opt-in live smoke test with a spend cap. Owner-gated, bead
  9.6.

## Go TestMain rows

Nine `*.TestMain` rows are Go test-harness entrypoints (`m.Run()` setup), not behavioral tests.
Rust has no equivalent; each Rust test does its own setup. They are not ported and need no
fixture. A `DIVERGENCES.md` row could record this once the owner approves it; this closeout does
not add a divergence without approval.

## cargo-mutants

This closeout changed no domain source logic, so the crate's mutant posture is unchanged from
when each 2a bead (2.1 to 2.27) sealed its module to zero missed under the swarm protocol. The
new scoring property tests only add coverage. As a confirmation, this closeout ran
`cargo mutants -p maverick-domain` over the three scoring source modules that the new property
tests back (`ewma.rs`, `penalties.rs`, `run_quality.rs`): 154 mutants, 153 caught, 1 unviable,
0 missed.
The workspace CI runs the full-crate `cargo-mutants` tier on every change (bead `3hk.1.2`).

## Gates

- `cargo fmt --check`: pass.
- `cargo clippy -- -D warnings`: pass on `maverick-domain` and `parity-support`.
- `cargo test -p maverick-domain`: pass.
- `cargo test -p parity-support`: pass (7 tests, including the tool's own coverage tests).
- `cargo deny check`: pass (advisories, bans, licenses, sources ok).

## Divergences

No new divergence is introduced. Every assertion that relies on a divergence keeps its row tag.
Examples: the D6 tags in `tests/scoring_run_quality.rs` (provider-error and timeout runs have no
`q_run`) and the D36 prefix asserted in `src/capture.rs` (the decrypt error prefix).
