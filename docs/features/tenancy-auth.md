# Tenancy and authentication

*Multi-tenant from day one — but tenants are configuration, not users.
Identity, billing, and self-service belong to the v1.1 product shell.*

## The problem it solves

The engine must isolate tenants from its first request: separate pools,
weights, constraints, caches, histories, and prompt fingerprint keys. But
building a user system — signup, sessions, orgs, billing — inside a Go
recommendation engine would weld product concerns onto the hot path and
have to be ripped out when the real product shell arrives. The v1 answer:
the engine knows *tenants* (a name, a status, a key ring, a config set) and
deliberately does not know *people*. People are the Rails application's
problem (v1.1), across [one seam](internal-provisioning.md).

## How it works

- **Tenants as config.** A tenant is created through the internal API and
  carries: a per-tenant HMAC key for prompt fingerprints (generated at
  creation, never leaves the engine — see
  [Privacy](privacy-capture.md)), a default pool, weights, constraints
  (including the zero-retention flag), and a status. Suspension turns a
  tenant's keys off with a distinct error, without touching its data.
- **API keys.** Keys are prefixed (`mk_live_`), high-entropy, shown exactly
  once at issue time, and stored only as a hash with the last four
  characters kept for identification. Verification is by hash lookup;
  revocation is immediate. Every request's tenant and key identities ride
  into logs and [decision traces](history-audit.md).
- **Rate limiting.** Fixed-window per key with standard limit/remaining/
  reset headers and a retry-after on rejection — protecting the hot path
  from a single runaway integration.
- **Two listeners, two trust models.** The public listener speaks tenant
  API keys and is the only surface the deployment proxy exposes. The
  internal listener speaks operator tokens — a comma-separated,
  label-tagged list to allow rotation, compared in constant time, with the
  label recorded as the acting party in audit logs. Internal callers are
  root; there is no tenant scoping inside the seam. (As built, the full
  `label:secret` string is the bearer token.)
- **What is deliberately absent:** users, sessions, passwords, orgs,
  billing, email — all of it lands in the Rails application (v1.1), which
  provisions engine tenants through the
  [internal API](internal-provisioning.md). One org maps to one engine
  tenant by identifier; the two systems share no database.

## Dependencies

- Tenant configuration is enforced by the [Policy engine](policy-engine.md); the HMAC key underpins [Privacy](privacy-capture.md) and the [Classification](classification.md) cache.
- Tenant/key lifecycle is driven through the [Internal provisioning API](internal-provisioning.md); walkthrough in [Onboarding](../onboarding.md).
- Auth and rate-limit wire contract: [API reference](../api.md).
- The v1.1 Rails division of labor: [`plans/model-maverick-v1-plan.md`](../../plans/model-maverick-v1-plan.md).
- Normative contract: [spec §9.1, §13](../../specs/model-maverick-v1-spec.md).
