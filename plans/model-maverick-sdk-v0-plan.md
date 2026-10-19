# Model Maverick — SDK v0 Plan

**Status:** draft for review (2026-07-14; revised 2026-09-28 to adopt the
privacy, workload-projection, settings, and telemetry changes from
`plans/master-plan.md` §3.5, now in `specs/model-maverick-v1-spec.md`)
**Parent:** `model-maverick-integration-surfaces-plan.md` (Track 1). This doc plans
the initial SDK line-up: **TypeScript, Ruby, Go, Python** — decided 2026-07-14.
**Governing guarantees:** G13 (zero ceremony, telemetry opt-out honored), G14
(fail-open — an SDK can never break or block the customer's LLM call; privacy
outranks it for zero-retention tenants, v1 spec §1.1).

## 1. What v0 means

Each SDK ships: the shared core behaviors (§2), **one flagship integration** for
its ecosystem, conformance-suite green (§2.3), and the 60-second story — install,
one config touch, `maverick/auto` where a model name used to go. v0 does *not*
mean every framework integration per language; fast-follows are listed per SDK.

## 2. The shared core — specified once, implemented four times

### 2.1 Behaviors (identical across languages)

- **Alias grammar:** `maverick/auto` (= balanced) | `maverick/quality` |
  `maverick/cost` | `maverick/<pool>:<mode>`, each optionally suffixed
  `?settings=caller`. Anything else passes through untouched — a concrete model
  name means "don't route, just call it."
- **Recommend client:** declared + prompt forms; request timeout default 2s
  (the declared hot path answers in single-digit ms; the uncached prompt path
  targets p95 < 1.8 s, inside the 2s timeout, v1 spec §2.3).
- **Workload projection** (added 2026-09-28): the prompt form is built by one
  versioned projection shared with the proxy: system/developer
  instructions, bounded recent conversation text (not only the last user
  message), the response-format schema, and tool names and descriptions,
  excluding binary content. The core sends the projection version
  (`workload.projection_version`), which the engine folds into its
  classification cache key (v1 spec §8.1). A caller-supplied
  `X-Maverick-Workload: <capability>[:<tier>]` (or the language's equivalent
  per-call option) switches that call to the declared form; docs recommend it
  for production routes.
- **Settings** (added 2026-09-28): every chain entry carries its column's
  `settings` (reasoning effort, thinking mode). The core applies them to the
  provider call by default and records `settings_applied` in telemetry; the
  `?settings=caller` alias suffix opts out and leaves the caller's own values
  untouched.
- **Chain executor:** try `recommendations[0]`; advance on 429 / 5xx / timeout /
  connection error; respect `Retry-After`; **never** advance on semantic 4xx (a
  bad request is bad on every model); per-entry timeout; exhausted chain →
  surface the last provider error unchanged.
- **Fail-open ladder (G14):** MM unreachable or erroring → last-known-good chain
  for that workload key (in-memory cache, TTL default 5m) → configured
  `default_model` → if the caller passed a concrete model, just call it. MM's
  absence must be observable only in logs.
- **Terminal privacy errors** (added 2026-09-28): `422
  zero_retention_requires_declared` and `409 no_compliant_columns` from the
  engine are not fail-open triggers. The core purges its last-known-good chains
  for that tenant and surfaces the error to the caller.
- **Zero-retention tenants and the G14 privacy override** (added 2026-09-28):
  recommend responses carry `zero_retention: true` for such tenants, and every
  chain entry carries its `retention_mode`. On seeing the flag the core switches
  to declared mode and keeps the tenant's zero-retention state in memory.
  During an engine outage its fail-open ladder then uses only cached entries
  stamped `retention_mode: zero`, uses `default_model` or the caller's original
  model only if locally configured as verified zero-retention
  (`MODELMAVERICK_ZERO_RETENTION_VERIFIED` lists them), and otherwise fails
  closed with `no_compliant_columns`.
- **Telemetry batcher:** in-memory queue; flush on interval (default 10s), size
  (default 20), and process exit; single batched `POST /v1/telemetry`; failures
  dropped silently, never retried more than once, never blocking. Items carry
  `check_pass` as three-valued: `true`, `false`, or absent when no check ran
  (absent derives `completed_unchecked`, which is not quality evidence), plus
  `settings_applied`. **Opt-out:**
  config `telemetry: off` / env `MODELMAVERICK_TELEMETRY=off` → the SDK never
  opens a connection to `/v1/telemetry` at all (G13).
- **Privacy modes:** `declared` (send capability/difficulty only — nothing else
  leaves the process) | `prompt` (default; the workload projection to MM for
  classification) | `off` (never call MM; use default model — the safe kill
  switch). A `zero_retention: true` response forces `declared` for that tenant.
- **Config surface, uniform names:** `MODELMAVERICK_API_KEY`,
  `MODELMAVERICK_BASE_URL`, `MODELMAVERICK_TELEMETRY`,
  `MODELMAVERICK_DEFAULT_MODEL`, `MODELMAVERICK_PRIVACY`. Provider keys are never
  MM config — they stay in each provider SDK's standard location.

### 2.2 What the core never does

No provider schema translation (the leverage rule — the host ecosystem does the
wire work). No disk state. No background threads beyond the telemetry batcher.
No retries of MM itself beyond one (fail-open is cheaper than resilience).

### 2.3 Conformance harness — the thing that keeps four languages honest

The engine repo ships `cmd/sdk-conformance`: a mock MM + mock provider server
(Go, reusing engine test plumbing) that runs scripted scenarios, plus JSON test
vectors. Every SDK repo runs it in CI. Scenarios, enumerated once here and
frozen as the cross-language contract:

1. Declared-form happy path; prompt-form happy path.
2. Provider 429 with `Retry-After` → chain advances after the wait.
3. Provider 500 / timeout / connection refused → chain advances.
4. Provider 400/422 → no advance, error surfaced verbatim.
5. Chain exhausted → last provider error surfaced.
6. MM down → cached chain used; cache cold → default model; `privacy: off` →
   MM never contacted. Zero-retention cases (added 2026-09-28): (a) engine
   answers `422 zero_retention_requires_declared` or `409 no_compliant_columns`
   → error surfaced, no fail-open, cached chains for the tenant purged; (b)
   warm-cache engine outage for a zero-retention tenant → only `zero`-stamped
   entries are used; (c) cold-cache engine outage for a zero-retention tenant →
   fails closed unless the default model is locally marked verified
   zero-retention.
7. Telemetry: batch shape, interval flush, exit flush, silent drop on failure;
   `check_pass` absent when no check ran (never defaulted to `true`);
   `settings_applied` set when entry settings were applied.
8. `MODELMAVERICK_TELEMETRY=off` → zero bytes to `/v1/telemetry` (asserted by
   the mock).
9. Alias parsing table, including pass-through of concrete model names and the
   `?settings=caller` suffix (entry settings applied by default, left alone
   with the suffix). The prompt-form request carries the workload projection
   and its version; an `X-Maverick-Workload` override sends the declared form.

## 3. The four SDKs

### 3.1 TypeScript — `modelmaverick-sdk-ts` (npm scope `@modelmaverick`)

- **Flagship:** `@modelmaverick/ai-sdk` — Vercel AI SDK meta-provider
  (implements their documented provider spec; delegates to the customer's
  installed `@ai-sdk/*` providers, which do translation/streaming/tools).
- Also at v0: `@modelmaverick/openai` — OpenAI SDK wrapper via its custom
  `fetch` hook; cross-family reach via provider OpenAI-compat endpoints.
- Packages: `core`, `ai-sdk`, `openai`. Node ≥ 20, ESM + CJS.
- **Why first:** largest population of LLM app code; the meta-provider is the
  single highest-leverage artifact in the whole initiative.
- Fast-follows: `@modelmaverick/anthropic`, LangChain.js.

### 3.2 Ruby — `modelmaverick-sdk-rb` (gem `modelmaverick`)

- **Flagship:** RubyLLM meta-layer — ask recommend, drive RubyLLM with the pick
  (it translates to OpenAI/Anthropic/Gemini/Bedrock/…), rescue provider errors →
  `with_model(next_in_chain)`, telemetry from a background thread.
- Rails first-class: generator-installed initializer
  (`config/initializers/modelmaverick.rb`), Solid-Queue-friendly batcher
  (in-thread by default; optional ActiveJob adapter).
- Ruby ≥ 3.2. Zero-gem interim path (RubyLLM `openai_api_base` → MM-routed
  proxy) stays documented for clients who arrive before the gem.
- **Why second:** the operator's early customers are Rails + RubyLLM — the gem
  also slots into the Rails product template's `add-ai-feature` template later.
- Fast-follows: bare `ruby-openai` wrapper if demand shows.

### 3.3 Go — `modelmaverick-sdk-go` (module `github.com/modelmaverick/modelmaverick-go`)

- **Dual role, build once use twice:** this module IS the proxy's routing
  library — `cmd/maverick-proxy` imports the same core (recommend client, chain
  executor, telemetry batcher). The proxy milestone depends on this one.
- **Flagship interception:** an `http.RoundTripper` — the idiomatic Go seam.
  Wraps any OpenAI-compatible client: intercepts `chat/completions` bodies whose
  `model` is `maverick/*`, rewrites model + upstream host per the pick, executes
  the chain. Works with `openai-go`, `anthropic-sdk-go` (both accept a custom
  HTTP client/middleware — spike validates the Stainless middleware hook), and
  hand-rolled `net/http`.
- Also: a plain `Client` for teams who want explicit
  `Recommend()`/`Execute(chain, fn)` calls — Go culture tolerates explicitness.
- Go ≥ 1.22. Telemetry batcher = goroutine + channel, `Close()` flushes.
- Fast-follows: langchaingo integration.

### 3.4 Python — `modelmaverick-sdk-py` (PyPI `modelmaverick`)

- **Flagship:** LiteLLM **library** router strategy — MM picks, LiteLLM executes
  across its 100+ providers, its callbacks feed telemetry. This same code seeds
  the Track 2 proxy plugin — second dual-role artifact.
- Also at v0: `modelmaverick[openai]` — OpenAI SDK wrapper via custom `httpx`
  client. Sync + async both.
- Python ≥ 3.10.
- Fast-follows: LangChain, instructor.

## 4. Sequencing

Supersedes the parent plan's N2–N7 table:

| # | Milestone | Contents | Depends on |
|---|---|---|---|
| N1 | Engine prerequisites | `/v1/telemetry`, outcomes-ceremony removal, alias grammar, correlation headers | — |
| N2 | Core spec + conformance harness | §2 frozen as a spec doc; `cmd/sdk-conformance` mock server + vectors | N1 |
| N3 | TypeScript SDK | core + AI SDK meta-provider + OpenAI wrapper | N2 |
| N4 | Ruby SDK | core + RubyLLM meta-layer + Rails initializer | N2 |
| N5 | Go SDK + proxy | Go core + RoundTripper; `cmd/maverick-proxy` consumes it | N2 |
| N6 | Python SDK + LiteLLM proxy plugin | core + LiteLLM strategy + OpenAI wrapper; Track 2 plugin from the same strategy | N2 |
| N7 | Fast-follows | Anthropic wrappers, Portkey spike/decision, langchaingo, docs pass | N3–N6 |

N3–N6 are independent after N2 and can run in parallel or in the listed order
(TS → Ruby → Go → Python). The listed order is the recommendation: TS for market
size, Ruby because the early customers are waiting, Go because the proxy is
blocked behind it, Python last because its interim story (LiteLLM without MM) is
the least painful.

## 5. Open decisions

1. **Repo layout** — one repo per language (recommended: matches each
   ecosystem's packaging norms and CI) vs a single `modelmaverick-sdks`
   monorepo.
2. **Go module home** — `github.com/modelmaverick/...` org naming; and whether
   the Go core lives in the engine repo (import cycle risk: proxy is already
   there) vs its own module (recommended: own module; the proxy imports it like
   any consumer).
3. **Parallel vs serial N3–N6** — serial keeps review load sane; parallel ships
   the set faster if the dev-team loop can fan out.
4. **Ruby ActiveJob adapter at v0** — in-thread batcher only (recommended) vs
   shipping the adapter now.
5. **Version floors** — proposed Node 20 / Ruby 3.2 / Go 1.22 / Python 3.10;
   confirm against the early customers' actual fleets.
