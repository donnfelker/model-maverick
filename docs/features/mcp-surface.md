# MCP server

*Agents are first-class callers: the same recommendation surface, exposed
over the Model Context Protocol without forking behavior, auth, or audit.*

## The problem it solves

An increasing share of the traffic that needs model routing originates
inside agents, which consume tools, not REST documentation. Making agents
scrape the REST API through bespoke glue loses schema discovery and tool
semantics; building a *separate* agent surface risks the worse failure —
two surfaces that drift apart in behavior, auth, or audit, so an
MCP-originated decision becomes less accountable than a REST one.

## How it works

- **Two transports.** Streamable HTTP at a public-listener endpoint for
  production — mounted behind the *exact* public middleware chain (same
  Bearer key auth resolved to a tenant, same per-key rate limit, same body
  cap) — and a stdio transport via a CLI subcommand for local development,
  authenticated once with the same key material.
- **Three tools, thin by design:** recommend a model, get the banded policy
  table, list the model catalog. Each is a wrapper over the same internal
  services as REST and returns the same response objects. Nothing is
  reimplemented; there is no behavior to keep in sync.
- **Identical audit.** An MCP-originated recommendation writes the same
  recommendation row and full [decision trace](history-audit.md) as a REST
  one — G6 does not care which door the request came through.
- **Privacy-aware tool guidance.** Tool descriptions steer agents to the
  declared-workload form when they already know the capability: faster (no
  classifier call) and nothing leaves the session — the same privacy lever
  documented in [Classification](classification.md).

## Dependencies

- Wraps: [Recommendation](recommendation.md), the [Policy engine](policy-engine.md)'s banded policy view (built on [Scoring](scoring.md) ceilings), and the [Model catalog](model-catalog.md) listing.
- Authentication and rate limiting are shared with REST: [Tenancy & auth](tenancy-auth.md).
- Tool shapes and transport details: [API reference — MCP](../api.md).
- Normative contract: [spec §12](../../specs/model-maverick-v1-spec.md).
