# Observability

The engine and the proxy emit structured traces and metrics. The one hard rule is that nothing customer-sensitive is emitted. See [Privacy and zero retention](privacy-and-zero-retention.md) for the redaction guarantee this builds on.

## Tracing

Both binaries use `tracing` with a JSON subscriber, so every log line is a structured event that a collector can parse. Spans carry request context (ids, the tenant, the workload capability), never content.

The redaction rule is enforced at the span boundary:

- Every `#[tracing::instrument]` uses `skip_all` and names an explicit allowlist of fields. A function argument is captured only if it is on the allowlist, so a prompt, a body, or a key is never captured by accident.
- The `instrument-lint` crate is a CI check (D19) that fails the build for any `#[instrument]` without `skip_all`.
- In tests, `test-support` captures the emitted traces and runs a secret-leak guard, so a test fails if a known secret appears in the logs.

## OTLP export

When `MAVERICK_OTEL_EXPORTER_ENDPOINT` is set, the engine exports traces and metrics over OTLP with `opentelemetry-otlp` and `tracing-opentelemetry`. An empty endpoint disables the export, so a local run needs no collector. `MAVERICK_OTEL_SERVICE_NAME` sets the service name (default `maverick`). The proxy has its own OTLP settings (`MAVERICK_PROXY_OTLP_ENDPOINT`, `MAVERICK_PROXY_OTLP_SERVICE_NAME`). It exports no spans. For each settled request, including one that dispatched nothing, it exports a `maverick.recommendation` log record and counts the request in `maverick_proxy_recommendations_total` (labels `measured` and `classification_method`). It continues the caller's W3C trace context into the engine and the providers (D31). A request whose recommendation says `telemetry: false` (the D39 opt-out) exports no record and is not counted (D43). A request that fails before the engine answers has no opt-out flag, so it is recorded.

The privacy acceptance suite (`maverick-acceptance`) points the engine's and the proxy's exporters at one mock OTLP collector. Its canary test flushes both pipelines and checks that no canary appears in any exported span, log record, or metric label.

## Metrics

The engine records operational metrics at their producers (batch progress, spend, job outcomes, queue depth) through the same OTLP pipeline. Metric labels follow the same redaction rule: no prompt, no completion, no credential, and no unbounded customer value as a label.

## Alerts

`maverick-alerts` turns operational conditions (a regression, a spend-cap trip, a failed batch) into alert rows and delivers them by SMTP (`MAVERICK_SMTP_URL`) and by a signed HMAC webhook (`MAVERICK_ALERTS_WEBHOOK_URL`, `MAVERICK_ALERTS_WEBHOOK_SECRET`). Alerts are the human-facing side of observability, separate from the trace and metric streams a collector ingests.

An unresolved classifier makes no classifier calls, so the classifier's failure-rate alert cannot fire. The engine raises its own `classifier_degraded` alert instead: critical, with subject `reason: "unresolved"`, once the classifier has been unresolved for 5 min. A later resolution writes an `info` alert of the same kind (`reason: "resolved"`). See D41 in [Request path](request-path.md).
