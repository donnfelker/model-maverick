# Bundle format fixtures (D49)

Each directory here is one bundle format, named by its format id (`mmbundle1`). Each
`<name>.tar` in it is a bundle that format's writer produced, signed with the fixed test
keys in `maverick_bundle::trust::test_keys`. Its `<name>.expected.json` records what the
bundle decoded to when it was written: the matrix version, `published_at`, the kids
whose signatures verified, and the public matrix.

`tests/fixtures.rs` runs in every CI run. Every fixture must verify against the test
trust set and decode to exactly its `.expected.json`. A release build trusts no test
key, so it refuses all of them (self-hosted plan §7.7, "Bundle compatibility").

## Rules

- Never edit or regenerate a fixture. It is what that format's writer produced, and the
  test is only worth something while it stays that way. A fixture that stops loading is
  a compatibility break, not a stale file.
- Fixtures stay forever, for every format a release still reads.
- Each format has a fixture signed by the current and standby test keys, and one signed
  only by the standby test key.
- When a release adds a format, add its directory with the ignored writer, from the
  commit that adds the format:

  ```sh
  cargo test -p maverick-bundle --test fixtures -- --ignored write_the_format_fixtures
  ```

  The writer refuses to replace an existing file. Then commit the new directory.
- The fixtures carry only public data and signatures. The test key seeds are in the
  source, and no private key file belongs here (`check_private_keys` in `scripts/ci.sh`).
