# Crate layout

The Cargo workspace is at the repo root. Every crate lives here under `crates/`. The
version table and the shared lints are in the root `Cargo.toml`
(`[workspace.dependencies]`, `[workspace.lints]`). Each crate inherits them with
`{ workspace = true }`.

## Dependency direction

```
maverick (bin, CLI) ─┬─ maverick-api ──┬─ maverick-runner ─ maverick-providers ─┐
                     │                 │                                        │
                     ├─ maverick-jobs ─┼─ maverick-store ───────────────────────┤
                     │                 │                                        │
                     ├─ maverick-alerts┘                                        │
                     └─ maverick-config                                         │
                                                                                ▼
                                                                        maverick-domain
maverick-proxy (bin) ─ maverick-core                    (pure: no tokio I/O, no sqlx)
```

Rules that the split enforces:

- `maverick-domain` is pure. It has no Tokio I/O and no sqlx. `cargo-mutants` runs on
  it, so it stays fast to build. It holds ids, task and grader types, graders, scoring
  (EWMA and Welford), ranking, policy, ceilings, bands, checks, reasons, prediction and
  spend math, catalog lifecycle, capture crypto, fingerprints, key format, pure
  classifier logic, and telemetry derivation.
- `maverick-store` is the only crate with SQL. Everything reaches Postgres through it
  (spec §2.2).
- `maverick-api` has no business logic. It calls services, which call domain and store.
- `maverick-core` is the shared SDK core. `maverick-proxy` links it directly and has no
  database.

## Crates

| Crate | Kind | Role |
|---|---|---|
| `maverick-domain` | lib | Pure domain logic. cargo-mutants target. |
| `maverick-config` | lib | Environment and YAML config, validation. |
| `maverick-store` | lib | sqlx pool, migrations, queries, transactions. The only SQL. |
| `maverick-providers` | lib | Provider clients, the fake provider, retry. |
| `maverick-runner` | lib | The measurement runner (tasks through providers and graders). |
| `maverick-jobs` | lib | graphile_worker jobs, the `batches` lease, spend guard, alerts, catalog jobs. |
| `maverick-alerts` | lib | Alert derivation and delivery. |
| `maverick-api` | lib | axum listeners, endpoints, and MCP (rmcp). No business logic. |
| `maverick-core` | lib | The shared SDK core (recommend client, chain executor, fail-open ladder). Required name (AGENTS.md). |
| `maverick-proxy` | bin | The trial proxy. No database. Required name (AGENTS.md). |
| `sdk-conformance` | lib + bin | Mock engine, mock provider, twelve frozen scenarios. Required name (AGENTS.md). |
| `maverick` | bin | Engine binary and CLI: serve, migrate, corpus, mcp-stdio, demo-run. |

`parity-support` is the dev-only fixture-loading crate. The skeleton exists now. The
helpers are a separate bead (pkg1.4).

`maverick-testdb` is the dev-only Postgres helper for store and job tests. Each test
binary starts one `postgres:16-alpine` container, applies a `Schema` once to a template
database, and clones one database per test. Set `MAVERICK_TEST_DATABASE_URL` to use an
existing server instead. Without Docker the tests skip, except when `CI` or
`MAVERICK_TEST_REQUIRE_DB` is set.

## Stack choices worth noting

- TLS is rustls only. `reqwest`, `sqlx`, and `graphile_worker` are pinned with
  `default-features = false` and the rustls features. No OpenSSL is in the tree
  (`cargo tree -i openssl-sys` is empty). `cargo deny` enforces this in CI.
- YAML uses `serde_norway`, a maintained fork. The original `serde_yaml` is archived.
- OTLP export uses the HTTP + reqwest(rustls) transport, not tonic, to keep the tree on
  rustls.
- `graphile_worker` is pinned to the `0.13` minor. The plan allows owning a fork if it
  stalls. Its sqlx driver is the same job queue Postgres uses; there is no Redis.
