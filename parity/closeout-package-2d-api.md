# Package 2d-API parity closeout

- Bead: `model-maverick-rust-port-3hk.5.18`
- Date: 2026-09-29
- Basis: committed `main` plus the tag and test changes in this same commit.
- Reproduce:
  - `cargo run -p parity-support --bin parity_coverage -- --list-missing`
  - `MAVERICK_REPLAY_REPORT=/tmp/replay.json cargo test -p maverick-api --test replay -- --nocapture`
  - `cargo test -p maverick-api --test contract`

## What this is

Package 2d-API is the HTTP API crate `maverick-api`: the public `/v1` listener, the internal
provisioning seam, MCP (streamable HTTP and stdio), health, and the middleware. Its parity
areas are `api` (`parity/manifest/api.json`) and the API-facing rows of `decision`
(`auth.*`). This report checks each item of the 5.18 checklist.

## Coverage summary

| area | total | tagged | divergence | missing |
|------|-------|--------|------------|---------|
| api | 256 | 256 | 0 | 0 |
| decision | 105 | 105 | 0 | 0 |
| measurement-core | 173 | 173 | 0 | 0 |
| measurement-io | 144 | 144 | 0 | 0 |
| proxy | 66 | 66 | 0 | 0 |
| store | 92 | 92 | 0 | 0 |
| TOTAL | 836 | 836 | 0 | 0 |

Every `api` and `decision` row has a Rust test. With the Package 2b closeout (bead 3.14),
every manifest row in every area now has one.

## Fixes applied in this closeout

- **20 middleware and envelope rows had no fixture-driven test.** The unit tests in
  `src/lib.rs` checked the same behavior, but they were untagged and did not read the
  fixtures. The new `tests/middleware_fixtures.rs` runs every case of
  `parity/fixtures/api/apierrors.json` and `middleware.json` against the real middleware:
  - the envelope writer and the code list: Go's 14 codes in order, plus the two Rust
    privacy codes of spec Appendix B;
  - the not-found handler and the request id;
  - the recoverer: the envelope on a panic, and a started response is never rewritten;
  - public and internal auth, the rate limiter, and the rate-limit headers;
  - the body limit.
  - The Go body-limit cases use a 16-byte cap. The Rust cap is the spec's fixed 2 MiB, so
    the same three boundaries run at 2 MiB.
- **The 8 `auth.*` verifier rows** (reference Part 4 "auth") are in the same file:
  - malformed headers never reach the store;
  - on real Postgres, `PgAuthBackend` answers unknown, revoked, suspended, and active keys;
  - a store failure is a 500, not a 401 or 403;
  - the identity and the internal actor are absent until the middleware sets them.
- **16 `server_test.go` rows** assert only on their recorded responses. The black-box
  replay compares those exactly (status, envelope bytes, the rate-limit, `Retry-After`,
  and request-id headers under the fixed clock). `tests/replay.rs` now carries their tags,
  and `api.TestMain`'s: that row is the suite harness, one migrated Postgres, which the
  replay builds.

## The checklist

1. **The black-box replay has zero unexplained diffs** (bead 5.15).
   - `tests/replay.rs` replays 617 recorded exchanges and 1,498 seed statements against one
     database. 507 are byte-exact, 110 are explained, and none is unexplained.
   - `parity/blackbox/replay-allowlist.json` maps every explained part to its DIVERGENCES
     row: D8/D9/D14/D15/D29/D39 recommend members (50), D39 (21), D1/D15/D29 (14), D16 (10),
     D2 (6), D36 (5), D34 (3), D35 (3), and D40 (1).
   - `TestPhase2ExitEndToEndRecommend` builds its own database and runs the jobs system, so
     it is not replayed. `crates/maverick/tests/e2e.rs` (bead 5.16) covers it.
   - The replay runs in `cargo test --workspace` and in the CI parity tier.
2. **Every `api.json` row (256) and every API-facing `decision.json` row has a Rust test.**
   The coverage checker reports 256 of 256 and 105 of 105.
3. **The contract matrix is complete** (bead 5.17). `tests/contract.rs` fails when an
   (endpoint, status, code) pair has no named test. `tests/CONTRACT.md` renders the matrix.
   - The pairs come from the recording, from spec Appendix A and B, and from reference
     Part 5 §5.2.
   - The shared middleware codes (401, 403, 413, 429, and 500 when the store fails) are
     tested on every route.
4. **Owner decisions are recorded for the parity questions this package raised.**
   - **MCP validation wording (Q7):** D34 (owner decision 2026-09-28). The shape, the
     `validating "arguments": ` prefix, and the named property are the contract. The
     validator's own wording is not. `tests/mcp.rs` asserts the named property, and the
     replay masks the rest under D34.
   - **Health key naming under D22:** D35 (owner decision 2026-09-28). The component is
     `job_queue`, read from graphile_worker, and has no `paused` status.
   - **The `completed_unchecked` outcome filter:** D40 (owner decision 2026-09-29, bead
     `model-maverick-5xn`). The filter's accepted values and its 422 message gain the D16
     outcome. The replay rule for that 422 names D40.
   - **Other parity questions found during the replay:** none needed a new owner decision.
     - Every difference matched an existing row.
     - The replay adapts the Go seeds for D18 (`capture_key_version` 1), because
       production starts on a fresh database (D37 note). `parity/blackbox/README.md` §8
       records it.
5. **Gates are green.** See below.

## Gates

- `cargo fmt --all --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `cargo test --workspace`: pass (1,369 tests, 0 failed).
- `cargo deny check`: advisories, bans, licenses, and sources ok.
- `scripts/sqlx-prepare.sh --check`: pass.
- `scripts/check-instrumentation.sh`: pass.
