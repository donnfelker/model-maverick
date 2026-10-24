# Public matrix bundles

The origin exports a bundle after each completed `aggregate_rebuild` (D49).
Set `MAVERICK_MATRIX_EXPORT_DIR` to a writable directory and
`MAVERICK_MATRIX_SIGNING_KEYS` to comma-separated `kid:path` pairs. Each path
contains a runtime PKCS#8 Ed25519 PEM private key. A kid has 1 to 64 ASCII letters,
digits, underscores, or hyphens. Two pairs sign with both keys during rotation.
Keep the keys outside the checkout and outside the public export directory.
With no export directory, the worker registers no export job. Replicas ignore
these settings and register no export job.

The rebuild commits the export job with its completion time and active matrix
version. The job reads one consistent snapshot of explicit global fields:

- `catalog.json`: model identity, settings, status, pricing, aliases, retention,
  quality source, constraints, and catalog dates.
- `aggregates.jsonl`: rows for that matrix version from the rebuild night and
  preceding 29 nights, inclusive.
- `calibration.json`: the public capability-level calibration series.
- `checks.json`: static public check definitions returned by recommend.

It reads no corpus tasks, tenant policy, recommendations, outcomes, predictions,
run outputs, or grader evidence. The integration test loads the synthetic parity
corpus and a populated tenant scenario, then checks the archive and decoded
payloads for private fields and canaries.

The archive is an uncompressed ustar file. Its root is `mmbundle1/`. File paths
are sorted. Headers have zero timestamps and owner ids. JSON is compact, with
stable object ordering. Each aggregate is one JSON line ending in a newline.
The manifest has `format`, `matrix_version`, `published_at`, `origin_release`,
`schema: {min, max}`, and `files`, a map from the four payload paths to lowercase
SHA-256 hashes. Each `signatures/<kid>` contains the raw 64-byte Ed25519 signature
over the exact `manifest.json` bytes. No signature covers a re-encoded manifest.
The schema minimum is 11. The maximum is the signed 64-bit maximum, meaning that
later additive migrations remain compatible with this format. Breaking bundle
changes require a new format id.

The reader requires at least one valid signature from the caller's trust set.
It ignores unknown kids, then verifies all four hashes, the format, the schema
range, and the typed payloads. It rejects unexpected paths, duplicate entries,
links, metadata entries, and nonzero trailing data. Replica import must call
`maverick_bundle::verify`; `inspect` checks hashes but does not authenticate an
archive. Replicas pass `maverick_bundle::trust::release_keys()`, the public keys
compiled into the release (`TRUSTED_KEYS` in `crates/maverick-bundle/src/trust.rs`).
Environment settings never add a reader trust key. Key custody, rotation, and
compromise are in `docs/operations.md`, "Bundle signing keys". Tests use the fixed
`trust::test_keys` (the `test-keys` feature), which no release trusts.

An archive is named `mmbundle1-YYYYMMDDTHHMMSS.nnnnnnnnnZ.tar`. Its time is the
rebuild completion time, in UTC. Publication uses an atomic hard link and never
overwrites an existing archive. A retry keeps the original bytes even if the
database or signing configuration changed. A changed retry emits a warning with
only the archive name. The directory is synced before the index can reference a
new archive, then synced again after the index rename. A Postgres transaction advisory lock
serializes publishers through the archive and index writes, including when a
worker delivery times out while a filesystem write continues.

`index.json` is an atomic replacement, newest first. Each entry has
`matrix_version`, `published_at`, `format`, `file`, `size`, and `sha256`.
The index is rebuilt from retained archives, so a retry repairs an interruption
between archive publication and index replacement. A corrupt retained archive
stops publication instead of hiding corruption. The filesystem must support
hard links and atomic rename within this directory.

The deployment pipeline copies the export directory to public static HTTPS
storage. The engine has no bundle HTTP endpoint. Keep every published bundle
for at least 12 months. Never copy signing keys into that storage.

## Import on a replica

Run migrations first, then set `MAVERICK_ROLE=replica` and run:

```sh
maverick matrix import /path/to/bundle.tar
maverick matrix status
```

Import checks the compiled release keys, payload hashes, format, and database
schema range before writing data. One transaction installs the catalog,
aggregate and calibration rows, and active bundle metadata. It preserves local
pricing and every existing aggregate key, including local latency rows. New
catalog references resolve after all column ids exist. Any failure rolls the
transaction back. No corpus is loaded.

If the replica bootstrapped before its catalog arrived, the first import completes
its missing default pool with all active columns. It does this only for an install
with exactly one tenant and no pool. Later imports preserve every existing pool
and its membership. The import serializes this step with bootstrap and pool
replacement, and audits `pool.initialize` after the transaction commits.

An import prints `matrix bundle imported`. Importing the active bundle again
prints `matrix bundle already active` and changes nothing. Both exit with 0.
Configuration, file, verification, schema, and import failures exit with 1.
Usage errors exit with 2. The command refuses to import on an origin.

An older bundle is refused. For an intentional rollback, run
`maverick matrix import /path/to/older.tar --allow-older`. This flag is never
used by the pull job. Existing aggregate keys remain append-only during rollback.

The real trust table stays empty until the owner supplies the public keys
(bead `tgm2`). Normal binaries then refuse every test-signed bundle. The sealed
test harness can build with `--features maverick/test-keys` in the debug profile
to accept the compiled test keys. Release binaries always use the real trust
table, including when that feature is selected. No environment setting changes
the trust set.

## Optional hourly pull

Set `MAVERICK_MATRIX_PULL_URL` to the directory holding `index.json` and the
archives. It accepts HTTP or HTTPS mirrors, without URL credentials, a query, or
a fragment. The setting is read only on replicas. Unset is the sealed default:
the worker registers neither `matrix_pull` nor its cron trigger. A configured
replica enqueues it at minute zero of each UTC hour.

The pull selects the newest entry in a supported format, regardless of index
order. It accepts only a canonical archive filename within the configured
directory. It checks the downloaded size and SHA-256 against the index, verifies
the signature and payloads, and checks that the signed identity matches the
index. The import transaction refuses an older bundle even if another import
finished while the pull was downloading. An edited index cannot authorize a
signature or a rollback. If the newest index identity is already active, the
pull skips downloading the unchanged archive.

Each HTTP request has a 10-second connection timeout, a 60-second read timeout,
and a five-minute overall timeout. It follows at most three redirects, each to
HTTPS without URL credentials. An HTTPS request never downgrades to HTTP. The
index limit is 4 MiB; the archive limit is 256 MiB. The job has the analytics timeout
and five attempts. Failure leaves the active matrix in service.

A successful delivery logs `matrix pull complete`, with `outcome=imported` or
`outcome=already_active`. A failure logs `matrix pull failed` with a bounded
`kind`, such as `http`, `format`, `bundle`, or `import`. The job timeout logs
`job handler timed out` with `job=matrix_pull`. Logs contain no mirror URL or
remote response body.
