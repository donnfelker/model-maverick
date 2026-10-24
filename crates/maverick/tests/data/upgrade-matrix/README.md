# Upgrade matrix dumps (D50)

Each `<name>.sql` here is one release's database, with the fixed scenario that
`tests/upgrade_matrix.rs` builds: two tenants with fixed keys, two active columns
with aggregates, a named pool, constraints, and recommendation traces. The test
`every_dump_upgrades_and_the_database_still_serves` restores each dump into an empty
database, runs `maverick upgrade --yes`, and checks that the database still serves:
the tenants' keys, the recommendation history and its traces, the config, a declared
recommend, and the internal tenant read. It runs in every CI run (spec §14.5).

## At each release

1. From the release's commit, write its dump:

   ```sh
   MAVERICK_MATRIX_DUMP_NAME=<release> mm-cargo test -p maverick --test upgrade_matrix \
     -- --ignored write_the_dump_for_this_release
   ```

2. Commit the new `<release>.sql`.
3. Add the release's row (name, date, newest migration) to `RELEASES` in
   `crates/maverick-store/src/schema.rs`.

## Rules

- Never edit a dump. It is what that release wrote, and the test is only worth
  something while it stays that way.
- Remove a dump once its release falls out of the 12-month upgrade window. The window
  check refuses it then, by design.
- `pre-release.sql` is the database at migration 0012, before any release. A schema
  older than every release counts as the first release's, so it follows the first
  release's window.
- The replica scenario (the bootstrapped single tenant) joins when bead xq7v.11 lands.
