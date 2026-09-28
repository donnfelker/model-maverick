# Model Maverick: Product Plan

Model Maverick answers one question: given this workload, these constraints, and the current state of every model tracked, which model should run it?

It is a recommendation engine. It benchmarks models continuously, classifies incoming workloads, applies tenant policy, and returns a ranked list. It does not execute inference. Applications take the answer and make their own inference call through whatever gateway they already use: Vertex, Bedrock, LiteLLM, OpenRouter, direct provider application programming interfaces (APIs), or a self-hosted endpoint.

Applications should never hardcode model names.

Companion document: capability-matrix-design.md holds the measurement-layer detail (grading dimensions, judge handling, difficulty methodology, research grounding). This plan is the product and build specification.

---

## Design decisions

- **No inference.** Maverick recommends; callers execute. Escalation on failed outputs happens caller-side, supported by three product features: the ranked list is the pool-constrained fallback chain, every capability ships a machine-readable check specification a gateway can run, and `POST /v1/outcomes` returns production results to the system.
- **Capability is the primary key.** Domains (real estate, medical, legal, photography) are tags on tasks, never top-level categories. Medical, legal, and financial tags switch on a high-stakes grading overlay.
- **The global benchmark is a private corpus.** Golden answers and hidden test suites never enter a prompt, which blocks contamination. Tenant-private corpora (bring your own tasks) are deferred past version 1 (v1); the v1 tenant layer is configuration, not tasks.
- **One central evaluation schedule.** Nightly smoke set, weekly full sweep, cron-configurable globally. Per-tenant schedules arrive with tenant-private corpora, where they have something to attach to.
- **Two services, one seam.** The Go engine owns everything the hot path and the on-premises binary need: benchmark data, aggregates, tenants as configuration, API keys, pools, policy, the recommend API, the Model Context Protocol (MCP) server. A Rails application, a version 1.1 (v1.1) deliverable, owns identity and commerce: users, organizations, sessions, billing, administration, the marketing site, the dashboard. The two share no database. Rails provisions engine tenants through internal administrative endpoints with service-to-service authentication, and the recommend path never calls Rails.
- **Every well-formed request gets a model.** Configuration writes are validated so a tenant cannot save a pool and constraint combination with zero usable columns; request-level constraints degrade to flagged preferences instead of filters; difficulty shortfalls flag rather than block. The only no-model response is a request naming models the system does not know or the tenant's configuration does not allow, and that error is loud on purpose because it is a caller bug to fix.

---

## Phased plan

Each phase is scoped by deliverables, never by dates. A phase ships when its contents ship.

### Version 1: the engine

The Go engine, complete and serving recommendations:

- Ten capabilities: `code_generation`, `structured_extraction`, `summarization_synthesis`, `multi_step_reasoning`, `instruction_schema_following`, `long_context_qa`, `factual_qa`, `rewriting_editing`, `translation_multilingual`, `critique_and_judging`
- Domain tags with the high-stakes overlay (medical, legal, financial)
- Five authored difficulty tiers per capability, banded policy output
- Four providers at launch: Anthropic, OpenAI, Google, xAI. Roughly 10 to 14 model columns
- Graders: deterministic and reference-based as primary; one cross-family judge for rubric dimensions
- Repeats, variance penalty, ambiguity penalty, exponentially weighted moving average (EWMA) smoothing, regression alerts
- Policy engine: named pools, request-level allowlists, three modes (quality, balanced, cost), hard constraints
- A Representational State Transfer (REST) API with tenant API keys, plus an MCP server wrapping the same endpoints
- Recommendation audit log, outcome feedback collection, global and tenant history endpoints
- Prediction tracking: locked nightly forecasts scored with a Brier metric, with calibration history retained for trend analysis
- Cost caps and projected-spend checks on every benchmark run

### Version 1.1: product shell and calibration

- The Rails application: users, organizations, sessions, billing, administration, the Hotwire dashboard, alerts rendered to email and the dashboard
- Item response theory (IRT) calibration replacing pass-rate ceilings, fitted on accumulated nightly pass data
- Matrix completion for sparse nightly runs, gated by the prediction-tracking Brier score so a cell is only predicted once the benchmark has shown it predicts that cell reliably
- Outcome blending: production feedback from `POST /v1/outcomes` weighted into tenant scores
- Judge ensembles for high-stakes tiers
- Capabilities: `classification_routing` and `tool_use_orchestration`; the latter brings the multi-turn harness with simulated tools, injected failures, and trajectory grading
- Additional providers: DeepSeek, Groq, Together
- Per-endpoint quality columns where tenant pools demand them
- Tenant webhooks for regression alerts

### Version 2: enterprise and on-premises

- Tenant-private task corpora with per-tenant evaluation schedules
- On-premises and air-gapped deployment: a single static binary compiled from the v1 Go codebase, signed license files, local benchmark execution
- Self-hosted provider support (Ollama, vLLM, LM Studio, Hugging Face Text Generation Inference, NVIDIA NIM): quality benchmarks globally on reference endpoints; latency and cost are measured on customer hardware because they exist nowhere else
- Capabilities: `vision`, `conversational_persona`, `long_form_writing`
- Maverick Gateway as an optional companion product: executes inference from Maverick recommendations and check specifications; customers keep their own gateways if they prefer

---

## Workload taxonomy

Every task and every recommendation is addressed by three coordinates:

- **Capability** (primary key): what the model must do
- **Domain tags** (filters): what the work is about; multiple allowed
- **Difficulty tier** (1 to 5): set by structural knobs authored per capability

A capability earns a slot only when model rankings on it would plausibly diverge from the rankings on every existing capability. Under that test, grounded retrieval-augmented generation (RAG) style answering is a variant inside `long_context_qa` (refusal when the answer is absent is a scored dimension there), and prompt-injection resistance is a dimension under `tool_use_orchestration` once it ships.

| Capability | Difficulty knobs |
|---|---|
| code_generation | files touched, dependency depth, spec ambiguity, adversarial tests |
| structured_extraction | document noise, field count, nesting depth, distractor content |
| summarization_synthesis | source length, redundancy, contradictions, compression ratio |
| multi_step_reasoning | step count, constraint interaction, branching, misleading framing |
| instruction_schema_following | constraint count, schema strictness, format edge cases |
| long_context_qa | context length, needle depth, hops to combine facts, contradictions |
| factual_qa | fact obscurity, entity confusability, false premises, required precision |
| rewriting_editing | constraint count, tone distance, meaning-preservation traps, format conversion |
| translation_multilingual | language-pair distance, idiom density, domain terminology, register constraints |
| critique_and_judging | defect subtlety, rubric strictness, near-correct outputs, issue count |

High-stakes overlay, switched on by medical, legal, or financial tags: factual accuracy, calibration, safe hedging, and citation honesty carry most of the grading weight, and one dangerous or fabricated answer fails the task.

---

## The global benchmark

**Grader-per-task.** Every task carries its grader as data, validated once at authoring time. Deterministic graders (test suites, schema validation, rule scans) and reference graders (golden answers, key-fact coverage) cover most of v1. A single cross-family judge (temperature 0, order-swapped) covers rubric dimensions such as code idiom and summary readability. `critique_and_judging` scores double as empirical judge selection: the judge role goes to models that measurably rank well at judging, not to reputation.

**Statistical handling.** Each task runs 3 times per model column, temperature fixed per task. Scores record mean and variance, and variance subtracts from quality, since a model that passes four times in five is riskier than its average suggests. A second penalty, ambiguity, is separate from variance: it comes from grader and judge disagreement on a single output. When the graders split on how good one answer is, that answer is harder to trust than a clean result of the same score, so the spread subtracts from quality on its own. Nightly numbers feed an EWMA, and the EWMA drives rankings, so one noisy night cannot flip a recommendation. A sharp drop in a smoothed score raises a regression alert.

**Corpus size.** 20 to 25 tasks per capability across the five tiers, 200 to 250 tasks total. Authoring is the largest single work item in v1, and system accuracy depends on it.

**Model columns.** A column is a (model, endpoint, settings) triple; thinking on versus off is a separate column. V1 measures quality through one primary endpoint per model, with list-price cost data and thin nightly latency probes for other endpoints. Quality transfers across endpoints; price and latency do not. Full per-endpoint quality columns arrive when a tenant pool demands them.

**Versioning.** Task sets are versioned. Every score row and every recommendation carries `matrix_version`, so any decision can be audited against the exact tasks and graders that produced it. Provider-side model updates make old evaluations unreproducible, so results are preserved as generated.

**Prediction tracking.** Before each nightly run, the system locks its predicted ranking (from the current EWMA in v1, from matrix completion in v1.1) and stores it. After the run, it scores that prediction against the actual results with a Brier score, the standard measure of whether confident forecasts came true and whether the unsure ones were right to be unsure. Locking the prediction before the results makes hindsight impossible. Each night's Brier score is kept, so the series over time shows whether the benchmark is getting better or worse at predicting itself, and where it is blind. This history is the go-ahead signal for matrix completion in v1.1: the system skips a real run only on cells it has already shown it can predict, and the calibration record is the evidence for that call.

---

## Tenancy

```
Global benchmark  +  Tenant configuration  =  Tenant recommendation
```

The benchmark is shared. The tenant layer holds:

- **Pools:** named sets of model columns (`default`, `vertex-only`, `hipaa-bedrock`, `budget`). Pools attach to a tenant or product and are the compliance boundary. A request cannot widen its own pool. The `hipaa-bedrock` example exists for government and healthcare clients that must stay inside Amazon Web Services (AWS).
- **Weights:** per-capability w_quality, w_cost, w_latency (summing to 1), plus a quality bar per capability that defines "passes" for ceiling purposes.
- **Modes:** quality (top score wins), balanced (cheapest within 1 to 2 points of the best), cost (cheapest within 5 to 6 points). Default balanced.
- **Constraints:** max cost per run, max latency, zero-retention required, and similar standing filters stored as metadata so filtering is automatic. Configuration writes are validated: a save that would leave a pool with zero usable columns is rejected, and a model retirement that would do the same raises an alert instead of silently shrinking the pool.

Tenant history: every recommendation and every reported outcome is stored per tenant, queryable, so an organization can see recommendation trends, policy impact, and what changed when.

---

## Policy engine

Order of operations for a request:

1. **Pool restriction.** Intersect all model columns with the caller's pool.
2. **Allowlist.** If `allowed_models` is present, validate it as a subset of the pool, then narrow to it. An entry unknown to the system is a hard error naming the entry; an entry outside the caller's pool is a hard error. No silent dropping, no silent widening.
3. **Standing constraints.** Apply the tenant's stored filters (zero-retention, cost and latency ceilings). These cannot empty the set, because configuration writes reject any combination with zero usable columns.
4. **Request preferences.** Per-request constraints (a cost or latency ceiling on this call) deprioritize rather than drop. If every surviving column violates one, the best available returns anyway, with the violations named in the response.
5. **Rank.** Normalize raw scores across the survivors and order them under the requested mode.

Raw dimension values are stored per cell; normalization happens at query time across the surviving set, so "best of these four" is computed on the four. The result is a guarantee: a well-formed request from a configured tenant always returns a model. Difficulty shortfalls and preference violations arrive as flags on a real recommendation, never as an empty response.

---

## Recommendation engine

Two outputs.

**Banded policy** (`GET /v1/policies`): per capability, a table mapping difficulty band to the cheapest model column whose competence ceiling clears the band at the tenant's quality bar, with a safety buffer. V1 ceilings come from observed per-tier pass rates on the authored tiers; IRT sharpens them in v1.1. Illustrative:

| Capability | Easy (1-2) | Medium (3) | Hard (4-5) |
|---|---|---|---|
| structured_extraction | gemini-3.1-flash | claude-sonnet-4-6 | claude-opus-4-8 |
| multi_step_reasoning | claude-haiku-4-5 | gpt-5.4 | gpt-5.5 |

**Single-workload answer** (`POST /v1/recommend`): classify the request (or accept a declared workload), filter by policy, then rank. The ranked list is the fallback chain, pool-constrained, so escalation never crosses a compliance boundary.

**Classification.** A declared workload (`capability`, `difficulty`, `domain`) skips classification and answers fast. A raw prompt goes through a classifier: v1 uses a small, cheap model (Haiku, Gemini Flash, or GPT mini tier) at temperature 0 with structured output, cached by prompt hash. The authored corpus doubles as labeled training data, so a trained classifier can replace the model call once volume justifies it. When `classification_routing` ships, its rankings pick the classifier model empirically, the same way `critique_and_judging` picks the judge. Privacy note surfaced to tenants: the prompt path sends the prompt to the classifier model; the declared path sends nothing.

**Tie-breaking.** Ties inside the noise band resolve to the cheaper column, and a same-family downgrade is preferred over a cross-family swap.

**Ceiling flag.** If the best allowed model sits below the estimated difficulty ceiling, return it anyway with `clears_difficulty: false`. The caller gets the best of their set and the risk.

**Constraint flag.** If every allowed model violates a per-request preference, the pick still returns, carrying `constraints_met: false` and the named violations. The caller's inference always has a model to run on.

---

## API

Base: `https://api.modelmaverick.dev/v1` (placeholder). Auth: per-tenant API keys. All responses include `matrix_version`.

**POST /v1/recommend**

```json
{
  "workload": { "prompt": "Extract the parties, dates, and obligations from this contract: ..." },
  "pool": "vertex-only",
  "allowed_models": ["claude-sonnet-4-6", "claude-opus-4-8", "gemini-3.1-pro", "gemini-3.1-flash"],
  "mode": "balanced",
  "constraints": { "max_latency_ms": null, "max_cost_per_run": null }
}
```

Response:

```json
{
  "classification": {
    "capability": "structured_extraction",
    "difficulty": 3,
    "domain_tags": ["legal"],
    "high_stakes": true,
    "confidence": 0.91,
    "method": "classified"
  },
  "recommendations": [
    { "provider": "anthropic", "model": "claude-sonnet-4-6", "endpoint": "vertex",
      "score": 94.1, "clears_difficulty": true,
      "reason": "cheapest column clearing tier 3 extraction at your quality bar in this pool" },
    { "provider": "anthropic", "model": "claude-opus-4-8", "endpoint": "vertex",
      "score": 96.3, "clears_difficulty": true, "reason": "escalation target" },
    { "provider": "google", "model": "gemini-3.1-pro", "endpoint": "vertex",
      "score": 92.8, "clears_difficulty": true, "reason": "cross-family alternative" }
  ],
  "pool": "vertex-only",
  "mode": "balanced",
  "check_spec": { "type": "json_schema", "ref": "checks/extraction/v3" },
  "constraints_met": true,
  "prompt_hash": "hmac_sha256:9f2c...",
  "recommendation_id": "rec_01J...",
  "matrix_version": "2026-07-01",
  "measured": true
}
```

Scores are normalized over the surviving set at query time under the requested mode. Index 0 is the pick; the array is the escalation chain. `constraints_met` reports whether the pick satisfies the request's preferences, with violations named when it does not. `prompt_hash` is the one-way fingerprint described under Tracing and debugging.

**GET /v1/policies?capability=structured_extraction&pool=vertex-only** returns the banded table with ceilings, buffers, and the quality bar used.

**GET /v1/models** returns the column catalog: provider, model, endpoint, settings, status, pricing snapshot, deprecation dates where published.

**POST /v1/outcomes**

```json
{ "recommendation_id": "rec_01J...", "model_used": "claude-sonnet-4-6",
  "outcome": "parse_failure", "escalated_to": "claude-opus-4-8" }
```

V1 collects outcomes and exposes them in history. Blending outcomes into scores is v1.1; production feedback is noisy and needs its own weighting design before it touches rankings.

**GET /v1/history/models/{column}?capability=...** returns the smoothed score series with regression markers. **GET /v1/history/recommendations** returns the tenant's audit log, filterable by `prompt_hash`. **GET /v1/history/recommendations/{recommendation_id}** returns one decision trace in full. **GET /v1/history/calibration?capability=...** returns the per-night Brier score series, for trend analysis of how well the benchmark predicts itself over time.

**Tenant configuration:** `PUT /v1/config/pools`, `PUT /v1/config/weights`, `PUT /v1/config/constraints`.

**MCP server:** a thin wrapper exposing `recommend_model`, `get_policy`, and `list_models` as tools, so agents and Claude Code sessions can ask Maverick directly. Same auth, same responses.

---

## Tracing and debugging

Built for one support conversation in particular: a customer says the system keeps returning a small model where a large one was deserved, and the cause has to be findable.

**Request identifier.** Every response carries a `recommendation_id`, and callers log it beside their inference call. The by-id endpoint returns the full decision trace: the classification with its confidence and method, the pool and allowlist applied, the mode and weights, every surviving column with its query-time score, every excluded column with the reason it was excluded, the flags, and the `matrix_version`. Haiku-instead-of-Opus resolves directly from this record: either Opus is in the trace with the numbers that ranked it behind (mode band, price tie-break, tenant weights), or it is in the exclusion list with the reason it never competed.

**Prompt hash.** Prompts are never stored by default. Each request is fingerprinted with a hash-based message authentication code (HMAC) over Secure Hash Algorithm 256 (SHA-256), keyed per tenant. The hash is one-way: the prompt cannot be recovered from it, and without the tenant key a guessed prompt cannot even be tested against it. The same prompt always produces the same fingerprint, so the audit log answers how often an exact prompt arrived and what it was classified as each time. The fingerprint is exact-match only: a one-character edit produces an unrelated hash. It is also the classifier cache key, so one hashing scheme covers both uses.

**Debug capture.** The hash cannot show whether the classifier misread a prompt's content. For that, a tenant can enable capture: a time-boxed window during which incoming prompts are stored encrypted, access-logged, and hard-deleted at expiry. Capture is off by default, requires explicit tenant enablement, can be scoped to one API key or sampled to low-confidence classifications only, and is refused for tenants whose configuration promises zero retention. A captured prompt enables replay: a debug endpoint re-runs classification on it, against the current classifier or the version live at the time, and shows whether the difficulty call reproduces. That settles whether a misroute was a classification error or a policy outcome.

**The durable fix.** When the trace shows one code path repeatedly misclassifying, the correction is standing, not forensic: that path declares `capability` and `difficulty` and skips the classifier. A `quality_insufficient` outcome posted to `POST /v1/outcomes` with the `recommendation_id` puts the complaint in the same history, so the trend is visible per capability and per route.

---

## Data model

Postgres. Core tables:

- `tenants`, `api_keys`
- `model_columns` (provider, model_id, endpoint, settings jsonb, status, pricing jsonb)
- `tasks` (capability, domain_tags[], difficulty_tier, grader_type, grader_spec jsonb, corpus_version, active)
- `runs` (task_id, model_column_id, tokens_in, tokens_out, latency_ms, cost, output_ref, temperature, ran_at)
- `scores` (run_id, dimension, value)
- `aggregates` (model_column_id, capability, difficulty_tier, dimension, ewma, variance, n, matrix_version, measured, as_of)
- `pools` (tenant_id, name, model_column_ids[])
- `tenant_weights` (tenant_id, capability, w_q, w_c, w_l, quality_bar)
- `recommendations` (tenant_id, prompt_hash, request jsonb with prompt text excluded, classification jsonb, candidates jsonb with survivor scores and exclusion reasons, response jsonb, matrix_version, created_at)
- `outcomes` (recommendation_id, model_used, outcome, details jsonb, created_at)
- `predictions` (batch_id, model_column_id, capability, difficulty_tier, predicted_pass_prob, source, matrix_version, locked_at)
- `calibration` (batch_id, capability, brier_score, overconfidence, n, scored_at)
- `debug_captures` (recommendation_id, prompt_ciphertext, captured_at, expires_at)

Raw values live in `scores` and `aggregates`; normalized scores are computed per query, never stored.

The Rails application keeps a separate database for users, organizations, and billing. Nothing in the engine references it.

---

## Scheduling, cost, and monitoring

Background jobs, cron-driven:

- **nightly_smoke:** 5 high-signal tasks per capability spanning an easy anchor and a hard anchor, so a frontier regression is caught the night it happens. Long-context smoke tasks run at 32k tokens to keep cost sane.
- **weekly_sweep:** the full corpus, with full-length long-context tasks.
- **latency_probes:** thin per-endpoint pings for columns whose quality is measured elsewhere.
- **aggregate_rebuild, regression_scan:** recompute EWMAs, flag drops.
- **price_sync, catalog_sync:** provider pricing and model identifiers change; both are data, not code.

Rough v1 spend, to be validated in M0: the smoke set is about 50 tasks times 12 columns times 3 repeats, near 1,800 runs a night, plausibly $25 to $80 depending on token mix; the weekly sweep runs 4 to 5 times that. Every job computes projected cost first and refuses to start past a configurable ceiling.

Operator alerts (email or webhook): regression flags, parse-failure rate climbing on any column, benchmark job failures, spend approaching the cap. A climbing parse-failure rate is the leading indicator of silent escalation on the caller's side, so it is watched even though v1 performs no routing.

---

## Tech stack

Go for the entire engine.

- **Maintenance:** the Go 1 compatibility promise keeps upgrades routine. A service of this shape needs about a dozen dependencies, with the standard library covering Hypertext Transfer Protocol (HTTP), JavaScript Object Notation (JSON), and cryptography. npm's version churn and supply-chain exposure are a recurring tax this stack does not pay.
- **Workload fit:** the runner mixes thousands of concurrent provider calls with central processing unit (CPU) bursts from graders (diff application, rule scans, schema validation). Goroutines handle both in one process. Workers idle in tens of megabytes, and the service ships as one static binary in a distroless image.
- **V2 alignment:** the on-premises, air-gapped enterprise build compiles from the same codebase: one binary, a signed license file, no runtime for the customer to install.

The recommend endpoint reads precomputed aggregates, so the language runtime is not the latency driver; the classifier call dominates the prompt path.

Concrete choices:

- **Language:** Go, current stable release
- **HTTP:** net/http with chi for routing
- **Postgres:** pgx as the driver, sqlc for typed queries generated from Structured Query Language (SQL) files (typed queries catch drift at compile time), goose for migrations
- **Jobs and cron:** River, a Postgres-backed queue, so there is no Redis to run; periodic jobs cover the nightly and weekly schedules
- **Provider clients:** official Go software development kits (SDKs) from Anthropic, OpenAI, and Google; xAI through its OpenAI-compatible API
- **MCP:** the official Go SDK for the MCP server wrapper
- **Observability:** structured logs with slog; metrics can wait
- **Deploy:** multi-stage Docker build to a distroless image, shipped with Kamal to Hetzner

Containment for Go's two weak spots: tagged-union shapes (grader specifications, policy rules, workload variants) are modeled as versioned JSON schemas validated at the boundary, with a small interface per grader type; the v1.1 statistics (IRT fitting, matrix completion) go to gonum first, with a small Python sidecar as the fallback for that one batch step.

The Rails application (v1.1) is the product shell: Rails 8 on its own Postgres, Devise for authentication, Pay with Stripe for billing, Hotwire for the dashboard, deployed with Kamal beside the engine. One Rails organization maps to one engine tenant by identifier. TypeScript shrinks to the customer JavaScript SDK, with the engine's API contract as the boundary.

---

## Build sequence

Milestones M0 through M6 cover version 1. They are deliverable-based and ordered by dependency. Each is done when its deliverables ship, not when a date passes.

- **M0. Skeleton:** repository (Go, chi, pgx with sqlc, River), schema and migrations, tenant authentication, provider clients for the four launch providers, runner that executes one task against one column and stores a run.
- **M1. First capability end to end:** author `structured_extraction`, 20 to 25 tasks across five tiers with deterministic and reference graders. Nightly smoke job, EWMA, first aggregates. Extraction goes first because it is the most cross-cutting capability and its graders are fully objective.
- **M2. Recommend API:** policy engine (pools, allowlists, constraints, query-time normalization), `POST /v1/recommend` with declared workloads, `GET /v1/policies`, audit log.
- **M3. Classification:** prompt-path classification with the small classifier model, cached; confidence surfaced; declared path unchanged.
- **M4. Corpus buildout:** author the remaining nine capabilities, wire the cross-family judge for the rubric dimensions, add the high-stakes overlay and tags. The system stays live for extraction throughout.
- **M5. Product surface:** outcomes endpoint, history endpoints, regression and spend alerts, MCP server, internal provisioning endpoints (create tenant, issue and revoke keys, set pools) behind service-to-service authentication so the Rails seam exists before Rails does, documentation, tenant onboarding for tenant #1.
- **M6. Dogfood and harden:** tenant #1 is you. Route model selection for your own products through Maverick before any external tenant. Every gap found here is a gap a customer never sees.

---

## Competitive position

Gateways (OpenRouter, LiteLLM, Portkey, Bifrost, Bedrock, Vertex) execute and route inference; Azure ships a model router with quality, balanced, and cost presets. Evaluation tools (LM Evaluation Harness, Braintrust, Langfuse, Helicone, OpenAI Evals) measure and observe. The closest routing-intelligence APIs as of early 2026 are Not Diamond and Martian. Maverick's position: benchmark-grounded recommendations with measured difficulty ceilings, tenant policy as a hard boundary, an audit trail from score to decision, and no inference lock-in, so it complements gateways rather than competing with them. Research grounding lives in the technical spec, with all sources listed in the References section below.

---

## Risks and open questions

- **Corpus authoring is the critical path.** 200 to 250 tasks with tight graders is the largest single effort in the build. Mitigation: extraction first, system live from M1, capabilities added while it runs.
- **Judge cost and bias** are contained in v1: deterministic and reference graders primary, one cross-family judge, order-swapped, temperature 0, monthly calibration against operator ratings.
- **Provider terms of service on benchmarking:** review each provider's terms for automated evaluation and publication of results before any public leaderboard exists. Tenant-facing scores are a different exposure than published rankings.
- **Classifier accuracy** bounds recommendation quality on the prompt path. Confidence is surfaced so callers can require declared workloads for high-stakes routes.
- **Cold start for new models:** a new column runs dense (full corpus, extra repeats) for several nights before its scores drive recommendations; thin data is flagged `measured: false`.
- **Naming:** confirm the Model Maverick name clears trademark and domain checks before anything public.

---

## References

Kept for historical and reference purposes. The companion technical specification (capability-matrix-design.md) maps each source to the specific design decisions it drove.

**Provided source material**

- Siciliani, T. "Choosing the Right LLM: A Decision Matrix." Medium. https://medium.com/@tsiciliani/choosing-the-right-llm-a-decision-matrix-afcace996d11. A three-step manual framework: licensing and privacy first, then requirements, then a custom evaluation pipeline on your own data. Its first step became the hard pre-filter in the policy engine; its third step is what Maverick automates and runs continuously.
- anadim/llm-benchmark-matrix (BenchPress). GitHub. https://github.com/anadim/llm-benchmark-matrix. Sparse model-by-task grids can be filled in by prediction at roughly 7 percent median error from about a third of cells observed, and a 5-task minimum set predicts the rest. Basis for the nightly smoke set, matrix completion in v1.1, and the cold-start rule for new model columns.
- X-RAY: "Mapping LLM Reasoning Capability via Formalized and Calibrated Probes." Gao et al., National University of Singapore, 2026. arXiv 2603.05290. https://arxiv.org/html/2603.05290. Difficulty parameterized along explicit structural dimensions so failure can be localized instead of averaged away. Basis for the structural difficulty knobs in the workload taxonomy.
- SCAN: "Structured Capability Assessment and Navigation" (first released as "From Rankings to Insights"). Wang, Gu, et al. arXiv 2505.06698. https://arxiv.org/html/2505.06698v4. Evaluation shifted from a single leaderboard rank to per-capability feedback. The backbone of the capability-first taxonomy.

**Supporting research**

- RouteLLM: "Learning to Route LLMs with Preference Data." Ong et al. International Conference on Learning Representations (ICLR) 2025. arXiv 2406.18665. A trained router reached 95 percent of GPT-4 quality while sending 26 percent of calls to the strong model. Positions Maverick as the offline measurement layer that feeds a router.
- RouterBench. Hu et al., Martian. arXiv 2403.12031. A routing benchmark in which a router over a pool of models beat the best single model in the pool.
- Judge-bias literature, 2025 to 2026: JudgeBiasBench (Zhou et al.), FairJudge (Yang et al.), and a RAND Corporation reliability study. Frontier judges exceeded 50 percent error on hard bias tests, and no judge was uniformly reliable across benchmarks. Source of the five-bias taxonomy (position, verbosity, self-preference, format, authority) and the judge mitigations: cross-family judging, order swapping, reference anchoring, scheduled recalibration.
- Code-judge reliability. arXiv 2604.16790. Judge models return different verdicts on identical cases under fixed prompts and rubrics, so judges get the same repeat-and-variance treatment as the models under test.
- Hybrid evaluation norm: reinforcement learning from verifiable rewards (RLVR) applied to evaluation, 2026. Verifiable checks answer whether the problem was solved; rubric judging answers how well. The pairing behind the grader-per-task design.
- Dynamic-routing survey, 2026. The when, what, and how axes of routing design, used to place Maverick: decision before the request, fed by measured past performance, computed offline into a ranking table.
- Item response theory applied to model evaluation, 2025 to 2026. Joint fitting of model ability and task difficulty from pass data. Basis for the v1.1 calibration that replaces pass-rate ceilings.
- Routing postmortem on silent cascade escalation (TrueFoundry, 2026). A provider-side output format change caused a schema check to fail and about 90 percent of traffic to escalate silently to the most expensive model. Source of the parse-failure-rate alert and the outcome monitoring design.

**Product documentation**

- Google Cloud Vertex AI Model Garden documentation. Provider coverage at design time: Gemini native, Claude and Grok as partner models, open-weight models included, OpenAI proprietary models absent.
- AWS announcement of OpenAI frontier models on Amazon Bedrock, April 2026. Ended the Azure exclusivity arrangement; GPT-5.5, GPT-5.4, and Codex generally available on Bedrock, with GovCloud availability. Informed the pool design for compliance-restricted tenants.
- Azure Model Router documentation. Quality, balanced, and cost presets, mirrored in Maverick's three recommendation modes.
