# TypeScript test agent

A zero-dependency client that exercises the engine the way a real SDK will
(see `plans/model-maverick-sdk-v0-plan.md` §2): recommend → execute the chain
with fallback → auto-report telemetry → verify the derived outcomes. Useful as
a live smoke test and as executable documentation of the SDK-core behaviors.

Requires Node ≥ 23.6 (native TypeScript type stripping) and a running engine
(`docs/onboarding.md` §§1–8 for the stack, tenant, and key).

```sh
MAVERICK_API_KEY=mk_live_... node agent.ts
```

Options (env):

| Var | Effect |
|---|---|
| `MAVERICK_BASE_URL` | engine public listener, default `http://localhost:8080` |
| `MODELMAVERICK_EXECUTION` | `direct` (default) or `openrouter` — openrouter mode uses each entry's `execution_aliases` slug as the wire model and stamps `endpoint_used` |
| `MODELMAVERICK_TELEMETRY` | `off` sends zero telemetry bytes (G13 opt-out) |
| `ANTHROPIC_API_KEY` | when set, the happy-path scenario makes one real Anthropic call instead of simulating |

What it checks: batch accepted/dropped counts (including a deliberate garbage
item), every derivation rule the scenarios exercise (`success`,
`provider_error` + fallback, `parse_failure`, `latency_exceeded`),
`endpoint_used` matching the execution mode, and that `POST /v1/outcomes`
returns 404 (the ceremony is dead). Exits non-zero on any failure.
