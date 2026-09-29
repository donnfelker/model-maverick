# Privacy and zero retention

Two rules shape more of this codebase than any other. They come from the guarantees in [`specs/model-maverick-v1-spec.md`](../../specs/model-maverick-v1-spec.md).

## Redaction

A prompt, a completion, a request body, or a credential never reaches a log line, a span, or a metric label. A prompt is never stored except in an encrypted debug-capture window.

The mechanism is a discipline the code and CI both enforce:

- Every `#[tracing::instrument]` uses `skip_all` and names an explicit allowlist of safe fields. Nothing is captured by default.
- The `instrument-lint` crate is a CI check (D19) that rejects any `#[instrument]` without `skip_all`, so the rule cannot be forgotten.
- The per-test tracing capture in `test-support` runs a secret-leak guard: a test fails if a known secret value appears in the captured logs.
- `Debug` implementations redact secrets. A tenant key, a provider key, and a prompt render as a redacted placeholder, so even an accidental `{:?}` leaks nothing.

The rankings the engine serves are derived from grades, not from customer prompts, so serving a recommendation reads no customer content.

## Debug capture

An operator can open a short debug-capture window for a tenant to diagnose a problem. Inside the window, request material is stored encrypted, with a bounded retention:

- The window has a maximum length (`MAVERICK_CAPTURE_MAX_WINDOW_MINUTES`) and a retention horizon (`MAVERICK_CAPTURE_RETENTION_HOURS`), and a cron job expires old rows.
- The data is encrypted with a master key (`MAVERICK_CAPTURE_MASTER_KEY`), with a key version and a previous-key slot so the key can be rotated. The crypto lives in `maverick-domain` (RustCrypto: AES-GCM, HKDF, HMAC).

Capture is the only exception to "never store a prompt," and it is encrypted, bounded, and opt-in.

## Zero retention

For a zero-retention tenant, the engine never sends a prompt or a pick to a column or an alias without valid zero-retention evidence. This rule fails closed, and it outranks the privacy guarantee and fail-open.

In practice:

- **The workload form.** An SDK core that knows a tenant is zero retention never projects the prompt for classification. It sends the declared form only (a header, or the declared fallback), so no prompt leaves the process. When a projection does reach the engine for a zero-retention tenant (for example from the proxy, which keeps no such memory), the engine classifies it only on a classifier column with valid zero-retention evidence (D2). Without one, it answers `422 zero_retention_requires_declared` before the prompt is hashed. This holds while the classifier columns are unresolved too (D41).
- **Fallback.** On an MM outage, the fail-open ladder keeps only the tenant's zero-stamped cached entries and only default models the operator has verified as zero retention. If none remain, it fails closed rather than call an unverified model.
- **OpenRouter (D15).** Each attempt forces the Zero Data Retention routing option, and an execution alias that is not verified `zero` is skipped. A chain with none left answers `409 no_compliant_columns` before any upstream request, with no direct-key fallback.
- **Terminal errors.** `422 zero_retention_requires_declared` and `409 no_compliant_columns` are surfaced unchanged, and they purge the tenant's cached chains.
- **Policies (D42).** For a zero-retention tenant, `GET /v1/policies` and the MCP `get_policy` tool list a column as a candidate only when its effective `retention_mode` is `zero` (expired evidence reads as `unknown`). This is the rule `/v1/recommend` applies (D1). A band with no such column has `column: null` and `clears_band: false`.

The SDK core (`maverick-core::failopen`) records that a tenant is zero retention when a recommendation says so, and every later call for that tenant then uses the declared form.

## Where this is verified

The privacy acceptance suite (`maverick-acceptance`) runs the real engine and the real proxy with recording fakes. Its canary test checks that no prompt, caller body field, or credential reaches a log line, a span, an OTLP export, or a metric label. Its other tests cover zero retention end to end and the telemetry opt-out. The conformance scenarios (`sdk-conformance`, groups 6, 9, and 10 to 12) cover the fail-open ladder, the zero-retention filtering, and the OpenRouter zero-retention rules. The instrument-lint check and the secret-leak guard run in CI on every build. See [Testing and parity](testing-and-parity.md).
