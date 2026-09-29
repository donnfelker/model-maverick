# API black-box corpus (Go oracle recording)

This directory is the differential-test input for the Rust port of the HTTP API, MCP transport, and internal seam (master plan §7.0 step 1 and step 2(d)). It was produced by running the Go contract suite `go test ./internal/api/...` (all 208 test functions passed, 0 skipped, 0 failed; the 209th `func Test*` is the `TestMain` harness) with a temporary recorder added in a scratch copy of the repository. The real repository was never edited.

Files:

| File | What it is |
|---|---|
| `api-recorded.jsonl` | 619 HTTP exchanges, one JSON object per line, in execution order. |
| `api-seed-sql.jsonl` | 1498 SQL statements that the test code itself (not the server) ran against Postgres: seeds, direct state mutations, and assertion reads, with full SQL text and bound arguments. |
| `api-timeline.md` | Generated per-test view that interleaves the two files above (seed statement names and request lines), for humans. |
| `README.md` | This file. |

Behavioral meaning of each endpoint, every error string, and the given/when/then for every test are in `reference/engine-reference.md (Part 5)`. Pure input/output cases (envelope writer, middleware, rate limiter, health component, telemetry derivation, MCP tool list) are in `../fixtures/api/*.json`.

## 1. How the recording was made

1. The repository was copied to a scratch directory. Docker Desktop could not start on the recording machine, so instead of testcontainers the suite ran against a local PostgreSQL 16.4 (Homebrew) on 127.0.0.1, database `maverick_rec3`, created empty and migrated by the suite's own `TestMain` (`store.Migrate`, the goose migrations plus River's). The only change to `TestMain` was: if `API_TEST_DATABASE_URL` is set, use it instead of starting a container. Everything else in the suite is unchanged, so the Go image `postgres:16-alpine` and a Homebrew Postgres 16 behave the same for this corpus.
2. Every `http.Handler` returned by `server.Public()` and `server.Internal()` in the test environment constructors (`newTestEnv`, `newInternalEnv`, `newPromptEnv`, the e2e env, and the one-off closed-pool server in `TestUpReturns503WhenDatabaseUnreachable`) was wrapped with a recorder. The wrapper sees the request exactly as the Go router does, including requests that arrive over a real socket through `httptest.NewServer` (all MCP traffic). It buffers the request body, lets the real handler run, and appends one line per exchange.
3. Every `pgxpool.Pool` built by those constructors got a pgx query tracer. It logs a statement only when (a) the context is not a request context (the recorder marks request contexts), and (b) the call stack contains a frame from a `*_test.go` file. So `api-seed-sql.jsonl` holds what the test did directly to the database, never what the server did while handling a request, and never background work of the River workers.
4. A shared global sequence number `gseq` is stamped on both files so the exact interleaving of seeds and requests can be reconstructed.

Tests run sequentially (no `t.Parallel` anywhere in the package), so the order in the files is the real execution order: Go runs test files in lexical file-name order and tests in source order within a file (`capture_read_test.go`, `capture_test.go`, `config_test.go`, `e2e_phase2_test.go`, `health_internal_test.go`, `history_test.go`, `internalv1_captures_test.go`, `internalv1_test.go`, `mcp_test.go`, `models_test.go`, `policies_test.go`, `recommend_prompt_test.go`, `recommend_test.go`, `server_test.go`, `telemetry_test.go`).

## 2. Record formats

### 2.1 `api-recorded.jsonl`

```json
{
  "test": "TestConfigPoolsRoundTrip",
  "seq": 3,
  "gseq": 812,
  "listener": "public",
  "request": {
    "method": "PUT",
    "path": "/v1/config/pools",
    "content_length": 123,
    "headers": {"Content-Type": ["application/json"]},
    "authorization": "Bearer <api_key sha256=1a94...>",
    "auth_kind": "minted_api_key",
    "body": "{\"pools\":[...]}"
  },
  "response": {
    "status": 200,
    "headers": {"Content-Type": ["application/json"], "X-Request-Id": ["01M3..."], "X-Ratelimit-Limit": ["300"], "X-Ratelimit-Remaining": ["299"], "X-Ratelimit-Reset": ["1783339260"]},
    "body": "{\"pools\":[...]}\n"
  },
  "duration_ms": 2
}
```

| Field | Meaning |
|---|---|
| `test` | `t.Name()` of the Go test that built the environment. Subtests run through the parent's environment, so their requests carry the parent test name (for example every row of `TestRecommendRequestErrors` is under `TestRecommendRequestErrors`). The one exception is `TestRecommendPromptHighStakesTagDerivation`, which builds an environment per subtest, so its records carry `TestRecommendPromptHighStakesTagDerivation/medical_tag` and `.../non-stakes_tags`. |
| `seq` | 1-based order of the exchange within that test name. Subtests of a table test appear in the table's order; where the Go test iterates a map (Go map order is random) the order of those subtests in this file is simply the order that happened in this run. The behavior doc marks those tests. |
| `gseq` | Global order across both files. |
| `listener` | `public` (the :8080 handler) or `internal` (the :8081 handler). |
| `request.path` | `r.URL.RequestURI()`: path plus raw query string, exactly as sent (percent-encoding preserved). |
| `request.content_length` | Go's `r.ContentLength`: the declared length, `-1` for unknown length (chunked). `httptest.NewRequest` sets it from the body for in-memory requests, so declared-length semantics apply to nearly all records. `TestConfigOversizeBodyChunked413` is the one chunked (unknown length) request. |
| `request.headers` | All request headers except `Authorization`. In-memory requests (`httptest.NewRequest`) carry almost no headers; socket requests (MCP) carry the Go client's headers. |
| `request.authorization` / `auth_kind` | The `Authorization` header, redacted by kind: `none` (absent, `""`); `minted_api_key` (a well-formed `mk_live_` + 43 char key, shown as `Bearer <api_key sha256=HEX>` where HEX is SHA-256 of the raw key, which equals the `key_hash` argument of the `CreateAPIKey` seed statement or the SHA-256 of the `key` returned by `POST /internal/v1/tenants/{id}/api-keys`); `internal_token_literal` (`Bearer label:secret`, shown verbatim: these are the test tokens `ops:alpha`, `rails:beta`, `ops:wrong`); `literal` (any other bearer, verbatim, for example `Bearer garbage`); `malformed_api_key_literal` (starts with `mk_live_` but wrong length, verbatim); `non_bearer_literal` (not `Bearer ` prefixed; redacted). |
| `request.body` / `response.body` | The raw body as a string when it is valid UTF-8 and at most 65536 bytes. Otherwise an object `{"truncated": true, "len": N, "sha256": HEX, "prefix": first 256 bytes, "all_same_byte": bool}`. Truncated bodies are listed in section 5 with the generator that rebuilds them exactly. |
| `response.headers` | All headers the handler set. Go canonicalizes names (`X-Request-Id`, `X-Ratelimit-Limit`); compare header names case-insensitively. Headers written by the Go HTTP server after the handler returns (`Date`, `Content-Length`, `Transfer-Encoding`) are not captured and must be ignored by the harness. |
| `response.body` | Every JSON body ends with exactly one `\n` (Go `json.Encoder`). Byte-exact comparison is meaningful after masking. MCP responses are SSE text (`event: message\ndata: <json>\n\n`). |
| `duration_ms` | Wall time inside the handler. Informational only. |

### 2.2 `api-seed-sql.jsonl`

```json
{"gseq": 4, "test": "TestTenantListCapturesShape", "sqlc_name": "CreateAPIKey :one",
 "sql": "-- name: CreateAPIKey :one\ninsert into api_keys (...) values ($1, $2, $3, $4, $5)\nreturning ...\n",
 "args": [{"go_type": "string", "json": "key_01M3..."}, {"go_type": "[]uint8", "hex": "1a94...", "json": "GpSq...="}, ...]}
```

`sql` is the exact statement text (sqlc-generated; `sqlc_name` is its name, empty for raw statements such as `begin`, `commit`, River's `LISTEN`). `args` are the bound parameters in `$n` order. `go_type` names the Go type (`string`, `bool`, `int16`, `int32`, `float32`, `[]uint8`, `[]string`, `pgtype.Text`, `pgtype.Date`, `pgtype.Timestamptz`, `pgtype.Float4`, `pgtype.Numeric`, ...). `json` is Go's JSON encoding of the value (pgtype values encode as their value or `null` when not valid); `[]uint8` also has `hex`. Replaying a statement with these arguments against the same schema reproduces the seed exactly, including capture ciphertext and nonces (the capture master key is fixed, see section 4).

Statement kinds present: writes (`CreateTenant`, `CreateAPIKey`, `RevokeAPIKey`, `UpdateTenant`, `CreateModelColumn`, `UpdateModelColumn`, `CreatePool`, `CreateTenantWeight`, `UpsertTenantConstraints`, `UpsertAggregate`, `UpsertCorpusVersion`, `UpsertTask`, `ActivateCorpusVersion`, `DeactivateCorpusTasksNotIn`, `AcquireCorpusActivationLock`, `CreateRecommendation`, `CreateDebugCapture`, `CreateCaptureWindow`, `DeleteExpiredDebugCaptures`, `InsertCaptureAccessLog`, `InsertAlert`, `UpsertCalibration`, River `JobInsertFastMany`/`PGNotifyMany`/`QueueCreateOrSetUpdatedAt`) and assertion reads (`GetRecommendation`, `ListDebugCapturesByTenant`, `ListCaptureAccessLogByCapture`, `ListAlertsByKind`, `GetLatestAggregate`, `GetTenant`, `GetModelColumn`, `ListModelColumnsByStatuses`, `ListActiveCaptureWindows`, `GetActiveCorpusVersion`, `GetCorpusVersion`, `ListRecommendationsPage`, `ListActiveTasksByCorpusVersion`). A harness replays the writes and may use the reads as post-condition checks (the expected results are the Go test assertions in `reference/engine-reference.md (Part 5)`).

## 3. Mapping records to tests

1. Group `api-recorded.jsonl` by `test`, order by `seq`. Group `api-seed-sql.jsonl` by `test`. Merge both by `gseq` to get the exact script: seed, request, seed, request, ...
2. `api-timeline.md` shows that merged script for all 189 tests that touched HTTP or the database. The two tests absent from it are `TestMain` (harness) and `TestRiverQueuesComponent` (pure function, `../fixtures/api/health_river_queues.json`). The 19 tests in `apierrors` and `middleware` never touch the wrapped server; they are in `../fixtures/api/apierrors.json` and `../fixtures/api/middleware.json`.
3. Assertions for each test (what must be true of the response and of the database afterwards) are in `reference/engine-reference.md (Part 5)` section "Test case catalog", keyed by test name.

## 4. Database state and environment per test

### 4.1 One shared database, accumulating state

All tests in `internal/api` except `TestPhase2ExitEndToEndRecommend` share one database for the whole run. Nothing is truncated between tests. Tests isolate themselves by creating fresh tenants, keys, and columns with random ULIDs, but several responses are global and therefore depend on everything earlier tests created:

| Global read | Affected tests | What to do |
|---|---|---|
| `GET /v1/models` and MCP `list_models` list every non-retired column in the database | every `/v1/models` 200 body, `TestMCPListModelsMatchesREST` | Replay the full sequence in order on one database, or compare as "contains these entries" (the Go tests only assert on their own columns). |
| `POST /internal/v1/tenants` builds the default pool from every `active` column in the database | `TestInternalTenantCreateDefaults`, `TestInternalTenantGetSummary`, `TestInternalAPIKeyRawReturnedExactlyOnce`, `TestInternalProvisionThenRecommendEndToEnd`, `TestInternalV1EachRotationTokenAcceptedWithAuditActor` | Same. `TestInternalTenantCreateWithoutActiveColumnsHasNoPool` first retires every active column in the database (visible as a run of `UpdateModelColumn` statements), which changes what later tests see. |
| Active corpus version (`matrix_version` on most bodies) | most | Many tests upsert `2026-07-01` as active. `TestInternalCorpusActivateFlipsActiveVersion` activates `int-corpus-b` and then restores `2026-07-01`. |
| `GET /internal/v1/alerts` (unfiltered) returns every alert row | `TestInternalAlertsFeedFilters` | Its assertions are "contains", not equality. |
| `GET /internal/v1/captures` (unfiltered) | `TestOperatorListCapturesFilters` | "contains". |
| `GET /v1/history/calibration` is matrix-global | `TestHistoryCalibrationListShape`, `TestHistoryCalibrationFiltersAndPagination` | The tests scope with `since=<scored_at of their first row>`; the harness must substitute its own row's `scored_at`. |
| `DeleteExpiredDebugCaptures` (run by the test) deletes every expired capture in the database | the three `...HardDeleted410` tests | Harmless for replay. |
| Classification cache keyed by prompt fingerprint | prompt-path tests | Tenants from `seedTenant` all have the all-zero HMAC key, so identical prompts from different tests share cache rows. `newCaptureEnv` tests send the same `capturePrompt`; the first one in the run classifies (`method` `classified`), later ones may hit the cache (`method` `cached`). Replay in order on one database to reproduce the recorded `method` values. |

The recommended harness strategy: create one fresh database from the migrated schema, replay the whole merged script in `gseq` order against the Rust binary, and compare each response with the recorded one after masking. Per the master plan, the differential run clones the same snapshot per binary; with this corpus the snapshot is "empty migrated schema" and the script is the seed.

`TestPhase2ExitEndToEndRecommend` creates its own database (`phase2_e2e_<unixnano>`), loads `../fixtures/api/testdata/e2e_corpus/`, and runs three nightly smoke jobs through real River workers with the fake provider before its two HTTP calls. Its aggregates are produced by the job system, not by seed statements, so it cannot be replayed from `api-seed-sql.jsonl` alone. Treat it as an end-to-end test of the jobs port; the expected values are in `reference/engine-reference.md (Part 5)`.

### 4.2 Environment constructors (server configuration per test)

| Constructor | Config (fields not listed are zero values) | Clock | Notes |
|---|---|---|---|
| `newTestEnv(t, mutate)` | `MAVERICK_INTERNAL_TOKENS` = `ops:alpha,rails:beta` (labels `ops`, `rails`); `RateLimitRPM` 300; `CatalogColdStartNights` 5; `CaptureMasterKey` = 32 bytes of `0xC4`; `CaptureMaxWindowMinutes` 1440; `CaptureRetentionHours` 72. Ranking bands, noise band, ceiling buffer all 0. `ClassifierColumn` empty. No provider API keys. | Fixed `2026-07-06T12:00:30Z` for the rate limiter and for internal-handler timestamps (`cold_start_until`). Everything else uses the wall clock. | Classifier: a client that always errors, so prompt-path requests get the fallback classification. |
| `withRecommendDefaults` (mutate) | `RankingBandQuality` 0, `RankingBandBalanced` 1.5, `RankingBandCost` 5.5, `RankingNoiseBand` 0.75, `CeilingBuffer` 0.05 | | |
| `withCeilingBuffer` (mutate) | `CeilingBuffer` 0.05 | | |
| `func(cfg){cfg.RateLimitRPM = N}` | RPM 2 (`TestV1RateLimitExceeded429`, `TestMCPRateLimitAppliesToMCPRequests`) or 1 (`TestV1RateLimitIsPerKey`) | | |
| `newInternalEnv(t, mutate, opts...)` | as `newTestEnv` | as `newTestEnv` | Server logs captured as JSON lines for audit assertions. `TestInternalColumnRetireBlockDeliversAlert` adds an alert notifier. |
| `newPromptEnv(t, stub)` | `newTestEnv` config + `withRecommendDefaults` + `ClassifierColumn` = `claude-haiku-4-5/direct` | as `newTestEnv` | Real classify resolver and cache over a stub provider client that returns a fixed text or error (section 4.3). |
| `newCaptureEnv(t)` | `newPromptEnv` with the extraction stub | | Also seeds: one active tenant (`seedTenant`), one API key, active corpus `2026-07-01`, one active column in pool `default` (is_default), and for `structured_extraction` the pass rates 0.97, 0.95, 0.91, 0.88, 0.86 (tiers 1..5) plus tier-3 quality 0.941, cost 0.0042, latency_p95 2100. |
| e2e (`TestPhase2ExitEndToEndRecommend`) | `config.Load` defaults with `MAVERICK_DATABASE_URL`, `MAVERICK_INTERNAL_TOKENS=test:secret`, `MAVERICK_CAPTURE_MASTER_KEY` = 64 hex zeros | Real clock | Own database. |

### 4.3 Classifier stub outputs (prompt path)

The stub sits at the provider-client seam; the real classifier parses its text.

| Test(s) | Stub text (exact) or error |
|---|---|
| all `newCaptureEnv` tests, `TestRecommendPromptFormClassified`, `...SecondRequestHitsCache`, `...OverCharLimit422`, `...AtCharLimitSurfacesTruncated`, `...RejectsDeclaredFields` | `{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"confidence":0.91}` |
| `TestRecommendPromptClassifierOutageFallsBack` | error `provider down` |
| `TestRecommendPromptHighStakesTagDerivation/medical_tag` | `{"capability":"factual_qa","difficulty":2,"domain_tags":["medical"],"confidence":0.85}` |
| `TestRecommendPromptHighStakesTagDerivation/non-stakes_tags` | `{"capability":"factual_qa","difficulty":2,"domain_tags":["internal","support"],"confidence":0.85}` |
| `TestReplayMismatch` (after the capture) | `{"capability":"code_generation","difficulty":4,"domain_tags":[],"confidence":0.88}` |
| `TestReplayClassifierFailureFallsBack` (after the capture) | error `classifier down` |

### 4.4 Common seed helpers (what the statements mean)

| Helper | Statements | Values |
|---|---|---|
| `seedTenant(status)` | `CreateTenant` (+ `UpdateTenant` if not active) | name `Contract Test Tenant`, `prompt_hmac_key` = 32 zero bytes. |
| `issueKey` / `seedActiveKey` | `CreateAPIKey` | fresh `mk_live_` key; `key_hash` = SHA-256(raw); `last4`; name `contract-test`. |
| `seedColumn(status)` | `CreateModelColumn` | provider `anthropic`, model_id `model-<column id>`, endpoint `direct`, settings `{}`, pricing `{}`. |
| `seedCatalogColumn(provider, model, endpoint)` | `CreateModelColumn` | status `active`, settings `{}`, pricing `{}`. |
| `seedPool(tenant, name, isDefault, ids...)` | `CreatePool` | |
| `seedAggregate(col, cap, tier, dim, ewma, asOf)` | `UpsertAggregate` | n 3, measured true, matrix_version `2026-07-01`. |
| `seedPassRates(col, cap, r1..rk)` | k x `UpsertAggregate` | dimension `pass_rate`, tiers 1..k, as_of 2026-07-01. |
| `seedFlatCost(col, cap, c)` | 5 x `UpsertAggregate` | dimension `cost_per_run_usd`, tiers 1..5. |
| `seedActiveCorpusVersion(v)` | `UpsertCorpusVersion` | description `""`, task_count 0, active true. |
| `seedStandingMaxCost(tenant, "0.01")` | `UpsertTenantConstraints` | numeric max cost. |
| `seedRecommendation(tenant, key, mutate)` | `CreateRecommendation` | a stored declared-path trace; the exact request/classification/candidates/response JSON is in the statement args. |
| `seedCalibration(batch, cap, brier)` | `UpsertCalibration` | overconfidence 0.03, n 150. |
| `seedExpiredCapture()` | `CreateRecommendation` + `CreateDebugCapture` | ciphertext/nonce sealed with the fixed master key for capture id; `expires_at` = now - 1h. |
| `insertTestAlert(kind, severity, subject)` | `InsertAlert` | id `alr_<kind><unixnano>` (not a ULID). |

## 5. Nondeterministic fields to mask

Mask these before comparing a Rust response to the recorded one. Everything else is expected to be byte-identical (after the JSON-level rules in the last row).

| Field | Where | Rule |
|---|---|---|
| Prefixed ULIDs `ten_`, `key_`, `col_`, `pool_`, `rec_`, `out_`, `cw_`, `cap_`, `cal_`, `alr_`, `bat_` + 26 Crockford chars | paths, bodies, SQL args | Map consistently: the first id seen in a test gets the same alias in both runs. Ids are generated by the test (seed args) or the server (response bodies); a harness that re-seeds creates its own ids and must build the mapping from seed order. Ids also sort: several lists are ordered by id, so the mapping must preserve relative order (ULIDs are time-ordered; re-seeding in the same order preserves it). |
| `alr_<kind><unixnano>` | `TestInternalAlertsFeedFilters` | alias. |
| `X-Request-Id` response header | every response | Minted ULID (26 chars) unless the request sent one (1..128 bytes), in which case it is echoed. No recorded request sends one. |
| `Mcp-Session-Id` response header and request header | MCP | Opaque session id (26 chars, base32) chosen by the MCP library. Map consistently. |
| RFC 3339 timestamps (`created_at`, `updated_at`, `starts_at`, `ends_at`, `captured_at`, `expires_at`, `scored_at`, `revoked_at`, `acked_at`) | bodies | Mask the value. Note the Go server renders them in the process's local time zone with the zone offset (this recording: `-04:00`; production containers run UTC and render `Z`), with microsecond precision and trailing fractional zeros trimmed. Relations still checked by tests: `ends_at - starts_at = duration_minutes`, `expires_at = window ends_at + 72h`. |
| Date-only values (`as_of`, `cold_start_until`, `deprecation_date`) | bodies | Deterministic. `cold_start_until` comes from the fixed clock (2026-07-11 for 2026-07-06 + 5 nights) and must match exactly. |
| `X-Ratelimit-Reset` | authenticated responses | Deterministic `1783339260` (= 2026-07-06T12:01:00Z) under the fixed clock; wall-clock based in the e2e test. |
| Pagination cursors (`next_cursor`, `cursor=` in request paths) | history lists, capture lists | Opaque tokens that encode ids (capture cursors are unpadded base64url of the last capture id). Mask; when a request path carries a cursor from a previous response, substitute the Rust run's cursor from the corresponding previous response. |
| `since=` query values copied from DB rows | `TestHistoryRecommendationsFilters`, `TestHistoryCalibration*`, `TestInternalAlertsFeedFilters` | Substitute the Rust run's row timestamp. |
| Raw API keys and `last4` | `POST /internal/v1/tenants/{id}/api-keys` response `key`, tenant summary `last4` | Random. Map by SHA-256. |
| `prompt_hash` | prompt-path responses and traces | `hmac_sha256:` + 64 hex. Deterministic for tenants created by `seedTenant` (all-zero HMAC key) given the prompt; random for tenants created through `POST /internal/v1/tenants`. |
| `recommendation_id` inside bodies and trace snapshots | recommend responses, MCP results, history traces | ULID alias. |
| Database name `phase2_e2e_<unixnano>` | e2e only | n/a. |
| Model lists and tenant default pools that include all columns in the shared DB | see 4.1 | Replay the whole script, or compare only the test's own entries. |
| `components.db.detail` of a failed ping | `/health/detail` degraded | pgx error text; not in this corpus. |
| JSON formatting | all | Compare bytes, not parsed JSON: key order, number spelling (`0.800000011920929`, `3.0` inside jsonb pass-through, float32 shortest forms like `0.8`), `null` vs omitted, and the trailing newline are all part of the contract (see `reference/engine-reference.md (Part 5)` section "Wire encoding rules"). |

## 6. Truncated bodies and their generators

Bodies over 64 KiB (or not UTF-8) were stored truncated. Six request bodies were truncated, no response bodies. All six are synthetic and rebuildable exactly:

| Test | Body |
|---|---|
| `TestV1OversizeBody413` | request: `a` repeated 2097153 times (2 MiB + 1), no JSON. |
| `TestV1BodyAtCapIsNotRejected` | request: `a` repeated 2097152 times. |
| `TestConfigOversizeBodyDeclaredLength413`, `TestConfigOversizeBodyChunked413` | request: `{"pools": [{"name": "` + `a` x 3145728 + `", "model_column_ids": ["x"], "is_default": true}]}`; the chunked one is sent with unknown length (`content_length` -1). |
| `TestRecommendPromptOverCharLimit422` | request: `{"workload":{"prompt":"` + `a` x 500001 + `"}}` |
| `TestRecommendPromptAtCharLimitSurfacesTruncated` | request: `{"workload":{"prompt":"` + `a` x 500000 + `"}}` |
| `TestTelemetryEnvelopeInvalidRequest422` (501 items) | request: `{"items":[` + `null` x 501 joined by `,` + `]}` (under 64 KiB, stored verbatim). |

Each truncated record keeps `len`, `sha256`, and the first 256 bytes, so a rebuilt body can be verified.

## 7. Known limits of this corpus

- The stdio MCP transport is not HTTP and is not in `api-recorded.jsonl`; `TestMCPStdioStyleTransportBindsRunContextIdentity` is described in `reference/engine-reference.md (Part 5)`. Its REST half (one `POST /v1/recommend`) is recorded.
- Server-side log lines (audit entries, `catalog_changes`) are asserted by the internal tests but are not part of this corpus; their exact shape is in `reference/engine-reference.md (Part 5)`.
- Side effects inside the server (trace rows, access-log rows, alerts) are checked by the tests through the assertion reads in `api-seed-sql.jsonl` and the catalog in `reference/engine-reference.md (Part 5)`.
- The recording ran against Homebrew Postgres 16.4, not the `postgres:16-alpine` container, because Docker Desktop failed to start (privileged-helper prompt). The schema and SQL are identical; no test depends on the Postgres build.

## 8. The Rust replay

`crates/maverick-api/tests/replay.rs` (bead 5.15) replays this corpus against the Rust engine. It runs in the CI parity tier (`scripts/ci.sh parity`) and in `cargo test --workspace`.

- **One database.** It uses one database with the engine migrations and graphile_worker's schema. Every seed statement and exchange runs in `gseq` order. Seeds run on one connection, so `begin` and `commit` pair up. Each statement is prepared, and its arguments are bound by the parameter types that Postgres infers.
- **Adaptations.**
  - River statements are skipped.
  - `CreateAPIKey` stores the hash of a key minted by the harness. A request that carries a Go key's hash sends that key. A key the server mints maps by the Go key's hash.
  - `CreateDebugCapture` gains `capture_key_version` 1 (D18).
  - Seed timestamps move from the recording's clock to the harness clock at the same `gseq`.
- **Id order.** Every seeded id maps to a fresh id from the engine's own monotonic generator when its seed runs. Seeded ids and server-minted ids therefore sort in the same order as in the Go run.
- **Masking and mapping (section 5).** Ids, raw keys, `last4`, prompt hashes, cursors, timestamps, and MCP session ids pair by position in each compared response. Later requests carry our values.
  - Two kinds of value come from the database rather than a response: ids the test read back, and `since=` values from rows its seeds returned. The harness takes both from our run's identical statements.
- **Environments.** Each test gets the environment its Go file's constructor built (section 4.2): the ranking knobs, the rate limit, and the classifier stub (section 4.3).
- **Skipped test.** `TestPhase2ExitEndToEndRecommend` builds its own database and runs the jobs system, so it is not replayed. `crates/maverick/tests/e2e.rs` covers it.
- **Allowed differences.** Every other difference must match `replay-allowlist.json`, which names the DIVERGENCES row for each.
  - Rules strip Rust-only members, rewrite a value a row changes, or mask library text.
  - Entries exempt named parts of one exchange.
  - Set `MAVERICK_REPLAY_REPORT=<file>` to write every difference, explained or not, as JSON.
