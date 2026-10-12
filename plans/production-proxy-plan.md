# The proxy as a production path, with OpenRouter-style model names (proposed D45, D46)

Status: proposed 2026-09-30. Owner decisions from 2026-09-30 are in §2. The beads are under the epic "Production proxy and OpenRouter-style model names (D45, D46)".

## 1. Why

The proxy was planned as a try-it door: the easy way to try Maverick, with the SDKs as the production path (`plans/model-maverick-integration-surfaces-plan.md` §4). The owner rejects that split. A caller tries Maverick by changing the base URL. If it does not work for them, they change it back. The proxy is the production path for every agent that cannot take an SDK, for example Claude Code.

That try-it label carries rules that hurt production callers:

- The proxy is exempt from fail-open (G14). An engine outage fails every request through it.
- Only `maverick/*` aliases are accepted. A concrete model name is `400`.
- A low fixed rate limit (60 requests per minute per tenant key).
- "No production SLA" and "never the production data plane" in the plans.

The engine itself has no try-it concept. No engine crate uses the label.

## 2. Owner decisions (2026-09-30)

1. **The try-it label is gone everywhere.** The label is the word matched by the case-insensitive pattern `tr[i]al`; this plan writes the pattern instead of the word so it passes its own check. The component is "the proxy" (crate and binary `maverick-proxy`, unchanged). Every spec, plan, doc, README, comment, crate description, script, and open bead drops it.
2. **The proxy is a production path.** It follows G14 fail-open like the SDK cores.
3. **Model names follow OpenRouter's grammar.** OpenRouter model ids are `<author>/<slug>` with an optional `:variant` suffix (`openai/gpt-6-astra`, `openai/gpt-6-astra:batch`), and its router is `openrouter/auto` with variants (`openrouter/auto:nitro`, `openrouter/auto:floor`). The form `openrouter/openai/gpt-6-astra` is a gateway prefix that third-party tools (LiteLLM, TrueFoundry) put in front of OpenRouter's id; it is not OpenRouter's own id. Maverick follows OpenRouter's own form.
4. **`maverick/auto` means the best model.** It selects `quality` mode. Cost is an opt-in variant.

## 3. Model name grammar (D45)

| Model string | Meaning |
|---|---|
| `maverick/auto` | Route over the tenant's default pool, `quality` mode |
| `maverick/auto:quality` | Same as `maverick/auto` |
| `maverick/auto:balanced` | Default pool, `balanced` mode |
| `maverick/auto:cost` | Default pool, `cost` mode |
| `maverick/<pool>` | Route over the named pool, `quality` mode |
| `maverick/<pool>:<variant>` | Named pool, variant is `quality`, `balanced`, or `cost` |
| `<author>/<slug>` (for example `anthropic/claude-opus-5-5`) | No routing. Call that model directly |
| A bare provider model id (for example `claude-opus-5-5`) | No routing. Call that model directly on the surface's native provider |

Rules:

- A string that starts with `maverick/` is a Maverick route. Everything after the prefix up to the first `:` is the route name. `auto` is the default pool. Any other name is a pool name. The text after the first `:` is the variant. An empty route name, an empty variant, or an unknown variant is `400 malformed_alias` on the proxy (same code as today).
- Variants are exact and case sensitive: `quality`, `balanced`, `cost`. There is no `auto` variant.
- A pool cannot be named `auto`, and a pool name cannot contain `:`. `PUT /v1/config/pools` rejects both with `422 invalid_request` naming the pool. Pool names may contain `/`.
- **Settings opt-out.** The `?settings=caller` suffix (D14) is removed from the grammar. A caller sends the header `X-Maverick-Settings: caller` instead (SDK cores: the existing option, now sent as a field, not a suffix). Any other value of the header is `400 invalid_request`. The proxy strips the header upstream. The OpenRouter grammar has no query suffix, and a header does not collide with a future variant.
- **Direct models (no routing).** A string that is not a Maverick route is sent to the provider as the caller wrote it, with one change: an `<author>/` prefix is removed when the author is the surface's native provider (`anthropic/` on `/v1/messages`, `openai/` on `/v1/chat/completions`), because the provider APIs do not accept it. An author that is not the surface's provider is `400 invalid_request` with message `model <author>/<slug> is not served by this surface`, because the proxy does not translate schemas. With a caller OpenRouter key (hosted mode, D12), the string is sent to OpenRouter unchanged. A direct call makes no engine call, has no recommendation id, and sends no telemetry. Settings and parameter normalization are not applied. This matches the SDK rule ("don't route, just call it").

The grammar lives in `crates/maverick-core/src/alias.rs` and is shared by the proxy and the four SDK cores. New conformance vectors, `crates/sdk-conformance/vectors/alias.v2.json`, replace `alias.v1.json`.

What changes from Go: Go parses `maverick/auto|quality|cost` and `maverick/<pool>:<mode>`, maps `auto` to `balanced`, and the Go proxy rejects concrete model names with `400`. D14's suffix is gone.

## 4. Fail-open on the proxy (D46)

The proxy uses the `maverick-core` fail-open ladder (`crates/maverick-core/src/failopen.rs`) that the SDK cores use.

1. The engine answers: route as today.
2. The engine fails (timeout, connect error, 5xx, or a malformed response): use the last good ranked list for the same tenant key and route (pool and variant), held in memory, if it is younger than `MAVERICK_PROXY_LKG_MAX_AGE` (default 24 h).
3. No usable last good list: use the surface's configured default model (`MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES`, `MAVERICK_PROXY_DEFAULT_MODEL_CHAT`; unset by default), called as a direct model (§3).
4. Nothing left: `503 engine_unavailable`, message `the engine is unavailable and no fallback model is configured`, with `Retry-After: 5`.

Privacy outranks fail-open (G14 as written today for SDKs):

- `422 zero_retention_requires_declared` and `409 no_compliant_columns` are terminal. They purge that tenant's last good lists and are relayed.
- For a zero-retention tenant, step 2 uses only entries stamped `retention_mode: zero`, and step 3 is allowed only when the default model is configured as verified zero retention (`MAVERICK_PROXY_DEFAULT_MODEL_ZERO_RETENTION=true`). Otherwise the request fails closed with the step 4 error.
- The engine's `401` and `403 tenant_suspended` are never fail-open triggers. They keep today's mapping (D13).

A request served from steps 2 or 3 carries `X-Maverick-Fallback: last_known_good` or `X-Maverick-Fallback: default_model`. Telemetry for a step 2 request uses the cached recommendation id. Step 3 sends no telemetry.

The last good store is in memory only (the proxy has no database), bounded (default 10,000 entries, least recently used evicted), and per replica. Each replica fills its own store from normal traffic.

## 5. Production operations

- **Rate limit.** The default `MAVERICK_PROXY_RATELIMIT_RPM` becomes 600 (from 60). Per-tenant limits arrive with the tenant `rpm` limit in v1.1 §4.2; until then the env value applies to every tenant key.
- **Health.** `GET /up` stays the liveness check. It does not call the engine, so an engine outage does not take proxy replicas out of rotation while fail-open serves traffic.
- **Replicas.** The proxy is stateless apart from the in-memory last good store. Kamal can run more than one replica behind its proxy.
- **Service level.** The proxy gets an availability target and alerts in `docs/operations.md`, like the engine API. The runbook (`docs/runbook-go-live.md`) deploys it as a production service.
- **Metrics.** A counter `maverick_proxy_fallbacks_total` by `step` (`last_known_good`, `default_model`, `unavailable`). No tenant, key, or model in labels.

## 6. Guarantees

| Guarantee | Effect |
|---|---|
| G1 | Unchanged in the engine. |
| G2, G3 | Routes still use the tenant's pools. A direct model is the caller's explicit choice and never enters a pool ranking. |
| G5 | Unchanged. The last good store holds ranked lists, never prompts. The zero-retention fail-open rules hold. |
| G13 | Unchanged. Telemetry goes through `/v1/telemetry`. |
| G14 | The proxy exemption is removed. The proxy follows the same ladder and privacy rule as the SDK cores. |
| No MM keys for customer traffic | Unchanged. Direct and fallback calls use the caller's credentials only. |

## 7. Proposed `DIVERGENCES.md` rows

| # | Area | Go behavior (oracle) | Rust expected | Plan / spec ref |
|---|---|---|---|---|
| D45 | Model name grammar | `ParseAlias` accepts `maverick/auto`, `maverick/quality`, `maverick/cost`, and `maverick/<pool>:<mode>`; `auto` is `balanced`; the proxy rejects any concrete model name with `400` (reference Part 6 §5) | OpenRouter-style: `maverick/<route>[:<variant>]`, route `auto` is the default pool, any other route is a pool, variants `quality`, `balanced`, `cost`, no variant means `quality`. A non-`maverick/` string is a direct model: `<author>/<slug>` or a bare provider id, sent to the surface's native provider (native author prefix removed; other authors `400`; OpenRouter keys pass it unchanged), with no engine call and no telemetry. Pools cannot be named `auto` or contain `:`. The settings opt-out is the header `X-Maverick-Settings: caller`, not a suffix (replaces the D14 suffix). Vectors `alias.v2.json` | `plans/production-proxy-plan.md` §3; spec §7.4, §8.1, §9.7; owner decision 2026-09-30 |
| D46 | Proxy fail-open | The proxy is exempt from G14. Any engine failure is `502 engine_error` | The proxy runs the core fail-open ladder: in-memory last good list per tenant key and route (max age 24 h), then a per-surface default model, then `503 engine_unavailable` with `Retry-After`. Zero-retention codes are terminal; zero-retention tenants use only `zero` entries or a default marked zero retention. `401` and `403` keep the D13 mapping. `X-Maverick-Fallback` marks fallback responses | `plans/production-proxy-plan.md` §4; spec §1.1 G14, §8.1; owner decision 2026-09-30 |

D14's "Rust expected" cell changes to name the header instead of the suffix.

## 8. Removing the label

Every occurrence goes. Measured on 2026-09-30: about 40 files. The largest are `plans/master-plan.md`, `plans/model-maverick-integration-surfaces-plan.md`, both specs, `parity/DIVERGENCES.md`, `README.md`, `docs/features/proxy.md`, and the crate docs in `crates/maverick-proxy`, `crates/maverick-core`, `crates/acceptance`, `crates/e2e`, and `crates/sdk-conformance`.

- "`tr[i]al` proxy" becomes "the proxy". The Track 3 heading (currently "`tr[i]al` proxy (the funnel)") becomes "Track 3: the proxy (base-URL integration)".
- The spec §8.1 heading ("`Tr[i]al` proxy behavior") becomes "Proxy behavior". Every reference to that heading moves with it, including the `source` text in `crates/sdk-conformance/vectors/normalize.d29.json` (metadata only, no vector changes).
- The architecture page for the proxy (`docs/architecture/`, currently named after the label) becomes `docs/architecture/proxy.md`. Links are updated.
- Text that states the old positioning ("demo door", "never the production data plane", "no production SLA", "exempt by positioning") is replaced with the production statements in §4 and §5, not just reworded.
- **Billing is not engine business.** `specs/model-maverick-v1.1-spec.md` describes the control plane (the Rails shell). Its plans, prices, subscription states, and any free starting period are control-plane business logic. They change outside the engine, and the engine never learns them. The engine's whole tenant contract is a status (`active` or `suspended`, set through `POST /internal/v1/tenants/{id}/suspend` and `/activate`) and numeric limits (`PUT /internal/v1/tenants/{id}/limits`). The control plane maps a plan to those limits. A parity test already shows that the engine rejects a `plan` field on tenant creation (`crates/maverick-api/tests/internal.rs`, `TestInternalTenantCreateValidation`). Whether the control plane keeps a free starting period is the control plane's decision, tracked outside the engine work. Because that spec is a control-plane document, the label check skips it.
- **Guard for the engine boundary.** A second CI check fails the build if `git grep -i -I -E 'stripe|billing|past_due|subscription_status'` finds a match in engine source and schema (`crates/*/src`, `crates/*/migrations`). Test data is out of scope: the conformance scenarios use "You are a billing assistant." as a sample customer prompt. The v1 spec §1.2 non-goals gains one line: the engine has no plans, prices, billing, or subscription state; it knows tenant status and numeric limits only.
- Two matches are not about Maverick: a competitor fact in a private research note ("no free evaluation period" is the rewording) and the OpenRouter sentence in `docs/features/proxy.md` ("for OpenRouter testing").
- Open beads are updated. Closed beads are history and stay as they are.
- A CI check in `scripts/ci.sh` fails the build if `git grep -i -I 'tr[i]al'` finds a match in tracked files outside `.beads/` and the control-plane spec (`specs/model-maverick-v1.1-spec.md`). The check writes the pattern, not the word, so it does not match itself. Measured on 2026-09-30 the pattern matches the label itself (146 times), its plural (3), and one `…ing` form (the v1.1 billing state below). Every match is reworded or removed.

The pure wording change lands first, on its own, so reviewers can see that it changes no behavior.

## 9. Open questions

1. **Direct models without the `<author>/` form.** Claude Code sends bare ids such as `claude-opus-5-5` when the caller has not set a `maverick/*` model. Recommendation: pass them through (§3), so pointing Claude Code at the proxy never breaks it. Routing happens only when the caller chooses a `maverick/*` model.
2. **Engine API default mode.** `POST /v1/recommend` without `mode` is `balanced` (Go behavior). `maverick/auto` now means `quality`. Recommendation: change the API default to `quality` too, as a separate row, so every path means "best model" by default. This also answers open question 7 in `plans/conversation-affinity-plan.md`.
3. **Rate limit default.** 600 requests per minute per tenant key. Recommendation: accept, and move to per-tenant limits with v1.1.

## 10. Build order

1. Remove the label (wording only) plus the CI check.
2. Adopt D45 and D46 into the spec, `DIVERGENCES.md`, `docs/api.md`, and the master plan §10 (record the decisions).
3. In parallel: the alias grammar v2 in `maverick-core` and the vectors; pool name validation in the engine; the proxy fail-open ladder.
4. The proxy wiring for direct models and the settings header.
5. Production operations: rate limit default, metric, runbook, and operations docs.
6. Acceptance tests through the real proxy and engine, and the Claude Code launcher script.

## 11. Tests

- Alias vectors: every row of §3, every error, case sensitivity, pool names with `/`, a pool route that looks like an author (`maverick/openai`), and the old `?settings=caller` suffix now being `malformed_alias`.
- Engine: `PUT /v1/config/pools` rejects a pool named `auto` and a name with `:`.
- Proxy: a direct model on each surface, with and without the native author prefix, a foreign author (`400`), an OpenRouter key (unchanged string), no engine call and no telemetry for a direct model; `X-Maverick-Settings` values and upstream stripping.
- Fail-open: each ladder step; LKG expiry; LRU bound; zero-retention tenant with and without zero entries and with and without a verified default; `401` and `403` are not fail-open triggers; `X-Maverick-Fallback`; the metric. A conformance scenario for the proxy engine outage (warm and cold), shared with the SDKs.
- Acceptance: an agent with `maverick/auto` routes to the quality pick; `maverick/auto:cost` routes to the cost pick; with the engine stopped, the same agent keeps working from the last good list.
- The label CI check and the engine-boundary check each fail on a planted occurrence.
