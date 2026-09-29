# Package 2b parity closeout

- Bead: `model-maverick-rust-port-3hk.3.14`
- Date: 2026-09-29
- Basis: committed `main` plus the tag and test changes in this same commit.
- Reproduce: `cargo run -p parity-support --bin parity_coverage -- --list-missing`

## What this is

Package 2b is the schema, the store layer, config loading, and the CLI: the crates
`maverick-store`, `maverick-config`, `maverick-testdb`, and `maverick` (the `maverick`
binary's commands). Its parity area is `store` (`parity/manifest/store.json`). The
`internal/tasks/store_test.go` rows of `measurement-io` belong to it too. This report checks
each item of the 3.14 checklist.

## Coverage summary

| area | total | tagged | divergence | missing |
|------|-------|--------|------------|---------|
| api | 256 | 220 | 0 | 36 |
| decision | 105 | 97 | 0 | 8 |
| measurement-core | 173 | 173 | 0 | 0 |
| measurement-io | 144 | 144 | 0 | 0 |
| proxy | 66 | 66 | 0 | 0 |
| store | 92 | 92 | 0 | 0 |
| TOTAL | 836 | 792 | 0 | 44 |

Every `store` row has a Rust test. The other areas belong to other packages; the counts are a
snapshot at this commit. The 44 open rows are API and auth rows, which the API replay (5.15)
and the 2d-API closeout (5.18) own.

## Fixes applied in this closeout

- **Five CLI rows were tested but not tagged.** `tests/cli.rs::cli_rows_match_go` already
  replays every row of `parity/fixtures/store/cli.json`. It now carries the tags of the five
  corpus rows: `TestRunCorpusLoadRejectsInvalidCorpusListingEveryViolation`,
  `TestRunCorpusLoadValidatesBeforeTouchingTheDatabase`,
  `TestRunCorpusLoadValidCorpusFailsOnUnreachableDatabase`,
  `TestRunCorpusActivateFailsOnUnreachableDatabase`, and
  `TestRunCorpusCalibrateFailsOnUnreachableDatabase`.
- **The crafted calibrate grid ran only in memory.** `tests/calibrate.rs` adds
  `main.TestRunCorpusCalibrateNamesTheInversionAndTheOutlierAndNothingElse` end to end on
  Postgres.
  - It seeds the Go grid: four columns, an older tier-3 row that the latest row supersedes, and
    an unmeasured column.
  - It then runs `maverick corpus calibrate`. The report equals
    `calibrate.json#report_crafted_grid` byte for byte.
  - So the latest-row and measured-only rules of the query are pinned. The unit tests in
    `src/corpus.rs` still check the tier findings against `calibrate.json#tier_findings`.
- **Two more rows were tested but not tagged.**
  - `tests/corpus.rs` runs `corpus calibrate` on an empty aggregates table and checks the exact
    `report_empty` text, so it carries `main.TestRunCorpusCalibrateEmptyAggregatesPrintsNoData`.
  - Its Postgres harness carries `main.TestMain`, as `store.TestMain` does in `store_go.rs`.
- **The query rule is now a test.** `crates/maverick-store/tests/query_coverage.rs` reads
  `parity/schema/queries/*.sql` and the store source. Each named Go query must map to exactly
  one store function, found by the Go name in the first line of its doc comment. A test under
  `crates/maverick-store/tests/` must call that function.

## The checklist

1. **The normalized `pg_dump` comparison passes.**
   `tests/schema_parity.rs::the_baseline_migration_equals_the_go_schema` migrates to the
   baseline and compares the dump with `parity/schema/schema.sql`, statement by statement. The
   sqlx and graphile_worker bookkeeping objects are left out (D21, D22).
2. **Every `store` row has a Rust test.** 92 of 92 are tagged: 45 config rows, 2 id rows, 33
   CLI rows, and 12 store rows. The 9 `internal/tasks/store_test.go` rows of `measurement-io`
   are tagged in `tests/corpus_store.rs` (144 of 144 in that area).
3. **Every query in `parity/schema/queries/` exists in Rust with a store test.** There are 95
   named queries in 19 files. `tests/query_coverage.rs` maps each to its function and finds a
   store test that calls it.
4. **`.sqlx/` is current.** `scripts/sqlx-prepare.sh --check` passes against a throwaway
   `postgres:16-alpine`, published on 127.0.0.1 only.
5. **D37: the migrations are up only, and both replacement tests pass.**
   - `tests/schema_parity.rs` checks that the baseline equals the Go schema.
   - `tests/migrations.rs::the_additive_migrations_add_exactly_the_spec_objects` checks the
     Rust additive migrations.
   - `tests/migrations.rs::a_second_run_applies_nothing_and_changes_nothing` checks the rerun.
   - `tests/migrate.rs` checks that a changed migration history is refused, and that an
     unreachable database fails before any migration.
   - The go-live starts on a fresh database, so there is no `adopt-goose-baseline` step.
6. **No row is left to a later package.** The CLI rows that needed other commands (`serve`,
   `mcp-stdio`, `demo-run`) are all implemented and tagged; `OTHER_BEADS` in `tests/cli.rs` is
   empty.

## Gates

- `cargo fmt --all --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `scripts/check-instrumentation.sh`: pass (285 files).
- `cargo deny check`: pass (advisories, bans, licenses, sources ok).
- `scripts/sqlx-prepare.sh --check`: pass.
- `cargo test -p maverick-store -p maverick-config -p maverick-testdb -p maverick` on real
  Postgres: pass (143 tests).

## Divergences

No new divergence is introduced. The package's tests rely on these existing rows:
- D21: the sqlx baseline.
- D22: graphile_worker, not River.
- D23: provider base URLs come only from the engine config, never from ambient SDK
  variables.
- D37: up-only migrations.
