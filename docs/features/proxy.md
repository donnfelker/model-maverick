# Trial Proxy

The trial proxy is a demo path for existing OpenAI and Anthropic clients. A
caller points its base URL at the proxy, uses a `maverick/*` model alias, and
keeps the provider wire format it already sends. The proxy asks the engine for a
ranked chain, tries the entries in order, and reports the attempts through
`/v1/telemetry`.

The engine still does not execute inference. The proxy is the only component
that calls providers, and it uses the caller credential or the self-hosted
operator keys.

## Modes

`MAVERICK_PROXY_MODE=self-hosted` is the local and single-tenant mode. The proxy
uses one `MAVERICK_API_KEY` for engine calls. It may also read provider keys
from its own environment as fallback credentials.

`MAVERICK_PROXY_MODE=hosted` is the shared trial mode. Each request must carry a
tenant key in `X-Maverick-API-Key`. Hosted mode does not read provider keys from
the proxy process. The caller must send its provider credential on the request.

## Claude Code

Claude Code uses the Anthropic Messages surface. The launcher is:

```bash
./scripts/claude-via-maverick.sh --dry-run
./scripts/claude-via-maverick.sh
```

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
Use an OpenAI-compatible client for OpenRouter trials.

## Fences

- No schema translation. Send OpenAI chat payloads to
  `/v1/chat/completions`, and Anthropic Messages payloads to `/v1/messages`.
- The Messages surface executes Anthropic entries only.
- Hosted mode never falls back to provider keys owned by Model Maverick.
- `X-Maverick-API-Key` is stripped before any upstream provider call.
- The default hosted rate limit is 60 requests per minute per tenant key.
- The default body cap is 32 MiB.
- This path has no production SLA. Production traffic should use an SDK or a
  gateway plugin once those exist.

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

A graduation-link response header could point trial users to the production
integration path. The owner removed it from v1 on 2026-09-28 as premature
because SDKs and public docs do not exist yet. Revisit when they do.

## References

- [API conventions](../api.md)
- [Model catalog](model-catalog.md)
- [Tenancy and auth](tenancy-auth.md)
- [History and audit](history-audit.md)
- Normative proxy contract: `specs/model-maverick-v1-spec.md` §8.1
- Claude Code environment variables: <https://code.claude.com/docs/en/env-vars>
