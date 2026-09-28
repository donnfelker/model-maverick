# AGENTS.md: Model Maverick (Rust)

Read this file before you do work in this directory. Then read `HANDOFF.md`.

## The project

Model Maverick is a B2B recommendation engine for LLMs. It benchmarks models against a private task corpus every night. It classifies each workload, applies tenant policy, and returns a ranked list of models (a fallback chain). The engine does not execute inference. The only component that executes inference is the trial proxy, and it uses only the caller's credentials.

## Language and stack

- **All source code in this project is Rust.** Use the current stable toolchain and the 2024 edition. Do not write Go. Do not look for the old Go source. It is not available, and you do not need it.
- The Cargo workspace is at the root of this directory. Put crates in `crates/`. `plans/master-plan.md` §4 and §6 name the required crates: `maverick-core`, `maverick-proxy`, and `sdk-conformance`. You decide how to divide the engine crates.
- Stack (from `plans/master-plan.md` §6): Tokio, axum and tower, sqlx 0.9 (compile-time checked queries, offline mode in CI), `graphile_worker` 0.13.x for jobs, reqwest with rustls (no OpenSSL), hand-written provider clients (Anthropic, OpenAI, Google; xAI through the OpenAI client), the `jsonschema` crate (draft 2020-12), serde, `rmcp` for MCP, `tracing` with JSON output, `tracing-opentelemetry` and `opentelemetry-otlp`, RustCrypto (`hmac`, `sha2`, `hkdf`, `aes-gcm`, `subtle`), `ulid`, testcontainers for store tests, and `insta` for snapshot fixtures.
- Other components use other languages. They are not in this directory: the Rails shell (v1.1, see `docs/agency-product-framework.md`) and the customer SDKs (TypeScript, Ruby, Go, Python).

## Order of authority

If two documents disagree, use this order:

1. `specs/`: the normative contracts. `specs/model-maverick-v1-spec.md` is the engine contract.
2. `parity/DIVERGENCES.md`: the approved differences from the old Go behavior.
3. `reference/engine-reference.md`: the exact behavior (strings, constants, order, quirks) for all points that the specs do not fix.
4. `plans/master-plan.md`: the design summary, the decisions, and the build order. It does not override the specs.

Some specs and plans still name Go tools (chi, pgx, sqlc, goose, River, `cmd/`, `internal/`). For these names, use the Rust stack above. A path such as `internal/proxy/keys.go` identifies where a behavior came from. It is not a file that you must find.

## Parity rules

- A work package is complete only when its Rust tests pass the parity data in `parity/` for its area. Code that compiles is not sufficient.
- Each Go test in `parity/manifest/*.json` must have a Rust test that replaces it, or a `DIVERGENCES.md` row that explains why not.
- A `QUIRK:` line in the reference is Go behavior. Do the same thing in Rust, unless `DIVERGENCES.md` lists it.
- Do not add a new intended difference yourself. Ask the owner. After approval, add a row to `DIVERGENCES.md` and update the spec.
- Do not compare live benchmark runs for equality. Model output changes between identical runs.

## Rules that you must not break

These come from the guarantees G1 to G14 in `specs/model-maverick-v1-spec.md`.

- **Privacy.** Do not store a prompt, except in an encrypted debug-capture window. Do not put a prompt, a completion, a request body, or a credential in a span, a log line, or a metric label. Every `#[tracing::instrument]` uses `skip_all` with an allowlist of fields.
- **Zero retention.** For a zero-retention tenant, the engine never sends a prompt or a pick to a column or an alias without valid zero-retention evidence. This rule fails closed. It has priority over G1 and G14.
- **The corpus is private.** Task prompts, golden answers, and rubrics must never go into this directory, into a public artifact, or into a benchmarked model's prompt (except the task prompt).
- **No real provider calls in CI.** Use the deterministic fake provider. The one live smoke test is opt-in and has a spend cap.
- **Spend is capped (G8).** A batch projects its cost first and refuses to run if the projection is more than the cap.
- **Pools are hard boundaries (G2).** A request can make a pool smaller. It can never make a pool larger.
- **No schema translation.** The engine and the proxy do not translate between provider wire formats.
- **No `POST /v1/outcomes`.** Outcome data comes only from MM components (proxy, SDKs) through `/v1/telemetry`.
- **No Redis.** Postgres holds all state, including the job queue.
- **No MM-owned provider keys for customer traffic.** Operator keys are only for benchmarks and classification.

## Tests and quality gates

- Put golden fixtures in the test data and calculate the expected values by hand, or take them from `parity/`.
- Store tests and job tests use real Postgres (testcontainers).
- CI gates: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, `cargo deny`, and `cargo-mutants` on the domain crates.
- Each endpoint has a contract test for each error code.

## Git

- Use plain `git`. Stage files by name. Do not use `git add -A`. Do not use `--no-verify`.
- Make one commit for each unit of work, with a clear message. End the message with the co-author trailer that your harness gives you.
- Work directly on `main`. Do not create branches, worktrees, or pull requests. When a unit of work is done and its tests pass, commit it, run `git pull --rebase origin main`, and push to `origin main`. Never force-push.

## When to stop and ask the owner

- An open decision in `plans/master-plan.md` §10 blocks your work.
- A parity diff does not match a row in `DIVERGENCES.md`, and you think that the Go behavior is wrong.
- A task needs a real provider key, the deploy host, the private corpus, or money.

## Writing rules for documents

- Use short, specific sentences. Use "is" and "has".
- Do not use em dashes or en dashes in the middle of a sentence.
- Do not use these words: delve, robust, seamless, leverage, crucial, pivotal, comprehensive, ecosystem, landscape.

<!-- br-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`/`bd`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View ready issues (open, unblocked, not deferred)
br ready              # or: bd ready

# List and search
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br search "keyword"   # Full-text search

# Create and update
br create --title="..." --description="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason="Completed"
br close <id1> <id2>  # Close multiple issues at once

# Sync with git
br sync --flush-only  # Export DB to JSONL
br sync --status      # Check sync status
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Always run `br sync --flush-only` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only open, unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers 0-4, not words)
- **Types**: task, bug, feature, epic, chore, docs, question
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads changes to JSONL
git commit -m "..."     # Commit everything
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always sync before ending session

<!-- end-br-agent-instructions -->
