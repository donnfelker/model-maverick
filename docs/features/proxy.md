# The trial proxy (`cmd/maverick-proxy`)

*Zero-code-change routing: point an existing OpenAI or Anthropic client's base
URL at the proxy instead of the provider, and it gets automatic model
selection, execution, and telemetry.*

## The problem it solves

The production integration path is an SDK that a team deliberately adopts
(v1.1 Track 1) — but before anyone adopts anything, they need to *see the
product work* against code they already have, without writing an integration
first. A base-URL swap is the lowest-friction demonstration available: no new
package, no new client code, just a different host and a `maverick/*` model
name. That demonstration has to behave like a real integration, not a toy —
real fallback on transient failure, real streaming, real telemetry — or it
proves nothing about the product it's demonstrating.

## How it works

- **Two surfaces, one behavior.** The proxy speaks OpenAI chat-completions
  (`POST /v1/chat/completions`) and the Anthropic Messages schema
  (`POST /v1/messages`) — the latter a deliberate extension past the original
  OpenAI-only trial-proxy design so Anthropic-native tooling (e.g. Claude
  Code via `ANTHROPIC_BASE_URL`) works unmodified. Both surfaces resolve the
  same `maverick/auto | quality | cost | <pool>:<mode>` alias grammar used
  everywhere else in the [SDK integration conventions](../api.md), asking the
  engine for a recommendation and walking its fallback chain.
- **A real chain executor, not a single call.** Each chain entry is tried in
  order; a transport failure or an upstream `429`/`5xx` advances to the next
  entry, a semantic `4xx` is relayed verbatim (the request was bad on every
  model, not just this one), and a `2xx` — streaming or not — is relayed the
  moment it's seen, so fallback only ever happens before the caller sees a
  byte. A chain that never produces an executable attempt fails closed
  (`502 no_executable_candidates`) rather than guessing.
- **BYOK where it's safe, the proxy's own key otherwise.** When a chain entry
  lands on the surface's native provider (Anthropic for Messages, OpenAI for
  chat-completions) and the caller sent its own credential, that credential
  is forwarded as-is and never inspected or stored. Every other entry — a
  cross-provider pick, or the native provider when the caller sent no
  credential — falls back to the proxy's own configured provider key; an
  entry with no resolvable credential anywhere is skipped without counting as
  an attempt.
- **Headers and query strings pass through by default.** Real client tooling
  sends headers the proxy can't enumerate in advance (Claude Code's
  `anthropic-beta` authorizing beta body fields, for one) — an allowlist
  would silently break them. Everything flows through except hop-by-hop
  headers and the two credential headers, which the resolved key replaces.
  The client's query string travels verbatim for the same reason.
- **Narrow, honest parameter normalization.** A client configured for one
  model chooses parameter values for *that* model, not whatever the engine
  routes to — a live finding was Claude Code sending an Opus-tier reasoning
  effort that a routed Sonnet-tier model rejected outright. v0 ships exactly
  one normalization per surface for exactly that failure; a catalog-driven,
  per-model version of the fix is future work, not solved generally here.
- **Zero-ceremony telemetry and optional observability.** Every dispatched
  attempt reports to `POST /v1/telemetry` automatically, same as any
  MM-authored component (G13) — the demonstration closes its own feedback
  loop without anyone wiring it up. Separately, and only when an operator
  points it at a collector, the proxy emits its own OTel logs and metrics
  (cold-cell rate, classifier-fallback rate) — fail-open, so a dead collector
  never touches request latency.
- **Deliberately narrow (v0).** No OpenAI↔Anthropic schema translation, so
  the Messages surface only ever executes Anthropic chain entries. Direct
  execution only — the catalog's `execution_aliases` (see
  [Model catalog](model-catalog.md)) are never read here. No production SLA,
  rate limiting, or request-size cap beyond the stdlib's defaults. This is a
  trial demonstration; production traffic graduates to a client SDK or a
  gateway plugin.

## Dependencies

- A separate binary and a separate deployable from `maverick` — no database,
  no migrations, no shared runtime with the engine.
- Drives [Recommendation](recommendation.md) exactly like any external caller
  (tenant API key auth — see [Tenancy & auth](tenancy-auth.md)) and closes the
  loop through the same automatic-telemetry path described in
  [History & audit](history-audit.md).
- Shares the alias grammar and the correlation-header convention with every
  other MM-authored component: [API reference — SDK integration
  conventions](../api.md).
- Full environment reference, surfaces, header/key-resolution rules, and the
  OTel event/metric shapes: [`cmd/maverick-proxy/README.md`](../../cmd/maverick-proxy/README.md).
- Normative contract: `plans/model-maverick-integration-surfaces-plan.md` §4
  (Track 3); the alias grammar and telemetry contract originate in the same
  plan's §1.
