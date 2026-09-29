# Model Maverick: Master Plan

Status: consolidated 2026-08-30 from every plan and spec in this repo.
Purpose: the one file that explains what Model Maverick is, how the whole system works end to end, and in what order to build it. It describes the target system as a plan to build from zero: design decisions, mechanics, sequencing, open questions. It does not track progress. Everything else in `plans/` and `specs/` is a drill-down from here (map in §12).

Precedence when documents disagree: `specs/model-maverick-v1-spec.md` is the contract for the engine's behavior; this file summarizes and never overrides it.

Implementation: the engine, the trial proxy, and the conformance harness are built in Rust (decided by the owner). An earlier Go engine implemented most of v1. Its exact behavior and its 675 tests were extracted on 2026-09-28 into `reference/engine-reference.md` and `parity/`, so the Rust work is a port with a complete behavioral reference. The Go source is not available to this project and nothing here depends on it: no Go binary, image, or runtime is used at any step. Behavior, guarantees, API, and schema are unchanged except where this file or `parity/DIVERGENCES.md` names a change. Where a spec or plan names a Go package, tool, or path (chi, pgx, sqlc, goose, River, `cmd/`, `internal/`), the Rust stack in §6 replaces it. Customer-facing SDK languages (§3.5) are a separate decision and stay as they are.

---

## 1. The product in one paragraph

Model Maverick answers one question: given this workload, these constraints, and the current state of every model tracked, which model should run it? It benchmarks models continuously against a private task corpus, classifies incoming workloads, applies tenant policy, and returns a ranked list. It does not execute inference. Applications take the answer and make their own call through whatever they already use (direct provider APIs, Vertex, Bedrock, LiteLLM, OpenRouter, a self-hosted endpoint). The pitch to a developer: applications should never hardcode model names. The pitch to a buyer: benchmark-grounded recommendations with measured difficulty ceilings, tenant policy as a hard boundary, an audit trail from score to decision, and no inference lock-in.

Model Maverick complements gateways (OpenRouter, LiteLLM, Bedrock, Vertex) rather than competing with them: they execute, it decides. Evaluation tools (Braintrust, Langfuse, LM Evaluation Harness) measure; Model Maverick turns measurement into a real-time recommendation layer.

**Market as of 2026-09** (sources: `tmp/ground-truth/market-brief.md`; refreshed quarterly by the owner, next 2026-12):

| Neighbor | What it does now | Overlap with MM |
|---|---|---|
| OpenRouter Auto (Stripe acquiring, announced 2026-08-19) | Classifies prompts into ~30 task types, ranks by trailing 7-day community spend, allow/deny patterns, 5 cost tiers, free | Classification, pools, cost bands, failover |
| Azure Foundry Model Router | Cross-provider (OpenAI, Anthropic, xAI, DeepSeek, Meta), Balanced/Cost/Quality modes, customer subsets, failover | Modes and pools, bundled with Azure |
| Ramp Router (2026-08-20, free through 2026) | Picks models by up to three user-specified benchmarks | Benchmark-driven selection, bundled with spend management |
| Not Diamond | Narrowed to coding-agent routing (2026-08-04) | Coding workloads only |
| Thesean Ship (Martian spin-out, 2026-07-21) | Executes inference | Savings pitch |
| Vals AI, LMArena, Artificial Analysis | Independent and custom benchmarking sold to enterprises | Evidence, not per-request decisions |
| Braintrust | Model selection from the customer's own experiments | Customer-specific evidence |

Martian and Unify no longer sell routers; Portkey, Helicone, Katanemo, and Langfuse were acquired. Generic auto-routing is now free and bundled, and research shows many routers fail to beat the best single model (LLMRouterBench, Routing Plateau) while a static task-type table does beat it (arXiv 2608.23023). The wedge is therefore not "automatic routing". It is independent private measurement per capability × difficulty, provider-neutral tenant policy, a decision trace a buyer can audit, and evidence that the recommendations beat Best Single and free baselines (§7.1 M6a). Buyer-specific evidence comes from a bounded pre-sale evaluation: the buyer supplies an evaluation set, MM runs the frozen M6a comparison on it offline, and delivers an evidence report, without the v2 tenant-private corpus machinery.

---

## 2. Binding design decisions

These hold for every release.

| Decision | What it means |
|---|---|
| **No inference** | Maverick recommends; callers execute. The only MM code that executes inference is the trial proxy, and it only ever uses the caller's credentials (§3.5); MM-owned provider keys are used for benchmarking and classification, never to execute customer traffic. The ranked list is the pool-constrained fallback chain. Every capability ships a machine-readable check spec a caller can run on model output. |
| **Capability is the primary key** | Domains (medical, legal, real estate) are tags on tasks, never top-level categories. Medical, legal, and financial tags switch on a high-stakes grading overlay. |
| **The corpus is private** | Task prompts, golden answers, rubrics, and grader internals never enter a public artifact and never enter a benchmarked model's prompt beyond the task prompt. Contamination resistance is the whole point. Tenant-private corpora are v2. |
| **One central schedule** | Nightly smoke set, weekly full sweep, cron-configured globally. Per-tenant schedules arrive with tenant-private corpora. |
| **Two services, one seam** | The Rust engine owns everything the hot path needs: benchmark data, tenants as configuration, keys, policy, the recommend API, MCP. A Rails shell (v1.1) owns identity and commerce: users, orgs, sessions, billing, dashboard, marketing site. They share no database. Rails provisions tenants through internal endpoints; the recommend path never calls Rails. |
| **Every well-formed request gets a model** | Config writes are validated so no pool and constraint combination can have zero usable columns. Request-level constraints degrade to flagged preferences. Difficulty shortfalls flag rather than block. The no-model responses are a request naming a model the system does not know or the tenant may not use (loud on purpose), and the zero-retention privacy exception: `422 zero_retention_requires_declared` (prompt form sent by a zero-retention tenant with no zero-retention classifier configured) and `409 no_compliant_columns`. Privacy outranks G1. Both codes are in the spec (v1 spec §1.1 and Appendix B, adopted 2026-09-28). |
| **Zero ceremony** (decided 2026-07-14) | A customer adopts by changing one line. Outcome data reaches the engine as a side effect of MM-authored components (proxy, SDKs, plugins), never as something the customer reports by hand. There is no `POST /v1/outcomes`. |
| **URL interception is the primary integration** (decided 2026-07-15) | Base URL swap plus a `maverick/auto` alias (plus the `X-Maverick-API-Key` header on the hosted proxy, §3.5) is THE integration story; the trial proxy is sequenced before the SDKs. The raw recommend API stays available and carries no obligations, but it is not the pitch. |
| **Never write provider schema translation** | MM contributes the decision, the chain, and the telemetry. Wire translation is done by the host library (Vercel AI SDK, LiteLLM, RubyLLM, providers' own OpenAI-compatible endpoints). |
| **Rust for the engine** | One static binary, one Postgres, no Redis (a Postgres-backed job queue). The v2 on-premises build compiles from the same codebase. The trial proxy and the conformance harness are Rust binaries in the same Cargo workspace. |

### The fourteen guarantees

Cross-cutting invariants referenced as G1 to G14 throughout every document.

| # | Guarantee | Owner |
|---|---|---|
| G1 | Always a model. Flags, never empty responses. Cold start, classifier outage, and missing data degrade honestly. Exception: zero-retention privacy errors (§2). | Recommendation, classification |
| G2 | Pools are hard boundaries. A request can narrow but never widen. | Policy engine |
| G3 | Loud caller bugs. Unknown or out-of-pool allowlist entries are hard errors naming the entry. | Policy engine |
| G4 | No empty configurations. Writes and retirements are simulated against every pool before commit. | Policy engine, catalog |
| G5 | Prompts are never stored by default. Fingerprints only, except inside an explicit encrypted capture window. | Privacy |
| G6 | Every decision is auditable from stored data alone. | History |
| G7 | The corpus is private. | Capability matrix, grading |
| G8 | Spend is capped. Every batch projects cost first and refuses past the ceiling. | Jobs |
| G9 | One-way dependency, two databases. Rails calls the engine; the engine never calls Rails. (v1.1) | Shell |
| G10 | Predicted never masquerades as measured. Matrix completion never sets `measured: true`. (v1.1) | Calibration |
| G11 | Outcome blending is tenant-local and bounded. Global aggregates are never touched. (v1.1) | Scoring |
| G12 | Benchmark code executes sandboxed or not at all. (v1.1) | Grading |
| G13 | Zero ceremony. No public surface asks the caller to report back. Telemetry is opt-out in every component and off means zero bytes. | Integration surfaces |
| G14 | Fail-open. An MM component failure never breaks the customer's LLM call: last-known-good chain → static default → the original model string. The trial proxy is exempt by positioning. Privacy outranks G14: for a zero-retention tenant, fallback may use only cached entries stamped `retention_mode: zero`, and the static default or original model only if locally configured as verified zero-retention; otherwise the call fails closed. | SDKs, plugins |

---

## 3. How the system works

Four layers. Each consumes the one above it.

```
MEASUREMENT     model catalog ─┐
                corpus ─────────▶ runner ──▶ graders ──▶ scoring ──▶ aggregates ──▶ calibration
                                                                         │
DECISION        classification ──▶ policy engine ──▶ ranking ◀───────────┘
                                                        │
ACCOUNTABILITY  prompt hash, decision trace, debug capture, alerts ◀──────┘
                                                        │
PLATFORM        tenants + keys   jobs (Postgres) internal API (the seam)   MCP   proxy
```

### 3.1 The measurement loop (how the numbers come to exist)

**The grid.** Ten capabilities × five difficulty tiers × N model columns. A *column* is the precise unit: (provider, model, endpoint, settings). Thinking on and thinking off are two columns. Each cell holds quality, pass rate, ambiguity, latency p50/p95, and cost, all EWMA-smoothed, dated, and stamped with `matrix_version`.

Capabilities (v1): `code_generation`, `structured_extraction`, `summarization_synthesis`, `multi_step_reasoning`, `instruction_schema_following`, `long_context_qa`, `factual_qa`, `rewriting_editing`, `translation_multilingual`, `critique_and_judging`. A capability earns a slot only when model rankings on it would plausibly diverge from rankings on every existing one. Difficulty tiers come from structural knobs authored per capability (for extraction: document noise, field count, nesting depth, distractor content).

**The corpus.** 20 to 25 authored tasks per capability, 200 to 250 total, in a private repository (`modelmaverick-corpus`). Each task is data: prompt, grader configuration, dimension weights, pass threshold, domain tags, tier. Five tasks per capability are smoke members with an easy anchor (tier ≤ 2) and a hard anchor (tier ≥ 4). The corpus loads under a date-stamped version after validation (schema, weights sum to 1, grader specs, smoke constraints) and activation flips atomically; the system stays live throughout. Authoring is the largest single work item in v1 and system accuracy depends on it.

**The catalog.** Columns are data, not code: created and edited through the internal API, never seeded in migrations. Lifecycle `cold_start → active → deprecated → retired`. A cold-start column runs the full corpus nightly for a dense-run window (default 5 nights) and its cells report unmeasured until they have enough nights. Secondary-endpoint columns (the Vertex variant, say) borrow quality cells from their primary column but keep their own latency (from hourly probes) and cost; quality is a property of the model, latency and price are properties of the path to it. Each column snapshots list price per million tokens; run cost is computed from the snapshot at execution time and never restated. Before any column retires, the engine simulates every tenant pool containing it; a pool that would drop to zero blocks the transition and raises a critical `pool_would_be_empty` alert (G4).

**The runner.** A batch is one scheduled execution: nightly smoke (smoke tasks × non-retired columns) or weekly sweep (full corpus × non-retired columns), three repeats per pair. Cold-start columns run the full corpus even on smoke nights. Worker pools per provider bounded by concurrency and requests per minute; repeats run sequentially (they measure variance, not throughput). Prompts are assembled byte-identically across columns. Rate-limit and server errors retry three times with backoff and jitter; exhausted retries record a provider error and are never rescheduled. Four launch providers: Anthropic, OpenAI, Google, xAI (xAI through the OpenAI-compatible API), roughly 10 to 14 columns.

**Graders.** Every task carries its graders as data. Five kinds: JSON schema (extract, validate draft 2020-12, per-field checks), rule scan (ordered include/exclude/length/language rules, a failed `critical` rule zeroes), golden answer (normalized, numeric tolerance, set, regex), key facts (weighted coverage plus `forbidden_facts` as hallucination traps), and judge rubric. A test-suite kind is schema-reserved and rejected in v1: no model-generated code is ever executed. The judge protocol: a pool of two cross-family columns (seed `gemini-3.1-pro`, `gpt-5.5`); the judge is the first pool member whose family differs from the graded column's; two structured-output calls at temperature zero with rubric order reversed; the mean is the score and the disagreement feeds the ambiguity penalty. Judges see reference material; benchmarked models never do. A grader error marks dimensions absent, never zero, and a batch with more than 10% judge failures alerts. `critique_and_judging` scores double as empirical judge selection: the operator re-points the judge pool at whatever columns judge best.

**Scoring.** Run quality is the task's weighted sum of graded dimensions, renormalized over present dimensions. Truncated or refused outputs are graded on whatever text exists. Provider errors and timeouts are excluded from quality (quality measures successful outputs) and counted instead in a per-cell `availability` aggregate (pass rate, and therefore ceilings, keeps the Go rule: provider errors and timeouts stay in the denominator as failures) over every scheduled repeat, including timeouts and provider errors; latency aggregates include timeouts as censored at the timeout value. This resolves the spec's contradiction (errors "score zero" yet are excluded from the mean) and is adopted in the spec (v1 spec §6.1, 2026-09-28). The high-stakes overlay (medical, legal, financial tags) reweights to accuracy 0.40, calibration 0.20, hedging 0.15, citation honesty 0.25 and force-fails a run on a `critical` rule or an `autofail` fact. Task score = max(0, mean − 0.5σ) over repeats with error runs excluded from the mean. Ambiguity: when two graders score the same dimension on the same output, their spread subtracts at quarter weight and aggregates per cell as its own dimension. A batch's cell observation is the mean of its task scores; cells fold into an EWMA with α = 0.3 plus Welford variance; a cell is `measured` after 3 distinct nights. Aggregates are append-only. Displayed quality is the same 0 to 100 scale everywhere; ranking normalization is internal and never shown.

**Ceilings.** For a column × capability at a tenant's quality bar, the ceiling is the highest tier such that every tier at or below it holds pass rate ≥ bar + buffer (defaults 0.80 + 0.05). Ceilings power the banded policy table and the per-entry `clears_difficulty` flag.

**Regression detection.** A cell flags when its smoothed value drops more than 3 points below its trailing 7-day high, or a raw observation shocks more than 2× the cell's own noise below the smoothed value. Critical when the drop crosses a quality bar of a tenant actually pooling the column; deduped per cell per week, re-armed if the drop deepens 2 points. A parse-failure watch alerts when a column's batch parse-failure rate exceeds 5% and 2× its trailing baseline (the leading indicator of caller-side silent escalation).

**Calibration.** Before each nightly batch, `prediction_lock` snapshots the EWMA-implied pass probability for every cell scheduled to run. After rebuild, `calibration_score` computes a Brier score and overconfidence per capability against those locked predictions. Locking before results makes hindsight impossible; the series is exposed to tenants and is the evidence gate for matrix completion in v1.1. Predictions record their source method so later predictors (matrix completion, IRT) compete on the same ledger.

**The nightly timeline (UTC).**

| Time | Job | What it does |
|---|---|---|
| 02:50 | `prediction_lock` | Lock tonight's predictions |
| 03:00 | `nightly_smoke` | Smoke tasks × columns × 3 (cold-start columns run everything) |
| Sun 04:00 | `weekly_sweep` | Full corpus × columns × 3; weekly cap only |
| chained | `aggregate_rebuild` → `regression_scan` + `calibration_score` | Fold EWMAs, flag drops, score predictions |
| hourly | `latency_probes` | Thin pings for secondary-endpoint columns; latency cells only |
| 05:00 | `history_prune` | Enforce retention (default 365 days) |
| 06:00 | `price_sync` | Refresh pricing snapshots from the configured price source (§10) |
| 06:10 | `catalog_sync` | Lifecycle transitions, guarded retirement, pool-drift check |
| hourly | `capture_expiry` | Hard-delete expired debug captures (default 72 h) |

Every job is unique per (job, batch window) so restarts collapse into one execution. The engine owns that uniqueness, not the queue: `graphile_worker` deletes completed jobs, so its `job_key` (§6) cannot stop a re-enqueue after a run from starting a second execution (River's `UniqueOpts{ByArgs}` in the Go engine covered running and completed jobs; this is `parity/DIVERGENCES.md` row D10). Each handler's first transaction acquires a lease on a `batches` row (`UNIQUE(kind, window)`, deterministic `batch_id`, `status`, `lease_owner`, `lease_expires_at`, `heartbeat_at`, `attempt`, `completed_at`): insert it if absent, or take it over if its lease has expired. `lease_owner` is the `graphile_worker` worker ID. A completed row makes the handler a no-op. A row whose lease is live and held by a different worker that is still alive makes the handler return `Complete` as a no-op (a `graphile_worker` handler can only complete or fail, and failing would burn `max_attempts` into a permanent failure); nothing is stranded, because the holder's own job stays locked and `graphile_worker` recovers it if the holder dies. A row whose owner is gone (Graphile recovered the job from that worker) is taken over immediately, even before `lease_expires_at`; a stale running row is likewise taken over and the batch resumes under the same `batch_id` from its existing `runs`. The holder renews the lease on a heartbeat well inside the expiry. Startup rejects any configuration where the batch lease TTL is not shorter than `graphile_worker`'s earliest recovery threshold. Jobs are enqueued with `job_key` = `<kind>:<window>` as a first filter; `max_attempts` is set per kind (batch 3, chained and hourly 5), never the library default of 25. Delivery is at-least-once, so every handler is idempotent: a batch resumes by skipping (column, task, repeat) triples that already have a `runs` row, and the rebuild is keyed by batch. Batches chain: a completed batch enqueues the rebuild, which enqueues the scan and the calibration score. A partially failed batch still rebuilds from what completed, records its completion percentage, and raises a `job_failure` alert below 90%. Batch jobs need a long timeout (hours, configurable, sanity-capped); chained and hourly jobs a short one; each handler runs under an explicit per-kind `tokio::time::timeout`. The failure tail (alert row plus the follow-up rebuild job) is written in the same Postgres transaction that records the batch outcome, before the handler returns, so process death or a deploy mid-tail cannot lose it; there are no detached tasks carrying durable work. Worker recovery (heartbeat, stale-lock sweep after a crash) comes from `graphile_worker`'s recovery configuration. Acceptance: `kill -9` of the worker mid-batch (including a case where queue recovery fires while the application lease is still live), a restart, an enqueue while the batch is running, an enqueue after it completed, and a handler timeout each end with no permanently failed jobs left behind, exactly one completed batch, one rebuild, no duplicate persisted runs or aggregate contributions, and the correct alerts. Duplicate external provider calls remain possible when a crash lands between a successful call and its `runs` commit; they are bounded by the spend guard's per-batch accounting.

**Classifier budget (G8).** The prompt path puts an MM-paid classifier call on every cache miss, and the cache is exact-match, so unique prompts nearly always miss. Each tenant gets a daily classification quota (v1: an engine-config default plus an operator override on the internal seam; from M8, written by entitlement sync), and a global daily classifier spend cap applies; past either limit the request degrades to `fallback_default` with a `classification_quota_exceeded` flag instead of spending. M6 measures classifier cost per tenant; success criterion: classifier cost under 10% of plan price at P95 usage. Timeouts are aligned: the classifier gets a 1.5 s budget so a cache miss answers inside the SDK's 2 s recommend timeout. Docs recommend the declared form for production code paths.

**The spend guard (G8).** Before running, every batch projects cost as ceil(chars/4) + 600 output tokens per run at current pricing. Over the cap (nightly $100, weekly $500 by default) the batch refuses to run and raises a critical `spend_refusal` carrying the projection; at 80% it runs and warns. Actual spend is recorded per batch so projections are auditable. Planning estimate to validate in M0/M1: the smoke set is about 50 tasks × 12 columns × 3 repeats, near 1,800 runs a night, plausibly $25 to $80 depending on token mix; the weekly sweep runs 4 to 5 times that.

### 3.2 The decision path (how a request becomes an answer)

```
request ──▶ workload resolution ──▶ policy engine ──▶ ranking ──▶ trace ──▶ response
             declared | prompt        5 steps           mode bands   persisted   pick + chain + flags
```

**Workload resolution.** Exactly one of two forms. *Declared*: `capability` plus optional `difficulty` (defaults to tier 3, labeled as defaulted) and `domain`; no model call, nothing leaves the engine, full confidence. *Prompt*: the raw text is fingerprinted (per-tenant HMAC-SHA256 over exact bytes), the classification cache is checked under (tenant, fingerprint, classifier version), and on a miss a small classifier column (seed `claude-haiku-4-5/direct`, temperature zero; for a zero-retention tenant the engine uses the global `classifier.zero_retention_column` (a column whose `retention_mode` is `zero`, validated under G4), and if none is configured the prompt form is refused with `422 zero_retention_requires_declared` and the prompt never leaves the engine; structured output enforced by a forced tool call) returns capability, difficulty, domain tags, and confidence. The workload prompt is fenced as data with a standing instruction never to answer or execute it; the fence and the schema are versioned into the cache key. One total deadline of 1.5 s covering the call, one retry, and backoff, bounded further by the caller's remaining deadline when propagated (this replaced the spec's 5 s + retry, adopted 2026-09-28); on failure the request degrades to `multi_step_reasoning`/3/confidence 0/`fallback_default` and sustained failure raises a critical `classifier_degraded` alert. Oversize prompts classify on head 80k plus tail 20k with capped confidence. High-stakes derives from medical, legal, or financial tags. The authored corpus doubles as labeled training data for a future trained classifier.

**Classifier accuracy is measured, not assumed.** An eval harness scores the classifier against human-adjudicated labels (never one model's opinion) on a balanced capability × tier grid with the known edge classes (ambiguous, adversarial, high-stakes, non-English, length extremes). Capability is scored strictly; difficulty is scored exact, within ±1 tier, and by rank correlation, against a written tier rubric with anchor examples.

**The policy engine.** Fixed order, every exclusion recorded with a closed-enum reason:

1. **Pool restriction (G2).** The named pool, else the tenant's default. Hard boundary.
2. **Allowlist (G3).** Entries resolve against the system catalog first. Unknown → hard error naming it. Known but outside the pool → a different hard error naming it.
3. **Standing constraints.** The tenant's stored cost and latency ceilings filter hard against the cell's aggregates, and the zero-retention flag filters hard against each column's `retention_mode` (exclusion reason `standing_constraint:zero_retention`). Safe only because writes are validated. If aggregate drift has emptied the set by request time, cost and latency violators are retained as least-violating, flagged, and a critical alert fires: G1 outranks those two filters. Zero retention is a privacy constraint and fails closed: it is never relaxed to satisfy G1. G4 write validation, retirement, and the nightly drift check simulate it like any other constraint, so an empty zero-retention set is prevented at write time; if a column's retention status changes and empties it anyway, the request returns `409 no_compliant_columns` and a critical alert fires. (The Go engine stores the flag but only enforces it for debug capture; the Rust port adds this enforcement, with tests showing neither classification nor recommendation sends a zero-retention tenant's prompt or pick to an unverified endpoint.)
4. **Request preferences.** Per-request cost or latency ceilings partition candidates; violators rank after satisfiers everywhere and never drop. If the pick violates one, `constraints_met: false` with the violations named.
5. **Rank.**

**Write-time validation (G4).** Pool and constraint writes are simulated together: every pool as it would exist after the write, under the constraints as they would exist, against current aggregates, must keep at least one usable column. Otherwise the write is rejected naming the pool and the constraint. The same simulation backs the retirement guard and runs nightly as a drift check. The internal API applies the same validation as the tenant-facing endpoints, so no caller can create a configuration a tenant could not.

**Ranking.** Columns below an availability floor (default 0.9 over the trailing 7 nights) rank after all columns above it, flagged `low_availability`. Min-max normalize quality, cost, and latency over the survivors (computed per query, never stored). Three modes set default weights: quality (band 0 points), balanced (band 1.5), cost (band 5.5); tenants can override weights per capability. Candidates within the band of the best quality are leaders, ordered by weighted value; everything below appends in quality order so the tail is a sane escalation chain. Ties inside the 0.75-point noise band prefer the cheaper column with a bias toward same-family downgrades (a cross-family swap changes failure modes and prompt behavior). Family = provider. Index 0 is the pick; the array is the fallback chain, pool-constrained by construction. The recommended unit is (model, settings): every chain entry returns its column's `settings` (reasoning effort, thinking mode), because effort and adaptive thinking now drive much of the intra-provider cost and quality spread (market brief §2: "Opus 5.5 at `medium` matches or exceeds Opus 5 at `high`"). The proxy and SDK cores apply those settings by default and record `settings_applied` in telemetry; callers opt out with `maverick/<pool>:<mode>?settings=caller`. Effort tiers become separate columns only for models where the corpus or M6a shows they change rankings, and the column count and spend estimate in §3.1 are re-estimated on that basis at M6a.

**Flags, not refusals (G1).** `clears_difficulty` per entry, `constraints_met`, `measured`. A fully cold pool returns 200 with its best candidates, null scores, and `measured: false`. Every response carries `recommendation_id`, `matrix_version`, `prompt_hash` (prompt form), a `check_spec` reference per capability, `execution_aliases` per entry, and a human-readable reason per entry (reasons explain, flags are contractual).

**Latency target.** The declared path reads precomputed aggregates and must answer in under 50 ms at p95. The uncached prompt path must answer under 1.8 s at p95 (inside the SDK's 2 s timeout), and the proxy's added time to first byte is measured separately; M6 gates on all three.

**Banded policy** (`GET /v1/policies`): per capability, the cheapest column whose ceiling clears each difficulty band at the tenant's bar, with `clears_band: false` when nothing does.

### 3.3 Tenancy and the seam

```
Global benchmark  +  Tenant configuration  =  Tenant recommendation
```

The benchmark is shared. A tenant is configuration, not people: a per-tenant HMAC key generated at creation, named pools (`default`, `vertex-only`, `hipaa-bedrock`), per-capability weights and quality bar, standing constraints (max cost, max latency, zero retention, enforced against each column's `retention_mode` = `unknown | standard | zero` with `retention_verified_at`, `retention_valid_until` (at most 90 days after verification), and a source link; `unknown` never satisfies zero retention, and expired evidence reads as `unknown`. `catalog_sync` alerts 14 days before any expiry; the owner revalidates quarterly; a test shows expiry yields `409 no_compliant_columns` rather than stale compliance), a status, and a key ring. API keys are `mk_live_` + base62(32 bytes), shown once, stored as SHA-256 with last4. Suspension turns a tenant's keys off with a distinct error without touching its data. Rate limiting is fixed-window per key with standard headers.

Two listeners, two trust models. The public listener (`:8080`) speaks tenant keys and is the only surface the deployment proxy exposes: `/up`, `/v1/*`, `/mcp`. The internal listener (`:8081`) speaks operator tokens (a comma-separated list of `label:secret` entries; the presented bearer is the full entry; constant-time compare; label recorded as acting party) and is reachable only on the private network: `/health/detail`, `/internal/v1/*`.

The internal API is **the Rails seam**, built before Rails exists: create/suspend/activate tenants (a new tenant arrives usable with a default pool of all active columns), issue/revoke keys, write pools/weights/constraints with the same G4 validation as the tenant-facing endpoints, read the alert feed, administer catalog columns (retirement guard enforced), activate corpus versions, read debug captures. Every write emits an audit log entry naming the actor. Until Rails exists, the human operator uses the same seam by hand.

### 3.4 Accountability (how decisions stay defensible)

Built for one support conversation: "your system keeps returning a small model where a large one was deserved."

- **The decision trace.** Every recommendation persists the classification, the request (prompt text excluded), the response as returned, and every column considered: survivors with the numbers that ranked them, exclusions with a closed-enum reason (not in pool, not in allowlist, which standing constraint, retired, no data). Opus is either a ranked survivor with the numbers that placed it second or an exclusion with the reason it never competed.
- **The prompt hash.** One-way, tenant-keyed, exact-match. The audit log can answer how often an exact prompt arrived and how it was classified each time; the same hash keys the classifier cache.
- **Debug capture.** A tenant opens a time-boxed window (optionally scoped to one key, optionally sampled to low-confidence classifications). Matching prompts are encrypted with a per-capture key (HKDF from a master key that never touches the database, AES-256-GCM with a fresh 96-bit CSPRNG nonce per capture) and hard-deleted at expiry. Each capture stores `capture_key_version`; the engine holds a key ring (current plus previous), encrypts only with the current key, decrypts by stored version, and drops a retired key once the longest capture window plus retention has passed, so a master-key rotation neither strands live captures nor keeps a compromised key indefinitely. Replay re-runs classification in memory and returns whether it matches, never the text. A tenant self-service read and an audited operator read exist for when replay is not enough. Every touch lands in an access log that outlives the capture. Zero-retention tenants are refused at window creation and at the request-time capture decision.
- **The durable fix.** When traces show one code path repeatedly misclassified, that path switches to the declared form and skips the classifier permanently.
- **Alerts.** Every alert is a database row first (closed kinds: regression, parse-failure rate, job failure, spend warning, spend refusal, pool-would-be-empty, classifier degraded), then fanned out asynchronously to SMTP and an HMAC-signed operator webhook (`X-Maverick-Signature`), retried independently, never blocking the write. Both job writers and request-time writers deliver through one process-wide fan-out drained at shutdown.
- **Retention.** Recommendations, traces, and outcomes prune daily past the tenant's retention. Aggregates and calibration are kept indefinitely; they are the product.

### 3.5 Doors into the engine

The strategy is two wrapper families for most app code, gateway plugins for platform teams, and OpenAI-compatible HTTP as the universal fallback. MCP for agents. The raw API for everyone else.

| Door | Milestone | Who | What they change |
|---|---|---|---|
| **Trial proxy** (`crates/maverick-proxy`) | Track 3, sequenced first | Evaluators; Anthropic-native tools such as Claude Code | `base_url` + `model: maverick/auto` |
| **MCP** (`/mcp` streamable HTTP, `mcp-stdio`) | v1 M5 | Agents, Claude Code sessions | Tool call: `recommend_model`, `get_policy`, `list_models` |
| **REST** (`POST /v1/recommend`) | v1 M2 | Anyone hand-rolling dispatch | Their code, their choice; no obligations |
| **TypeScript SDK** (`@modelmaverick/ai-sdk` meta-provider, `@modelmaverick/openai`) | N3 | TS apps on Vercel AI SDK or the OpenAI SDK | `model: maverick('auto')` or wrap the client |
| **Ruby SDK** (gem `modelmaverick`, RubyLLM meta-layer) | N4 | Rails apps on RubyLLM | Initializer; or zero-gem: `openai_api_base` → proxy |
| **Go SDK** (`http.RoundTripper`) | N5 | Go apps | Wrap the HTTP client |
| **Python SDK** (LiteLLM router strategy, `modelmaverick[openai]`) | N6 | Python apps | Router config or custom httpx client |
| **LiteLLM proxy plugin** | N6 | Platform teams with a gateway | A few lines of `config.yaml`; app devs change nothing |
| **Portkey** | only on customer request (acquired by Palo Alto Networks) | Platform teams | Plugin or a config generator |
| **Any other language** | Via proxy or LiteLLM plugin | Java, PHP, C#, … | `base_url` + alias |

**The alias grammar**, parsed identically everywhere: `maverick/auto` (= balanced), `maverick/quality`, `maverick/cost`, `maverick/<pool>:<mode>`. A concrete model name where an alias is expected is rejected by the proxy and passed through untouched by an SDK ("don't route, just call it").

**The shared SDK core** (specified once, implemented four times): recommend client (declared and prompt forms, 2 s timeout); chain executor (advance on 429/5xx/timeout/connect error, respect `Retry-After`, never on semantic 4xx, exhausted chain surfaces the last provider error); fail-open ladder (G14); telemetry batcher (10 s / 20 items / exit flush, fire-and-forget, `MODELMAVERICK_TELEMETRY=off` means zero bytes); privacy modes `declared | prompt | off`; execution modes `direct | openrouter`. `422 zero_retention_requires_declared` and `409 no_compliant_columns` are terminal privacy errors, not fail-open triggers: the core purges its last-known-good chain for that tenant and surfaces the error. Recommend responses carry `zero_retention: true` for such tenants, which switches the core to declared mode, and every chain entry carries its `retention_mode`. The core persists the tenant's zero-retention state in memory, and during an engine outage its fail-open ladder applies the G14 privacy rule (cached entries stamped `zero` only; static default or original model only if locally marked verified zero-retention; else fail closed). Conformance scenarios cover both codes plus a cold-cache and a warm-cache engine outage for a zero-retention tenant. Uniform config names: `MODELMAVERICK_API_KEY`, `_BASE_URL`, `_TELEMETRY`, `_DEFAULT_MODEL`, `_PRIVACY`, `_EXECUTION`. Provider keys stay in each provider SDK's standard location. The core never does schema translation, keeps no disk state, and runs no background threads beyond the batcher. The proxy's own copy of the core is a Rust crate (`crates/maverick-core`) in the engine workspace. A conformance harness (`crates/sdk-conformance`, a mock engine plus mock provider with twelve frozen scenarios) keeps four languages honest.

**The trial proxy.** A separate small binary with no database and its own deployment, so the engine stays control-plane pure. It speaks OpenAI chat-completions (`POST /v1/chat/completions`) and Anthropic Messages (`POST /v1/messages`) so Anthropic-native tooling works through `ANTHROPIC_BASE_URL` unmodified. Per request: parse the alias, project the request to a workload, ask the engine, walk the chain. **Workload projection** (one versioned definition shared by the proxy and every SDK core): system/developer instructions, bounded recent conversation text (not only the last user message), response-format schema, and tool names and descriptions, excluding binary content, truncated per §3.2. The projection version is part of the classification cache key. Callers can override with a declared workload header (`X-Maverick-Workload: <capability>[:<tier>]`), which is the recommended form for production routes. This replaces the Go proxy's last-user-text-only rule (`internal/proxy/workload.go`), a recorded divergence; M6 gates on multi-turn and tool-use classification fixtures. A real chain executor: fallback happens only before the first byte reaches the caller; a 2xx is relayed or streamed the moment it is seen; a semantic 4xx is relayed verbatim; an inexecutable chain fails closed with `502 no_executable_candidates`. Tenant authentication: the proxy is one shared hosted instance. `Authorization` (or `x-api-key` on the Messages surface) is reserved for the caller's provider credential, and the tenant's Maverick key travels in `X-Maverick-API-Key`; rate limits and telemetry are per tenant key, with isolation tests showing one tenant's key never recommends from another's pools. Onboarding is therefore a base-URL swap plus one header (two env vars for Claude Code: `ANTHROPIC_BASE_URL` and a custom header setting), not base-URL alone; the Go proxy's single configured `MAVERICK_API_KEY` remains the self-hosted form. Credentials come only from the caller. Two modes, fixed per deployment: **hosted** (MM operates the instance) accepts either a caller OpenRouter key (every entry executes through OpenRouter) or a surface-native credential, in which case the chain is restricted to that provider's entries and other entries are skipped; the hosted build reads no provider env keys at all, so a cross-provider pick cannot fall back to operator secrets. **Self-hosted** (the customer runs the binary) may also use the customer's own per-provider env keys (standard names, never `MAVERICK_`-prefixed). This differs from the Go proxy (`internal/proxy/keys.go`), which falls back to env keys in any deployment; the port changes it, with a test that a hosted instance given only an OpenAI key never calls Anthropic. Keys live in memory for the request and are never stored or logged. Headers and query strings pass through except hop-by-hop and credentials. The recommended column's `settings` are applied unless the caller opted out (§3.2); caller-supplied model-dependent values (reasoning effort tiers, for one) are normalized to the nearest value the routed model accepts; catalog-driven per-model constraints are the durable form. Automatic telemetry per attempt. Optional OTel logs and metrics (a recommendations counter by `measured` and `classification_method`) so cold-cell rate and classifier fallback rate are graphable, fail-open. Fences: no schema translation (the Messages surface executes Anthropic entries only), direct execution by default, tighter rate limits than the API, no production SLA. (A "graduation link" header pointing to the production path is a nice-to-have, not planned; see integration-surfaces plan §4.) A launcher script runs Claude Code through it.

**Telemetry** (`POST /v1/telemetry`). Batch of up to 500 items: `recommendation_id`, `model_used`, `status`, optional `latency_ms`, `tokens_in`, `tokens_out`, `finish_reason`, `error_class`, `check_pass`, `endpoint_used`. Best-effort by contract: malformed or foreign items are dropped and counted, never 4xx the batch, no cross-tenant existence oracle. Outcomes are derived server-side (timeout → `latency_exceeded`; connect/provider error, 429, 5xx → `provider_error`; 2xx with `check_pass: false` → `parse_failure`; 2xx with `check_pass: true` → `success`; 2xx with no check → `completed_unchecked`; else `other`). `check_pass` is three-valued (true, false, absent): only true or false is quality evidence; `completed_unchecked` counts toward availability only and never enters outcome blending (M12) into the `outcomes` table; `/v1/history/outcomes` exposes them. Documented only in SDK and plugin internals; it is plumbing, not customer API surface. Components stamp `X-Maverick-Recommendation-Id` and `X-Maverick-Model-Used` on responses they touched.

### 3.6 Execution backends

The engine recommends from measurements on the column's own endpoint; the customer may execute elsewhere. Columns carry an `execution_aliases` map (one key defined at v0: `openrouter`, value the OpenRouter slug; future aggregators use the same mechanism) exposed on `GET /v1/models` and on every recommend entry so an SDK needs zero extra calls. `openrouter` is a legal `endpoint` value for future measured-via-OpenRouter columns. In `execution: openrouter` mode the chain executor sends every entry to one upstream with the entry's alias as the model slug and stamps `endpoint_used: openrouter`; alias-less entries are skipped; an alias-less chain is `no_executable_candidates` (a catalog gap, not fail-open). One customer key replaces N provider keys. The fidelity gap (direct-measured, OpenRouter-executed) is made observable through `endpoint_used`; the structural fix is measured `endpoint: openrouter` columns under the v1.1 per-endpoint promotion rule. MM never holds OpenRouter keys and never proxies production traffic. Retention is a property of the execution path: each `execution_aliases` entry carries its own `retention_mode` with the same `retention_valid_until` expiry rule as columns. For a zero-retention tenant, openrouter-mode executors (SDK core and hosted proxy) send OpenRouter's per-request Zero Data Retention option on every attempt and skip any alias not verified `zero`; if nothing remains the result is `409 no_compliant_columns` (fail closed), never a direct-key fallback. Conformance and proxy scenarios cover a zero-retention tenant with an OpenRouter key.

---

## 4. Topology and repositories

```
          www / app.modelmaverick.dev                 api.modelmaverick.dev
                     │                                        │
           ┌─────────▼──────────┐                   ┌─────────▼──────────────┐
           │   Rails shell      │   private net     │   maverick serve       │
           │  marketing + SPA   │──────────────────▶│  public :8080          │
           │  billing (Stripe)  │  seam + read key  │  internal :8081        │
           │  Solid Queue       │◀──────────────────│  job workers           │
           └─────────┬──────────┘  tenant webhook   └─────────┬──────────────┘
                     │                                        │
           ┌─────────▼──────────┐                   ┌─────────▼──────────┐   ┌──────────────┐
           │  Postgres (shell)  │                   │ Postgres (engine)  │   │ provider APIs│
           └────────────────────┘                   │ 22 tables + jobs   │   │ anthropic    │
                                                    └────────────────────┘   │ openai google│
                                                                             │ xai          │
   customer app ──base URL──▶ maverick-proxy ──recommend/telemetry──▶ engine └──────▲───────┘
                              (no database)   └────────── executes chain ───────────┘

   customer app + SDK ──recommend/telemetry──▶ engine ; executes via its own provider SDKs
```

One engine binary, three shapes: `serve` (everything), `serve --api-only` (stateless replica), `serve --worker-only` (exactly one). Subcommands: `migrate`, `corpus load | activate | status | calibrate`, `mcp-stdio --api-key`, `demo-run`. Postgres is the entire state, including the job queue. The shell arrives in v1.1; the SDKs in v1.2.

| Repository | Contents |
|---|---|
| `modelmaverick` | Cargo workspace: engine, `maverick-core`, trial proxy, conformance harness; plus examples, specs, plans, docs |
| `modelmaverick-corpus` (private) | Task YAML per capability, authoring notes |
| `modelmaverick-web` | Rails shell (v1.1), cloned from the agency product framework template |
| `modelmaverick-sdk-ts`, `-rb`, `-go`, `-py` | The four SDKs (one repo per language, N3 to N6) |
| `modelmaverick-litellm` | LiteLLM proxy plugin, shares the Python core |

Deployment: multi-stage Docker (cargo-chef for cached dependency layers) building a static musl binary into a distroless static image, rustls only (no OpenSSL), Kamal 2 to a dedicated Hetzner host, Postgres as an accessory with a named volume, internal listener never proxied, migrations in a pre-boot hook, `/up` as the health check. At v1.1 the shell deploys beside the engine on the same box with `web` and `worker` roles; the Kamal proxy routes `www`/`app` to Rails and `api` to the engine; nightly off-box backups for both databases with a scheduled restore test.

---

## 5. Data model

Twenty-two tables in the engine (including `batches` and `classifier_usage`, new in the Rust port), grouped:

| Group | Tables |
|---|---|
| Tenancy | `tenants`, `api_keys`, `pools`, `tenant_weights`, `tenant_constraints` |
| Catalog and corpus | `model_columns` (with `execution_aliases`), `tasks`, `corpus_versions` |
| Measurement | `runs`, `scores`, `aggregates` (append-only, `as_of`), `predictions`, `calibration` |
| Decisions | `recommendations` (full candidate trace), `classification_cache`, `classifier_usage` (classifier budget counters; new in the Rust port), `outcomes` (with `endpoint_used`) |
| Privacy | `capture_windows`, `debug_captures`, `capture_access_log` |
| Operations | `alerts`, `batches` (lease and status per job kind × window; new in the Rust port, adopted into the spec 2026-09-28) |

Raw values live in `scores` and `aggregates`; normalized scores are computed per query and never stored. Every ID is a prefix plus ULID (`rec_`, `bat_`, `out_`, …) minted through one helper. The Rails shell keeps its own databases (the framework's primary plus `queue`/`cache`/`cable` logical databases; primary holds users, sessions, `Account`/`AccountUser`/`AccountInvitation`, engine links, provisioning outbox, plans, Pay tables, key metadata, alert copies); nothing in the engine references it. v1.1 adds, all additive: key scopes, tenant limit overrides, `alert_tenants`, `tenant_webhooks`, `irt_fits`/`irt_abilities`/`irt_task_params`, `tenant_column_adjustments`, `task_type`/`interaction` on tasks, `transcript` on runs, three more providers in the check constraint.

---

## 6. Tech stack

Engine: Rust (current stable, 2024 edition), Tokio, axum + tower (middleware for auth, rate limits, request IDs), sqlx (compile-time checked queries from SQL, offline mode in CI) with `sqlx migrate`, `graphile_worker` 0.13.x (Postgres job queue on sqlx 0.9 + rustls: `job_key` as a pre-run dedupe filter with per-window uniqueness enforced by the engine's `batches` claim (§3.1), explicit per-kind `max_attempts`, retries with backoff, cron, graceful shutdown, worker heartbeats and stale-lock recovery; per-kind timeouts are the handler's own `tokio::time::timeout`; small project, so pin the version and own a fork if it stalls), reqwest + rustls, hand-written typed provider clients for Anthropic, OpenAI, and Google (none ships an official Rust SDK; xAI through the OpenAI client with a base URL), the `jsonschema` crate (draft 2020-12), serde for JSON and a maintained serde YAML crate, `rmcp` (the official MCP Rust SDK), `tracing` with JSON output, RustCrypto (`hmac`, `sha2`, `hkdf`, `aes-gcm`, `subtle` for constant-time compares), `ulid`, `tracing-opentelemetry` + `opentelemetry-otlp` in the engine and the proxy, exporting to the framework's ops-box OTel Collector over the private network (available from v1, independent of the shell) (no public metrics endpoint), with W3C trace context propagated proxy/SDK → engine → provider call; metrics: recommend and classifier latency histograms, cache hit and classifier fallback rate, provider status and retries, queue depth and oldest-job age, job duration/failure/recovery, spend per batch; testcontainers-rs for store tests. Tagged-union shapes (grader specs, policy rules, workloads, closed-enum exclusion reasons) are serde enums, so an unknown variant fails at deserialization and a missing match arm fails at compile time; the versioned JSON schemas stay at the boundary for callers. **Telemetry redaction is a G5 control.** Every `#[tracing::instrument]` uses `skip_all` with an explicit allowlist of fields (a CI lint rejects bare `#[instrument]`, which records every argument via Debug); prompt text, completions, request bodies, and credential headers (`Authorization`, `x-api-key`, `X-Maverick-API-Key`) are never span attributes, log fields, or metric labels; fingerprints only. Engine and proxy spans go to a Collector pipeline that does not export GenAI content to Langfuse. Acceptance: a prompt-form recommend and a proxy request carrying a canary string, after which the canary appears in no exported span, log line, or metric label. v1.1 statistics (IRT fits, soft-impute) go to ndarray + nalgebra with `argmin` for the optimizer, with a Python sidecar as the fallback for that one batch step.

Shell (v1.1, per the agency product framework, which overrides any conflicting detail in `specs/model-maverick-v1.1-spec.md`; that spec's organizations/memberships and rswag text are updated before M7 is assigned): Rails 8, Rails 8 auth generator (not Devise), Vite React + TypeScript SPA served same-origin (not Hotwire), Tailwind + shadcn/ui from a tokens file, a hand-authored spec-first OpenAPI contract with committee/skooma conformance tests in Minitest and openapi-typescript/orval client generation (inherited from the tagged framework template with its prewired `bin/ci`, not re-chosen), Pay + Stripe, Solid Queue/Cache/Cable, Kamal 2. No JWT in the browser, no Redis, no separately deployed frontend.

Testing conventions: golden fixtures carry the bulk (graders, scoring, ranking) with hand-computed expected values checked in; store and job tests run against real Postgres; golden fixtures use `insta` snapshots where the expected value is a document rather than a number; no real provider is ever called in CI (a deterministic fake provider is the only "model"); corpus-dependent exit tests gate on a corpus checkout; one opt-in, spend-capped live smoke test (two tasks × two columns, shape and sanity assertions only); every endpoint has a contract test per error code; QA mutation testing (`cargo-mutants`) on every unit before commit; `clippy -D warnings` and `cargo deny` gate CI.

---

## 7. Release map

Each release is scoped by deliverables, never dates.

```
v1  Engine (M0 to M6)
 │
 ├── v1.2  Integration surfaces (N1 to N7) + OpenRouter execution backend
 │
 ├── v1.1  Product shell + measurement upgrades
 │        Rails track M7 to M10 ∥ engine track M11 to M13
 │
 └── v2   Enterprise and on-premises
```

### 7.0 The Rust port program (agent team)

The behavioral reference is `reference/engine-reference.md` plus the test data in `parity/`. Every Rust milestone is done when the Rust engine passes the parity data for its area, not when it compiles. No step runs or needs the Go engine.

0. **Ratify the contract.** Done 2026-09-28: every spec change from §10 is in `specs/`, and the intended differences from the Go behavior are listed in `parity/DIVERGENCES.md`.
1. **Freeze the reference.** Done 2026-09-28: `reference/engine-reference.md` (exact behavior, prompts, formulas, constants, wire formats, quirks, SQL query catalog) and `parity/` (fixtures for all 675 Go tests, 619 recorded HTTP exchanges with their 1,498 seed SQL statements, 54 proxy scenarios, the schema, and a manifest that maps every Go test to its replacement). Where a `DIVERGENCES.md` row applies, the Rust test asserts the Rust expected behavior, not the recorded Go value.
2. **Port order**, each step a work package an agent can own, with parallel tracks where there is no dependency: (a) pure domain crates (ids, grader kinds, scoring, EWMA/Welford, ranking, policy, ceilings, alias grammar, capture crypto), gated by `parity/fixtures/`; (b) the schema and store layer: one sqlx baseline migration equal to `parity/schema/schema.sql`, gated by store tests on real Postgres and by a normalized `pg_dump --schema-only` comparison against that file. If the owner keeps the existing Go-era database (it holds one real benchmark), a one-time `maverick migrate adopt-goose-baseline` command verifies the live schema fingerprint and transactionally inserts the exact `_sqlx_migrations` row so sqlx's checksum validation passes; this needs only the schema, not Go; (c) provider clients plus the fake provider, gated by the recorded exchanges in `parity/fixtures/measurement-io/providers_wire.json`; (d) HTTP API, MCP, internal seam, and the proxy, gated by a **replay test**: apply `parity/blackbox/api-seed-sql.jsonl` and send `parity/blackbox/api-recorded.jsonl` in order against one database, comparing each response field by field with the masking rules in `parity/blackbox/README.md`; the proxy replays `parity/fixtures/proxy/scenarios.json` against a mock engine and mock providers; (e) jobs on `graphile_worker`, gated by the scoring and fold fixtures in `parity/fixtures/measurement-core/` (bit-exact under float32 storage), the job acceptance tests in §3.1, and the given/when/then cases in `reference/engine-reference.md` Part 3. Live nightlies are never compared for equality, since run-to-run nondeterminism is real (5.37% of model-question pairs flip across identical runs, arXiv 2608.23023).
3. **Parity bar per step.** All ported fixtures green, the replay at zero unexplained diffs, `clippy -D warnings`, `cargo-mutants` on the domain crates. An agent may not mark a package done with a diff it has not either fixed or matched to a `DIVERGENCES.md` row. A new intended difference needs the owner's approval and a new row.
4. **Go live.** The engine has no external tenants, so Rust replaces Go directly: there is no shadow period and no rollback to Go. With a kept database: take a backup, stop the Go processes, run `adopt-goose-baseline`, seed `batches` with completed rows for every (kind, window) that already has `runs`, start Rust. Rollback is restoring that backup and fixing forward in Rust.

Success criterion for the port as a whole: every manifest entry in `parity/manifest/` has a passing Rust test (or an approved divergence), the API and proxy replays have zero unexplained diffs, and the first live Rust nightly completes under the spend cap. If a Go-era database is kept, that nightly also passes pre-registered distribution checks against the stored Go-era aggregates: mean paired delta per provider and per capability within one pooled standard error, at most 5% of cells outside a band of max(2σ, 2 points) (variance floored at 2 points), no regression alert attributable to the port, and spend within 10% of the projection.

### 7.1 v1: the engine (M0 to M6)

The Go engine implemented most of M0 to M5; that behavior arrives in Rust through §7.0: M0, M1 (code), M2, M3, and M5 are ported via packages (a) to (e); zero-retention enforcement, hosted-proxy keys, the classifier budget, and the `batches` claim are new in Rust; corpus authoring (M1, M4), M6a, and M6 are remaining non-port work. The list below keeps the milestone definitions as the reference for what each package must reproduce.

- **M0 Skeleton:** Cargo workspace (Tokio, axum, sqlx, the job queue crate), schema and migrations, tenant auth, the four provider clients plus the fake, a runner that executes one task against one column and stores a run.
- **M1 First capability end to end:** author `structured_extraction` (fully objective graders), grader framework and the four deterministic/reference graders, corpus loader, scoring pipeline, nightly smoke job, EWMA, first aggregates, spend guard. Extraction first because it is the most cross-cutting capability.
- **M2 Recommend API:** tenant config with G4 validation, policy engine, ranking, ceilings and banded policy, `POST /v1/recommend` (declared path) with the decision trace, `GET /v1/models`.
- **M3 Classification:** prompt hash, classifier client with cache and fallback, prompt-path wiring.
- **M4 Corpus buildout:** the cross-family judge, the high-stakes overlay, the remaining nine capabilities (objective-graded first, judge-dependent later; authoring runs in parallel with M2 and M3 once the loader exists), the remaining jobs (sweep, probes, price and catalog sync, retirement guard), the tier calibration report.
- **M5 Product surface:** history endpoints, regression scan and alert delivery, prediction tracking, debug capture, MCP server, the internal provisioning API, `/v1/telemetry` and the alias grammar (N1), the trial proxy (Track 3), docs, deployment.
- **M6a Recommendation-value validation (before any external tenant):** preregister held-out, customer-shaped workloads (at least 3 workloads from design partners or the owner's own products, prompts never seen by the corpus authors) and compare MM's pick against Best Single, the cheapest model meeting the quality bar, a static task-type table built from public benchmarks, and OpenRouter Auto, plus Azure or Ramp where accessible. Report quality, cost, latency, routing regret, lone-expert recall, confidence intervals, and repeat-run sensitivity. Grading protocol, fixed before the run: each workload ships with a buyer- or owner-written rubric or reference answers; blinded human scoring against the rubric is the primary M6a outcome (a solo operator plus design-partner reviewers; at least 2 raters per scored item on the high-stakes and disputed subset). A cross-family judge panel with rubric-order swap supplies secondary scores for the full set, and its agreement with humans is reported as ICC and mean absolute error on the numeric scores plus rank correlation per workload; judge scores count only if ICC ≥ 0.7. No panel is required to exclude every picked family, since picks can span all four providers; Best Single is chosen on a separate calibration split, never on the test prompts. Inputs: operator gates cleared (§8 step 0), authored capabilities covering the M6a workloads, and at least 3 measured nights. Launch thresholds, fixed before the run: (1) the lower confidence bound shows no more than 1 point quality loss against Best Single with at least 20% lower cost on the target workloads, and no high-stakes regression beyond the 2-point margin; (2) for each free baseline (the public-benchmark static table and OpenRouter Auto), the paired contrast baseline_regret − MM_regret has a lower confidence bound above zero on the same workloads. Pre-registered: one-sided α = 0.05, a paired bootstrap resampling prompts within workloads, at least 150 prompts per workload with 3 repeats, and a high-stakes non-inferiority margin of 2 quality points. **Commercial gate (M6b)** (suspended 2026-09-28 by the owner: not required while the goal is a working system that may be open-sourced; see §10), originally before the N3 onward SDK work and the M7 shell: at least 3 external design partners supply workloads, review their evidence report, and at least 2 convert to paid pilots or price-specific LOIs at the planned flat price. If this fails, the pricing or ICP changes before more surface is built. **Buyer data runbook** for the pre-sale evaluation: NDA and DPA first; only approved or ZDR provider endpoints; an encrypted per-buyer workspace outside the corpus repository; no buyer data ever committed to the corpus; access logged; fixed retention (30 days) and a deletion attestation; the canary no-log test (§6) runs on the workspace. If MM fails either technical threshold, the pitch changes (evidence and audit sold without the routing-savings claim) before any external sale. This runs beside the port and does not wait for it.
- **M6 Dogfood and harden:** tenant #1 is you; route one of your own products through the proxy, then through the API; hot-path performance check against the §3.2 latency targets (declared p95, uncached prompt p95, proxy-added TTFB); the opt-in live smoke harness; a hardening loop until a full week produces no new entries.

### 7.2 v1.2: integration surfaces (N1 to N7)

A standalone initiative; only N1 touches v1.1 (its §7 and §18).

| # | Milestone | Contents | Depends on |
|---|---|---|---|
| N1 | Engine prerequisites | `/v1/telemetry`, no outcomes ceremony, alias grammar, correlation headers, `execution_aliases`, `endpoint_used` | none |
| N2 | Core spec + conformance harness | Shared core frozen as a spec doc; `crates/sdk-conformance` mock server + twelve scenarios | N1 |
| N3 | TypeScript SDK | core + Vercel AI SDK meta-provider + OpenAI wrapper | N2 |
| N4 | Ruby SDK | core + RubyLLM meta-layer + Rails initializer | N2 |
| N5 | Go SDK + trial proxy | Go core as `RoundTripper` (standalone; no longer shared with the proxy); the proxy already runs on `maverick-core` (built with it in M5) and must pass the same conformance scenarios; decide the OpenRouter-backed proxy variant | N2 |
| N6 | Python SDK + LiteLLM proxy plugin | core + LiteLLM strategy + OpenAI wrapper; the Track 2 plugin from the same strategy | N2 |
| N7 | Fast-follows | Anthropic wrappers, langchaingo, docs pass (Portkey only on customer request) | N3 to N6 |

N3 to N6 are independent after N2. Recommended order: TS (market size) → Ruby (agency clients on Rails + RubyLLM) → Go (Go app market; it no longer shares code with the proxy, so it can slip behind Python if needed) → Python (least painful interim story). The 60-second acceptance test for the whole track: install one package, one config touch, `maverick/auto` where a model name used to go, outcomes flow without the customer knowing the concept exists.

### 7.3 v1.1: product shell and measurement (M7 to M13)

Two tracks in parallel; M8 to M10 need their engine additions landed first within each milestone.

**Rails track.**
- **M7 Shell skeleton:** `modelmaverick-web` from the framework template, Rails 8 auth, the framework's `Account`/`AccountUser`/`AccountInvitation` with `Current.account` and Pundit (one engine tenant per `Account`), seam client, outbox, provisioning happy path, Kamal deploy with the job-execution smoke passing.
- **M8 Billing and lifecycle:** engine adds per-tenant limits, tenant listing, acting-party audit; Pay/Stripe plans (Trial 14 days, Pro, Scale; flat, no metering), entitlement sync (`PUT /internal/v1/tenants/{id}/limits`), the suspension state machine, daily reconciliation.
- **M9 Dashboard:** engine adds API key scopes (`read` keys 403 on every write and on recommend); SPA pages over the generated client for recommendations browser with full trace view, policy table, models, score trends, calibration, outcomes, config editing via the seam, key management. First customer-visible artifact.
- **M10 Alerts everywhere:** engine adds `alert_tenants`, `GET /v1/alerts`, tenant webhooks (max 3, HMAC-signed, `webhook_failing` operator alert); Rails registers itself as each account's webhook, renders alerts to dashboard and email, `/admin`, marketing site. **v1.1 launchable here.**

**Engine measurement track.**
- **M11 Calibration:** IRT (2PL per capability, health-gated by holdout log-loss, pass-rate as permanent fallback, `ceiling_source` on policies); three-judge panels for high-stakes tasks (median scoring, 2-of-3 autofail).
- **M12 Prediction and feedback:** matrix completion (soft-impute rank 3, shipped disabled, skip a nightly cell only when its own Brier record earns it, every cell runs at least every third night, the sweep never skips, G10); outcome blending (telemetry-derived from checked outcomes only, enabled only once check coverage and false-positive rate pass gates set at M12; shrinkage-first, ±2 quality points max, tenant-local, G11, opt-out per tenant).
- **M13 Surface:** providers DeepSeek, Groq, Together via one OpenAI-compatible client; per-endpoint quality promotion (`POST /internal/v1/columns/{id}/promote-quality`); the multi-turn harness with simulated tools, injected failures, adversarial tool results, and a `trajectory` grader; `test_suite` activation behind `sandboxd` (ephemeral container, no network, G12); capabilities `classification_routing` (makes classifier selection empirical) and `tool_use_orchestration` authored in the corpus. Spend caps rise to $150/$750.

### 7.4 v2: enterprise and on-premises

Tenant-private corpora with per-tenant schedules; an air-gapped single static binary with signed license files and local benchmark execution; self-hosted providers (Ollama, vLLM, LM Studio, TGI, NIM) with quality measured globally on reference endpoints and latency/cost measured on customer hardware; capabilities `vision`, `conversational_persona`, `long_form_writing`; Maverick Gateway as an optional companion that executes inference from recommendations and check specs (customers keep their own gateways if they prefer). v2 is vision-level and has no spec yet.

---

## 8. Recommended build order

0. **Operator gates first**: provider terms-of-service review, provider keys for all four launch providers (the judge pool needs two families), the deploy host, one production nightly under the cap. M6a depends on them.
0a. **The Rust port (§7.0)** (M6a runs beside it; the M6b commercial gate is suspended by the owner as of 2026-09-28, see §10), then new work in Rust only; M6a validation runs on the Rust engine as soon as the §7.0 gates for packages (a) to (e) pass and its inputs exist (§7.1).
1. **Remaining v1 work after the port** (corpus authoring, M6a, M6 dogfood; see the mapping in §7.1), with two changes to the original milestone list: fold the N1 engine prerequisites (telemetry, alias grammar, execution aliases) into M5, and build the trial proxy in M5 so that M6 dogfooding uses the base-URL door first. The proxy is the demo and the first real integration; it needs only the recommend API, the catalog, and telemetry.
2. (Operator gates moved to step 0.)
3. **N2 conformance harness, then N3 TypeScript SDK**: the Vercel AI SDK meta-provider is the single highest-value artifact in the plans, and the harness is what keeps the other three languages honest.
4. **N4 Ruby SDK**: agency clients are Rails + RubyLLM and have a zero-gem interim path through the proxy.
5. **M7 to M10, the Rails shell**: the seam is designed in v1 so this is an addition, not surgery; launchable at M10.
6. **N5 Go SDK and N6 Python + LiteLLM plugin**; decide the OpenRouter-backed proxy variant at N5.
7. **M11 to M13 engine measurement upgrades**, with matrix completion enabled only after a watched Brier baseline.

Cut lines if squeezed: the tier calibration report, the live smoke harness, the Portkey spike, the Ruby ActiveJob adapter, and the OpenRouter proxy variant can all slip without violating a guarantee. Nothing else in the v1 list can.

---

## 9. Operator work outside the code

- Provider terms-of-service review for automated evaluation before the first real benchmark on any provider. Tenant-facing scores are a different exposure than published rankings; there is no public leaderboard.
- Trademark and domain check on the name before anything public.
- Provisioning the Hetzner host, secrets, and the provider keys; confirming the internal port is unreachable publicly.
- Monthly judge calibration against operator ratings; re-pointing the judge pool and the classifier column from `critique_and_judging` and (v1.1) `classification_routing` cells, as logged manual operations.
- Dogfooding: tenant #1 is you, for at least two consecutive weeks before any external tenant.

---

## 10. Open decisions

Consolidated from every plan. Decided items are kept for the record with their dates.

| Decision | Options | Recommendation | State |
|---|---|---|---|
| Outcomes as a customer obligation | `POST /v1/outcomes` vs automatic telemetry | automatic | Decided 2026-07-14 |
| Primary integration | recommend API vs URL interception | interception | Decided 2026-07-15 |
| SDK language order | TS first vs Python first | TS, Ruby, Go, Python | Decided 2026-07-14 |
| Proxy surfaces | OpenAI-only vs also Anthropic Messages | both | Decided 2026-07-15 |
| Shell auth and frontend | Devise + Hotwire (plan) vs Rails 8 auth + Vite React (framework) | framework wins | Decided 2026-07-13 |
| Trial proxy keys | BYOK pass-through vs MM-owned capped trial keys | caller credentials only; hosted mode reads no provider env keys (§3.5) | Decided in this plan |
| Trial proxy hosting | MM-operated instance plus binary vs binary only | hosted plus binary | Decided 2026-09-28 (owner): hosted plus binary |
| Trial proxy execution | OpenRouter-backed (one BYOK key) vs per-provider direct | per-provider direct by default; OpenRouter as one opt-in backend among several (Vercel AI Gateway, Bedrock). OpenRouter is being acquired by Stripe and runs a free Auto Router that overlaps MM, so defaulting evaluators through it sends them to a competitor and adds a single-vendor dependency | Decided 2026-09-28 (owner): per-provider direct by default; OpenRouter is opt-in |
| Scope label | standalone v1.2 vs fold into v1.1 | standalone | Open |
| Telemetry naming | `/v1/telemetry` vs `/v1/observations`; rename history/outcomes? | keep `/v1/telemetry`, history unchanged | Open |
| SDK repo layout | one repo per language vs monorepo | per language | Open |
| Rust core home | `maverick-core` crate in the engine workspace vs its own repo | in the engine workspace; the proxy depends on it by path | Open |
| Publish a Rust SDK | keep `maverick-core` internal vs publish it on crates.io as a fifth SDK (reqwest middleware / tower layer) | keep internal until a customer asks; publishing is nearly free once it passes conformance | Open |
| Job queue | in-house Postgres queue vs `apalis` (1.0 still RC, no stable Postgres backend) vs `graphile_worker` | `graphile_worker` for delivery, retries, cron, heartbeats and crash recovery (built on sqlx 0.9); per-window uniqueness is an engine-owned `batches` claim because `job_key` does not cover running or completed jobs | Decided in this plan |
| N3 to N6 parallel vs serial | | serial keeps review sane | Open |
| Ruby ActiveJob adapter at v0 | in-thread batcher only vs ship the adapter | in-thread only | Open |
| SDK version floors | Node 20 / Ruby 3.2 / Go 1.22 / Python 3.10 | confirm against client fleets | Open |
| Alias shape | generic `execution_aliases` map vs single `openrouter_slug` column | generic map | Open |
| Alias population | operator-only vs a sync job against OpenRouter's list | operator-only at v0 | Open |
| Price source for `price_sync` | none (operator edits via the column PUT) vs a models.dev-backed source | build the source | Decided 2026-09-28 (owner): build the models.dev source |
| Proposed spec changes from this plan | workload projection and declared-workload header, three-valued `check_pass` and `completed_unchecked`, `retention_valid_until`, per-tenant classification quota field on tenant limits, (model, settings) chain entries, per-alias `retention_mode`, zero-retention codes and G1/G14 exceptions, `retention_mode`, classifier budget and 1.5 s deadline, availability aggregate and error-scoring fix, hosted-proxy key and tenant-header rules, `batches` table; v1.1 spec shell section aligned to the framework | adopt into `specs/` as §7.0 step 0, a hard gate before port work packages are assigned | Adopted 2026-09-28 |
| Classifier hosting | provider column vs a trained classifier from the corpus | provider column until volume justifies training | Open |
| Commercial gate (M6b) and business model | M6b as a hard gate before N3+ SDKs and the M7 shell vs no commercial gate for now | owner: not a gate now; the goal is a working system, and the project may be open-sourced | Decided 2026-09-28 (owner): M6b is not required; revisit only if the project is sold commercially |

---

## 11. Risks

- **Corpus authoring is the critical path.** 200 to 250 tasks with tight graders is the largest single effort in v1. Mitigation: extraction first, system live from M1, capabilities added while it runs. The v1.1 multi-turn harness is the next largest item; sequence it last.
- **Judge cost and bias**: contained by deterministic graders first, one cross-family order-swapped judge at temperature zero, reference anchoring, empirical judge selection, monthly calibration; panels only for high-stakes tasks.
- **Classifier accuracy** bounds the prompt path. Confidence is surfaced so callers can require declared workloads on high-stakes routes; the eval set must use adjudicated labels and a written difficulty rubric, or it measures agreement rather than accuracy.
- **Cold start**: a new column runs dense for 5 nights and is `measured: false` until 3 nights of data; recommendable only when nothing measured survives.
- **Measurement fidelity across execution backends**: direct-measured, OpenRouter-executed. Observable via `endpoint_used`; fixed structurally by measured OpenRouter columns under per-endpoint promotion.
- **Competitive adjacency (high)**: routing is free and bundled (OpenRouter Auto with Stripe behind it, Azure Model Router, Ramp Router), and Vals AI sells custom per-customer benchmarks. MM survives only if M6a shows measurable value over Best Single and a static table, and if the audit and policy story is what buyers pay for. Owner reviews §1's table quarterly.
- **Provider terms of service** and the **name clearing trademark** are operator gates (§9).
- **OpenRouter/Stripe dependency**: terms or Auto Router bundling can change after the acquisition closes. Mitigation: `execution_aliases` stays multi-backend and direct execution is the default.
- **Buyer trust**: security and privacy is the top gateway adoption barrier (44%, Kong 2025 in the brief), competitors sell SOC 2, EU residency, and self-hosting, and 2026's exits make vendor continuity a question. Mitigation at v1.1: a DPA and subprocessor list, declared-only mode marketed as "prompts never leave your process", a documented EU-hosted option, a continuity clause (self-host binary escrow); SOC 2 is a v2 gate triggered by the first deal that requires it.
- **Single operator**: alerts push (email and webhook) rather than wait for a dashboard visit; the shell adds a dead-man heartbeat and backup-freshness monitoring at v1.1.
- **Rust ecosystem gaps**: no official provider SDKs and no River equivalent, so the engine owns its provider clients and depends on a small queue crate. Mitigation: provider clients cover only what the runner and classifier call (chat, structured output, forced tool calls), tested against recorded fixtures; the queue gets its own crash and restart tests (§3.1) and is pinned, with a fork as the fallback. Compile times are managed with a split workspace and cargo-chef in Docker.
- **Job runtime**: a full sweep runs thousands of provider calls over hours, so batch job timeouts must be sized to the work and the failure tail written transactionally with the batch outcome, or a batch dies silently at the queue's default.
- **Port drift**: agents can produce Rust that is locally plausible and globally different from the Go behavior. Mitigation: the §7.0 parity fixtures and replays; no package is done with an unexplained diff.

---

## 12. Source documents

| File | What it is |
|---|---|
| `plans/model-maverick-v1-plan.md` | The product plan: decisions, taxonomy, benchmark, tenancy, policy, API sketch, tracing, data model, stack, M0 to M6, competition, risks, references |
| `plans/model-maverick-integration-surfaces-plan.md` | Zero ceremony; Tracks 1 to 3 (SDKs, gateway plugins, trial proxy); G13/G14; client coverage; N1 to N7 |
| `plans/model-maverick-sdk-v0-plan.md` | The shared SDK core, conformance harness, four SDKs, sequencing |
| `plans/model-maverick-openrouter-execution-plan.md` | OpenRouter as an execution backend |
| `reference/engine-reference.md` | Exact behavior of the earlier Go engine: algorithms, prompts, constants, wire formats, quirks, SQL query catalog |
| `parity/` | Test data for the port: fixtures, recorded HTTP exchanges and seed SQL, proxy scenarios, schema, test manifest, `DIVERGENCES.md` |
| `docs/` | Feature docs, API and operations docs, the market brief (`docs/market-brief.md`), and the owner's web-product framework (`docs/agency-product-framework.md`) |
| `specs/model-maverick-v1-spec.md` | The normative engine contract: guarantees, packages, 21-table DDL, runner, graders, scoring formulas, policy, API, tracing, jobs, MCP, seam, config, testing, endpoint and error catalogs |
| `specs/capability-matrix-design.md` | Measurement methodology: dimensions per capability, high-stakes overlay, difficulty knobs and tiers, judge bias handling, statistics, corpus authoring guide, research grounding |
| `specs/model-maverick-v1.1-spec.md` | Rails shell on the seam (framework-aligned), engine additions, IRT, completion, blending, panels, harness, sandbox, providers, promotion, M7 to M13 |
| `specs/model-maverick-openrouter-execution-spec.md` | `execution_aliases`, `endpoint_used`, SDK openrouter mode, proxy variant, conformance additions |
