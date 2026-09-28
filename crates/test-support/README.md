# test-support

Shared test observability for the Model Maverick engine. Every failing test should explain
itself. This dev-only crate gives every package the same four tools, so production logs and
test logs read the same and parity failures are diagnosable without a re-run.

Not published (`publish = false`). Depend on it under `[dev-dependencies]`.

## Tools

### `init_test_tracing()`

Installs a per-test `tracing` subscriber, scoped to the current test thread, that captures
every event at `DEBUG` in the engine's JSON log shape (slog keys `time`, `level`, `target`,
`msg`, then the event fields, then `spans`). `RUST_LOG` narrows the captured levels. The
captured lines print to stderr only when the test fails.

```rust
let capture = test_support::init_test_tracing();
tracing::info!(method = "GET", path = "/v1/recommend", status = 200, "request");
let events = capture.captured_json();
```

The returned guard is thread-scoped, so tests run in parallel without seeing each other's
logs. Events emitted on other threads (for example spawned Tokio tasks) are not captured.

### Secret-leak guard (a G5 check for free)

The capture layer scans every rendered line for registered secrets. If one appears, the
guard fails the test when it drops. [`MAVERICK_TEST_CANARY`] is always registered; add more
with `init_test_tracing_with_secrets(&[..])` or `capture.register_secret(..)`. Plant the
canary in a prompt or request body and any accidental echo into a log, span field, or metric
label fails the test (spec §14.3).

A test that intentionally exercises the guard calls `capture.take_violations()` to drain the
recorded leaks so the drop guard does not then fail it.

### `step!(...)`

Logs a numbered step with the elapsed time since `init_test_tracing()` ran, so a failure
shows the last step that completed. Use it in multi-step tests (store, replay, e2e,
acceptance).

```rust
let _capture = test_support::init_test_tracing();
test_support::step!("load config");
test_support::step!("insert {} rows", rows.len());
```

### Diff reporters

- `format_json_diff(expected, actual, anchor)` returns a field-path diff such as
  `$.recommendations[1].score: expected 94.1, got 94.2`, headed with the fixture anchor.
  `format_json_diff_tagged(.., divergence)` also names an approved divergence row.
- `bytes_diff(expected, actual)` returns a hex-and-text window around the first differing
  byte, for wire bodies, SSE frames, and rewritten bodies.
- `float_diff(expected, actual)` shows both values in full `f64` precision and as `f32` bits.
- `assert_json_eq`, `assert_bytes_eq`, and `assert_float_eq` panic with the readable diff.

### `ReplayReport`

Record one outcome per replayed exchange (`passed`, `allowlisted` by a divergence row, or
`unexplained`), then print `summary()` and call `assert_clean()`. `assert_clean()` fails the
run and prints a detail block for every unexplained difference.

## Conventions

- Name each parity test after its `go_test` tag (see bead 1.5), for example
  `/// parity: decision/policy.TestEvaluatePoolRestriction`.
- One assertion message per fixture case; pass the fixture anchor to the diff reporters.
- Never log prompt text, completions, request bodies, credentials, or corpus content. The
  secret-leak guard is a backstop, not a license to log sensitive data.
