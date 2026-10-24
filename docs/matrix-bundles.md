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
archive. The signing-key custody work supplies the compiled public trust set.
Environment settings never add a reader trust key.

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
