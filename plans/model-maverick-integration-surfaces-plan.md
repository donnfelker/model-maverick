# Model Maverick — Integration Surfaces Plan

**Status:** draft for review (2026-07-14; revised 2026-09-28 to adopt the G14
privacy override, three-valued `check_pass`, and the proxy's tenant-header
and credential rules from `plans/master-plan.md` §3.5)
**Origin:** adversarial review of the invocation mechanic (2026-07-14). Finding: the
recommend API is right as the engine core and wrong as the only door — every
production consumer except MCP agents must hand-build a router around it. Companion
user decision: **`POST /v1/outcomes` must not exist as a customer obligation** —
outcome data arrives automatically or not at all.
**Relationship to v1.1:** independent initiative (working name v1.2) except Phase 0,
which re-scopes v1.1 §7 (outcome blending) and §18 (endpoint table). Engine hot path
and Parts II–IV of v1.1 are untouched.

## 0. The principle

**Zero ceremony.** A customer adopts Model Maverick by changing one line, and every
byte of learning data the engine receives is a side effect of that adoption — never
a favor the customer performs. Three surfaces deliver this for three segments:

| Track | Surface | Segment | Data custody |
|---|---|---|---|
| 1 | Client SDKs (in-process interception) | App teams — the main market | None; interception happens in the customer's process |
| 2 | Gateway plugins | Platform teams with existing gateways | None; their gateway keeps the traffic |
| 3 | Proxy (base-URL integration) | Any OpenAI or Anthropic client; 60-second first taste | Transit only |

**Leverage rule (binding for Tracks 1–2):** never write provider schema translation.
Ride the ecosystems that already ship it — Vercel AI SDK providers, the LiteLLM
library, and providers' own OpenAI-compat endpoints. MM contributes the decision,
the chain, and the telemetry; the wire work is someone else's solved problem.

### New guarantees

- **G13 — Zero ceremony.** No public surface asks the caller to report anything
  back. Outcome data is produced only by MM-authored components (SDKs, plugins,
  proxy) as an invisible side effect. `POST /v1/outcomes` is removed as a public,
  documented obligation. Telemetry is **opt-out configurable** in every component
  (§2): disabled means disabled — zero calls to `/v1/telemetry`, no fallback
  phoning home. The documented consequence is the tenant's own outcome blending
  has no data (their rankings stay benchmark-driven) — stated, never punished.
- **G14 — Fail-open.** An MM component failure never breaks or blocks the
  customer's LLM call. SDKs and plugins degrade: last-known-good chain (TTL cache)
  → configured static default → the customer's original model string. Telemetry is
  fire-and-forget and drops silently. (The proxy follows the same ladder for a
  `maverick/*` route: its last good list, then a per-surface default model, then
  `503 engine_unavailable`; D46, adopted 2026-10-24. The Go proxy was exempt.) **Privacy outranks G14** (added
  2026-09-28): for a zero-retention tenant, fallback may use only cached entries
  stamped `retention_mode: zero`, and the static default or original model only
  if locally configured as verified zero-retention; otherwise the call fails
  closed. `422 zero_retention_requires_declared` and `409 no_compliant_columns`
  are terminal privacy errors, never fail-open triggers (v1 spec §1.1).

## 1. Phase 0 — engine prerequisites (shared by all tracks)

Small, and a precondition for everything below.

- **Retire the outcomes ceremony.** Remove `POST /v1/outcomes` from `docs/api.md`,
  the v1 spec's public contract, and onboarding. The `outcomes` storage, the
  `/v1/history/outcomes` read surface, and v1.1 §7 blending all survive — only the
  producer changes.
- **`POST /v1/telemetry`** — the automatic replacement. Batch array; item:
  `{recommendation_id, model_used, status, latency_ms, tokens_in, tokens_out,
  finish_reason, error_class?, check_pass?, endpoint_used?, settings_applied?}`.
  Best-effort by contract: accepted entries map onto the existing outcome rows
  (`success` / `parse_failure` / `completed_unchecked` / `provider_error` /
  `latency_exceeded` derived server-side from status + error_class + check_pass);
  malformed items are dropped and counted, never 4xx the batch. `check_pass` is
  three-valued (true, false, absent): a 2xx with no check derives
  `completed_unchecked`, which counts toward availability only and never enters
  outcome blending (v1 spec §9.10, changed 2026-09-28). Same tenant Bearer auth. Documented only in SDK/plugin internals — it is
  plumbing, not API surface for customers.
- **Model name grammar**, defined once, parsed identically by every track (D45,
  adopted 2026-10-24; spec §8.1): `maverick/auto` (the default pool, quality),
  `maverick/auto:<variant>`, and `maverick/<pool>[:<variant>]`, with variant
  `quality`, `balanced`, or `cost`. Any other string is a direct model, called as
  the caller wrote it.
- **Correlation convention:** components stamp `X-Maverick-Recommendation-Id` (and
  `X-Maverick-Model-Used`) on responses they touched.
- **v1.1 spec edits when accepted:** §7 data source becomes telemetry-observed
  outcomes (shrinkage-first design unchanged, and strengthened — observed data
  beats volunteered data); §18 endpoint table drops `POST /v1/outcomes`; G11
  unchanged.

## 2. Track 1 — client SDKs (the production path)

**v0 line-up (decided 2026-07-14): TypeScript, Ruby, Go, Python** — per-SDK
build detail, shared-core spec, and conformance harness in
`plans/model-maverick-sdk-v0-plan.md`. New repos: `modelmaverick-sdk-ts` (npm
scope `@modelmaverick`), `modelmaverick-sdk-rb` (gem `modelmaverick`),
`modelmaverick-sdk-go` (Go module, also the proxy's routing library),
`modelmaverick-sdk-py` (PyPI `modelmaverick`). All are thin shells around a
shared core design:

**Core (per language):** recommend client (declared + prompt forms); chain executor
(advance on 429/5xx/timeout/connection error, respect `Retry-After`, never advance
on semantic 4xx); telemetry batcher (interval + exit flush, fire-and-forget);
resilience per G14 (per-workload-key last-good chain cache with TTL, static default,
fail-open); privacy modes `declared | prompt | off` (declared sends nothing but
capability/difficulty; off never calls MM and uses the configured default —
a kill switch that keeps deploys safe; a `zero_retention: true` recommend
response forces declared mode, and the engine's two privacy codes are terminal,
per G14's privacy rule); workload projection and the `X-Maverick-Workload`
declared header, and applying each entry's `settings` by default with an
`X-Maverick-Settings: caller` opt-out (D45), per `plans/model-maverick-sdk-v0-plan.md` §2.1;
telemetry switch `telemetry: on | off`
(default on, env `MODELMAVERICK_TELEMETRY=off`) — off suppresses all
`/v1/telemetry` traffic per G13. Config: `MODELMAVERICK_API_KEY` + optional
`MODELMAVERICK_BASE_URL`; provider keys stay exactly where they already live
(each provider SDK's standard env vars).

**TypeScript packages, in build order:**

1. `@modelmaverick/ai-sdk` — a Vercel AI SDK **meta-provider**. Implements the
   provider interface; on each call asks MM, then delegates to the customer's
   installed providers (`@ai-sdk/openai`, `@ai-sdk/anthropic`, `@ai-sdk/google`, …).
   The AI SDK already does per-provider translation, streaming, and tool calls —
   the meta-provider inherits every provider for free. Highest leverage; build
   first. Usage: `model: maverick('auto')`.
2. `@modelmaverick/openai` — wraps the OpenAI SDK via its custom-`fetch` hook;
   intercepts `chat.completions` where `model` is a `maverick/*` alias, rewrites
   model + baseURL. Cross-family reach via providers' OpenAI-compat endpoints
   (Anthropic and Gemini both ship them; a spike validates feature gaps —
   tool-call fidelity, streaming, stop sequences).
3. `@modelmaverick/anthropic` — same pattern on the Anthropic SDK; fast-follow.

**Python packages:**

1. `modelmaverick` core.
2. LiteLLM **library** integration — a custom Router strategy: MM picks (chain sent
   as the deployment ordering), LiteLLM executes and translates to its 100+
   providers, LiteLLM success/failure callbacks feed telemetry. Highest leverage in
   Python; also the direct ancestor of Track 2.
3. `modelmaverick[openai]` — custom httpx client wrapper, mirror of the TS one.

**Ruby (`modelmaverick-sdk-rb`, gem `modelmaverick`):** elevated because the
operator's own stack standardizes on Rails + RubyLLM. RubyLLM is the leverage
target — it already translates to OpenAI/Anthropic/Gemini/Bedrock/DeepSeek/… — so
the gem is the same meta-layer shape: ask recommend, drive RubyLLM with the pick,
rescue provider errors → `with_model(next_in_chain)` retry, telemetry from a
background thread (Rails apps get an initializer + Solid-Queue-friendly batcher).
Zero-gem fallback that works the day Track 2/3 ships, straight from RubyLLM's own
custom-endpoints docs: `config.openai_api_base` → an MM-routed OpenAI-compatible
proxy, `model: 'maverick/auto', provider: :openai, assume_model_exists: true`.

**The 60-second story (acceptance test for the whole track):** install one package,
wrap the client or register the provider, set `model` to `maverick/auto`. Nothing
else. Outcomes flow without the customer knowing the concept exists.

## 3. Track 2 — gateway plugins (the platform-team path)

- **LiteLLM proxy plugin first** (`modelmaverick-litellm`, shares the Python core).
  A custom routing strategy consults recommend pre-call — prompt form or declared
  form built from proxy metadata, per config — with the proxy's configured
  deployments passed as `allowed_models` (MM never recommends what the gateway
  can't serve). LiteLLM's native fallbacks execute the chain; its success/failure
  callbacks feed `/v1/telemetry`. Customer-visible integration: a few lines of
  `config.yaml`, including the same `telemetry: off` switch (G13).
- **Portkey: spike, then decide.** Validate whether request hooks can mutate the
  routing target from an external call. If yes, a plugin; if not, the degraded
  mode is a **config generator** — MM policies rendered into Portkey conditional
  routing configs on a refresh schedule (loses per-call routing, keeps MM as the
  brain).
- Kong AI Gateway and others: backlog, demand-driven.
- Deliverable includes a reference-architecture doc: *MM as the brain inside the
  gateway you already run* — the recipe the adversarial review found missing.

## 4. Track 3: the proxy (base-URL integration)

A separate small deployable — `cmd/maverick-proxy` in the engine repo, its own
binary, its own deployment, its own SLA class. The engine stays control-plane pure
by deployment topology, same as the internal listener does today.

- **Surface:** OpenAI-compatible `POST /v1/chat/completions` only, streaming and
  non-streaming. `model` MUST be a `maverick/*` alias — concrete model names are
  rejected with a docs pointer (the proxy exists to demonstrate routing, not to be
  a generic gateway).
- **No translation layer:** the proxy forwards OpenAI-format requests to the picked
  provider's OpenAI-compat endpoint (OpenAI native; Anthropic and Gemini compat
  endpoints). The proxy's pool is constrained to compat-capable providers. This is
  what keeps the proxy genuinely thin — recommend, pick, forward, stream back,
  telemetry.
- **Keys: caller credentials only** (decided 2026-09-28, replacing the
  `X-Maverick-Provider-Keys` JSON header). `Authorization` (or `x-api-key` on the
  Anthropic Messages surface) is reserved for the caller's provider credential.
  Two modes, fixed per deployment: **hosted** (MM operates the instance) accepts
  either a caller OpenRouter key (every entry executes through OpenRouter) or a
  surface-native credential, in which case the chain is restricted to that
  provider's entries and other entries are skipped; the hosted build reads **no
  provider env keys at all**, so a cross-provider pick cannot fall back to
  operator secrets. **Self-hosted** (the customer runs the binary) may also use
  the customer's own per-provider env keys (standard names, never
  `MAVERICK_`-prefixed). Keys live in memory for the request and are never
  stored or logged. Zero at-rest custody. This differs from the Go proxy, which
  falls back to env keys in any deployment; a test shows a hosted instance given
  only an OpenAI key never calls Anthropic.
- **Workload:** the proxy builds the prompt form with the shared versioned
  workload projection (`plans/model-maverick-sdk-v0-plan.md` §2.1), not only the
  last user message, and honors `X-Maverick-Workload: <capability>[:<tier>]` as a
  declared override. Entry `settings` are applied unless the request carries
  `X-Maverick-Settings: caller` (D45). Zero-retention tenants get the G14 privacy rule and the
  OpenRouter Zero Data Retention rules (OpenRouter execution spec §3.2).
- **Auth and fences:** the tenant's Maverick key travels in `X-Maverick-API-Key`
  (the hosted proxy is one shared instance; rate limits and telemetry are per
  tenant key, with isolation tests showing one tenant's key never recommends
  from another's pools; the self-hosted Go form with a single configured
  `MAVERICK_API_KEY` remains); tighter rate limits than the API;
  body cap; honors a tenant-level telemetry opt-out (the proxy still routes but
  writes no outcome rows — G13 holds even where MM is in-path); chain execution
  on provider failure per the core rules; response
  headers carry `X-Maverick-Recommendation-Id` and `X-Maverick-Model-Used`.
  Documented non-goals: embeddings, images, audio, batch, fine-tuning. The
  proxy is a production path (`plans/production-proxy-plan.md` §5).
- **Nice to have (not planned; removed by the owner 2026-09-28 as premature):**
  a "graduation link" response header pointing proxy users to another
  integration path (SDK or gateway plugin). Moot since 2026-09-30: the proxy is
  a production path.

## 5. Who uses which door — client coverage

The strategy is not "everyone uses one SDK." It is two wrapper families for the
bulk of app code, gateway plugins for platform teams, and OpenAI-compatible HTTP
as the universal fallback. Per client, the answer:

| Client today | Door | What they change |
|---|---|---|
| TS app on Vercel AI SDK | `@modelmaverick/ai-sdk` meta-provider | `model: maverick('auto')` |
| TS/JS app on OpenAI SDK | `@modelmaverick/openai` | wrap client, `maverick/*` alias |
| App on Anthropic SDK | `@modelmaverick/anthropic` | same |
| Python app on `openai` lib | `modelmaverick[openai]` | custom httpx client, alias |
| Python app on LiteLLM library | MM router strategy | router config |
| **Rails app on RubyLLM** | `modelmaverick` gem (meta-layer over RubyLLM); or zero-gem: `openai_api_base` → MM-routed proxy | gem initializer, or two config lines |
| Go app (openai-go, anthropic-sdk-go, net/http) | `modelmaverick-go` — `http.RoundTripper` interceptor or explicit client | wrap HTTP client, alias |
| Platform team with a gateway | Track 2 plugin (LiteLLM proxy first) | few lines of `config.yaml`; their app devs change nothing |
| Any other language (Java, PHP, C#, …) | OpenAI-compat client → self-hosted LiteLLM+plugin or the proxy | `base_url` + `maverick/*` alias |
| Agents | MCP surface (ships in v1 today) | tool call |
| None of the above | raw `POST /v1/recommend` — ask-then-act, hand-rolled dispatch | their code, their choice |

The last row stays open deliberately: it is the engine's native surface and carries
no obligations (G13 — nothing to report back). It is just no longer the pitch.

RubyLLM detail (verified against its docs): `config.openai_api_base` redirects its
`:openai` provider to any OpenAI-compatible URL — RubyLLM documents exactly this
pattern for LiteLLM/vLLM proxies — and `assume_model_exists: true` lets
`maverick/auto` through its model registry. So Rails clients have a production
path (self-hosted LiteLLM + MM plugin, N6) and the proxy (N5) if they arrive
before the gem (N4), which upgrades them to in-process interception, no proxy hop.

## 6. Sequencing

Superseded in detail by `plans/model-maverick-sdk-v0-plan.md` §4; summary:

| # | Milestone | Contents |
|---|---|---|
| N1 | Engine prerequisites | `/v1/telemetry`, outcomes-ceremony removal, alias grammar, correlation headers; v1.1 §7/§18 edits |
| N2 | SDK core spec + conformance harness | Shared behaviors frozen; `cmd/sdk-conformance` mock server + test vectors |
| N3 | TypeScript SDK | core + AI SDK meta-provider + OpenAI wrapper |
| N4 | Ruby SDK | core + RubyLLM meta-layer + Rails initializer |
| N5 | Go SDK + proxy | Go core doubles as the proxy's routing library |
| N6 | Python SDK + LiteLLM proxy plugin | LiteLLM strategy seeds the Track 2 plugin |
| N7 | Fast-follows | Anthropic wrappers, Portkey spike/decision, langchaingo, docs pass |

N3 to N6 are independent after N2: parallel or in listed order. The proxy
rides N5 because it imports the Go SDK core.

## 7. Open decisions

1. **Proxy keys**: decided 2026-09-28, caller credentials only, hosted
   mode reads no provider env keys, self-hosted may use env keys (§4). MM-owned
   capped keys were rejected (cost exposure, abuse handling, and a
   commerce problem the v1.1 shell isn't scoped for).
2. **Language order** — TS first (recommended: Vercel AI SDK meta-provider is the
   single highest-leverage artifact in the plan) vs Python first.
3. **Proxy hosting** — MM-operated hosted instance (recommended, it's the
   funnel) plus downloadable binary, vs binary-only.
4. **Scope label** — standalone v1.2 (recommended; v1.1 is already large and only
   Phase 0 touches it) vs folding into v1.1.
5. **Naming** — `/v1/telemetry` vs `/v1/observations`; whether
   `/v1/history/outcomes` renames to match (recommended: keep, storage and read
   surface are unchanged).
