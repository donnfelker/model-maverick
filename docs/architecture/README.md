# Architecture

This directory explains how Model Maverick is built, one document per area. Start with the overview, then read the area that matters for your change. The [`specs/`](../../specs/) directory is the normative contract; these documents describe the implementation and link back to the spec and to `parity/DIVERGENCES.md` for the approved differences from the old Go behavior.

## Documents

- [Overview](overview.md): the component map and how a request and the nightly benchmark flow through the system.
- [Request path](request-path.md): the trial-proxy chat request, from alias to recommendation to relay, with the fail-open ladder.
- [Trial proxy](trial-proxy.md): hosted and self-hosted modes, chain execution, and how credentials are resolved.
- [Nightly benchmark](nightly-benchmark.md): the corpus, the runner, scoring, and the rankings rebuild, under the spend cap.
- [Data model and store](data-model.md): the Postgres tables, migrations, and sqlx offline mode.
- [Jobs and scheduling](jobs-and-scheduling.md): the `graphile_worker` job kinds, cron, leases, and the shutdown rule.
- [Telemetry and outcomes](telemetry-and-outcomes.md): `/v1/telemetry`, the batcher, and why there is no `POST /v1/outcomes`.
- [Privacy and zero retention](privacy-and-zero-retention.md): the redaction rule, the fail-closed zero-retention rule, and debug capture.
- [Observability](observability.md): structured tracing, the `skip_all` rule, OTLP export, and alerts.
- [Testing and parity](testing-and-parity.md): the parity data, the strict manifest audit, the CI tiers, the acceptance, end-to-end, and latency harnesses, and mutation testing.

## The one-paragraph version

Model Maverick recommends which LLM to call for a given workload, and it proves those recommendations with a nightly benchmark. The engine (the `maverick` binary) serves recommendations over HTTP and runs the benchmark jobs on Postgres. The trial proxy (the `maverick-proxy` binary) is a separate deployment that a customer's traffic flows through: it asks the engine for a ranked chain, then relays the call to the first runnable model using only the customer's own provider credentials. Both share one library, `maverick-core`, so the proxy and the customer SDKs behave the same way. The engine never executes customer inference, never stores a prompt outside an encrypted debug window, and never lets an MM outage break a customer call.
