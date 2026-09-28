# Crate Layout

This workspace keeps pure decision code away from I/O-bound crates. The domain crate is the target for fast mutation tests. Store is the only crate that owns SQL.

| Crate | Role |
|---|---|
| `maverick-domain` | Pure IDs, task and grader types, scoring, ranking, policy, ceilings, checks, reasons, spend math, classifier math, fingerprints, and capture crypto. No Tokio I/O and no SQL. |
| `maverick-config` | Environment parsing and startup validation. |
| `maverick-store` | sqlx pools, migrations, queries, and transactions. |
| `maverick-providers` | Provider clients, the deterministic fake provider, retry classification, and wire parsing. |
| `maverick-runner` | Batch run composition and provider execution orchestration. |
| `maverick-jobs` | graphile_worker handlers, batch leases, job chaining, and job recovery glue. |
| `maverick-api` | axum REST API, MCP surfaces, middleware, and the internal seam. Business rules stay in domain and services. |
| `maverick-alerts` | Alert persistence fan-out helpers and webhook/email delivery glue. |
| `maverick-core` | Shared SDK/proxy core: alias grammar, workload projection, recommend client, chain executor, fail-open ladder, and telemetry batcher. |
| `maverick-proxy` | Trial proxy binary. It has no database. |
| `sdk-conformance` | Conformance harness library and binary for mock-engine and mock-provider scenarios. |
| `maverick` | Engine CLI and process wiring. |
| `parity-support` | Dev-only test helpers for loading parity fixtures and asserting approved divergences. |
