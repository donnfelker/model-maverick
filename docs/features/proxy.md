# Proxy

The proxy is the base-URL integration for existing OpenAI and Anthropic
clients. A caller points its base URL at the proxy, uses a `maverick/*` model name, and
keeps the provider wire format it already sends. The proxy asks the engine for a
ranked chain, tries the entries in order, and reports the attempts through
`/v1/telemetry`. Any other model name is a direct call, so pointing a client at the
proxy never breaks it.

The engine still does not execute inference. The proxy is the only component
that calls providers, and it uses the caller credential or the self-hosted
operator keys.

## Model names

The names follow OpenRouter's model ids (D45):

| Model | What the proxy does |
|---|---|
| `maverick/auto` | Routes over the tenant's default pool in `quality` mode |
| `maverick/auto:balanced`, `maverick/auto:cost` | Routes over the default pool in that mode |
| `maverick/<pool>`, `maverick/<pool>:<variant>` | Routes over the named pool (`quality` when there is no variant) |
| `anthropic/claude-opus-5-5` on `/v1/messages` | Calls `claude-opus-5-5` at Anthropic directly |
| `claude-opus-5-5` | Calls that model on the surface's own provider directly |

A direct call skips the engine. It has no recommendation id and sends no telemetry.
Claude Code sends bare ids such as `claude-opus-5-5` unless you set a `maverick/*`
model, so its default model is a direct call. A model from another provider than the
surface's own is `400 invalid_request`, because the proxy does not translate schemas.

`maverick/quality` and `maverick/cost` name pools called `quality` and `cost`. Use
`maverick/auto:cost` for cost mode on the default pool. A malformed `maverick/*` name is
`400 invalid_model_alias`. The full grammar is in
[API conventions](../api.md#sdk-integration-conventions).

The proxy applies the recommended entry's settings (reasoning effort, thinking mode).
Send `X-Maverick-Settings: caller` to keep your own. The proxy strips that header
before it calls a provider.

## Fail-open

An engine outage does not break a routed call (D46). When the engine times out, cannot
be reached, answers 5xx, or sends a malformed answer, the proxy uses:

1. The last good ranked list for the same tenant key and route, if it is younger than
   `MAVERICK_PROXY_LKG_MAX_AGE` (default 24 h).
2. Else the surface's default model, `MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES` or
   `MAVERICK_PROXY_DEFAULT_MODEL_CHAT` (unset by default), as a direct call.
3. Else `503 engine_unavailable` with `Retry-After: 5`.

A fallback response carries `X-Maverick-Fallback: last_known_good` or
`X-Maverick-Fallback: default_model`. The last good store is in memory, per replica,
and bounded. It holds ranked lists, never prompts.

Privacy comes first. The engine's `422 zero_retention_requires_declared` and
`409 no_compliant_columns` are relayed, never fallen open. For a zero-retention tenant,
the proxy uses only cached entries marked `zero`, and the default model only when
`MAVERICK_PROXY_DEFAULT_MODEL_ZERO_RETENTION=true`. Otherwise it answers the `503`. An
engine `401` or `403` is never a fallback trigger.

## Modes

`MAVERICK_PROXY_MODE=self-hosted` is the local and single-tenant mode. The proxy
uses one `MAVERICK_API_KEY` for engine calls. It may also read provider keys
from its own environment as fallback credentials.

`MAVERICK_PROXY_MODE=hosted` is the shared hosted mode. Each request must carry a
tenant key in `X-Maverick-API-Key`. Hosted mode does not read provider keys from
the proxy process. The caller must send its provider credential on the request.

## Claude Code

Claude Code uses the Anthropic Messages surface. The launcher is:

```bash
./scripts/claude-via-maverick.sh --dry-run
./scripts/claude-via-maverick.sh
```

The launcher selects `maverick/auto` for the main model and `maverick/auto:cost`
for the fast model. Leaving Claude Code's default bare model id sends that model
directly to Anthropic, with no engine call or recommendation telemetry.

For hosted mode, put this in `.env.maverick.local`:

```bash
MAVERICK_PROXY_MODE=hosted
MAVERICK_PROXY_URL=https://proxy.example.test
MAVERICK_TENANT_KEY=mk_live_...
ANTHROPIC_API_KEY=sk-ant-...
```

The script sets `ANTHROPIC_BASE_URL` to the proxy and adds
`X-Maverick-API-Key` through `ANTHROPIC_CUSTOM_HEADERS`. It never prints key
values. The `ANTHROPIC_API_KEY` value remains the caller's Anthropic credential
for direct Anthropic attempts.

For self-hosted mode, set:

```bash
MAVERICK_PROXY_MODE=self-hosted
MAVERICK_PROXY_URL=http://localhost:8090
MAVERICK_ENGINE_URL=http://localhost:8080
```

You may set `ANTHROPIC_API_KEY` to send a caller Anthropic key, or rely on
provider keys loaded by the self-hosted proxy.

Claude Code reads `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`, and
`ANTHROPIC_CUSTOM_HEADERS`; see the Claude Code environment variable reference.

## Manual Check

Use dry-run first:

```bash
./scripts/claude-via-maverick.sh --dry-run -p "."
```

For a local self-hosted check, start the engine and proxy with the fake provider
path from `docs/onboarding.md`. Then run:

```bash
./scripts/claude-via-maverick.sh -p "."
```

The script should print the proxy URL, mode, and aliases. It must not print the
tenant key or provider key.

## Execution

Direct execution is the default. The chat-completions surface can run OpenAI and
OpenAI-compatible entries. The Messages surface executes Anthropic entries only.
The proxy does not translate schemas between providers.

OpenRouter is opt-in on the chat-completions surface. In hosted mode, a caller
Bearer key that starts with `sk-or-` selects OpenRouter execution. The proxy uses
the catalog entry's OpenRouter alias as the upstream model. For a zero-retention
tenant, the alias must have valid zero-retention evidence, or the proxy returns
`409 no_compliant_columns`.

Claude Code uses the Messages surface, so the launcher refuses `sk-or-` keys.
Use an OpenAI-compatible client for OpenRouter testing.

## Fences

- No schema translation. Send OpenAI chat payloads to
  `/v1/chat/completions`, and Anthropic Messages payloads to `/v1/messages`.
- The Messages surface executes Anthropic entries only.
- Hosted mode never falls back to provider keys owned by Model Maverick.
- `X-Maverick-API-Key` is stripped before any upstream provider call.
- The default rate limit is 600 requests per minute per tenant key
  (`MAVERICK_PROXY_RATELIMIT_RPM`). Direct calls count against it.
- The default body cap is 32 MiB.
- The proxy is a production path. Its fail-open ladder is in spec §8.1 (D46).
  Its availability target, alerts, and replicas are in `docs/operations.md`
  "The proxy in production".

## Telemetry

Each attempt reports through `/v1/telemetry`, unless telemetry is disabled.
When a tenant has `telemetry_opt_out`, the engine returns `telemetry: false`,
and the proxy sends no telemetry for that tenant. Operators can also set
`MAVERICK_PROXY_TELEMETRY=off` to send no proxy telemetry at all.

OTel logs and metrics are separate operator observability. They are enabled only
when the proxy has `MAVERICK_PROXY_OTLP_ENDPOINT`. An opted-out tenant's requests
export no OTel record either: no log record, and the request counter does not
count them (D43).

## Nice To Have (Not Planned)

A graduation-link response header was once planned to point proxy users to
another integration path. It is moot: the proxy is a production path
(`plans/production-proxy-plan.md`, owner decision 2026-09-30).

## References

- [API conventions](../api.md)
- [Model catalog](model-catalog.md)
- [Tenancy and auth](tenancy-auth.md)
- [History and audit](history-audit.md)
- Normative proxy contract: `specs/model-maverick-v1-spec.md` §8.1
- Claude Code environment variables: <https://code.claude.com/docs/en/env-vars>
