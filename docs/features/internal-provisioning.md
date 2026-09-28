# Internal provisioning API — the Rails seam

*A root-scoped control plane on a private listener: everything a future
product shell needs to operate the engine, built before that shell exists.*

## The problem it solves

The product's identity and commerce layer — users, orgs, sessions, billing,
a dashboard — is deliberately a separate Rails application (v1.1) sharing
**no database** with the engine. That decision only works if the boundary
between the two is designed up front: a provisioning surface the shell can
drive without reaching into the engine's tables, and that the engine's hot
path never depends on in return. Building the seam in v1 — before Rails
exists — is what keeps v1.1 an *addition* rather than a surgery. Until
then, the same seam is how the human operator provisions tenants by hand.

## How it works

- **Private by topology.** The seam lives on the internal listener, which
  the deployment proxy never exposes — it is reachable only over the
  host's private network. Authentication is by operator token with
  rotation-friendly labels ([Tenancy & auth](tenancy-auth.md)); callers are
  root, with the label recorded as the acting party.
- **The provisioning vocabulary:** create a tenant (which arrives usable —
  generated fingerprint key, a default pool of all active columns, open
  constraints); suspend and reactivate; issue an API key (raw key returned
  exactly once) and revoke; write pools, weights, and constraints with the
  **same validation as the tenant-facing config endpoints** — G4 parity, so
  an operator or the future shell cannot create a configuration a tenant
  couldn't; read the [alert](alerts-delivery.md) feed; administer catalog
  columns (creation, status, pricing — with the
  [retirement guard](model-catalog.md) enforced); and activate a loaded
  corpus version ([Capability matrix](capability-matrix.md)).
- **Every write leaves a trail.** All provisioning writes emit structured
  audit log entries naming the actor label — the operator's actions are as
  accountable as tenants' requests.
- **The dependency points one way.** Rails will call the engine to
  provision; the recommend path never calls Rails. An outage in the product
  shell cannot touch recommendation traffic. One org maps to one engine
  tenant by identifier — that mapping is the entire integration contract.
- Diagnostics for the same private audience — the detailed health endpoint —
  share this listener ([Observability](observability.md)).

## Dependencies

- Auth model: [Tenancy & auth](tenancy-auth.md). Administers: tenants/keys, [Policy engine](policy-engine.md) config (shared G4 validation), [Model catalog](model-catalog.md) columns, [Capability matrix](capability-matrix.md) corpus activation; exposes [Alerts](alerts-delivery.md).
- Full endpoint reference: [API reference — internal API](../api.md); hands-on walkthrough: [Onboarding](../onboarding.md).
- The v1.1 Rails shell built on this seam: [`specs/model-maverick-v1.1-spec.md`](../../specs/model-maverick-v1.1-spec.md) (product sketch in [`plans/model-maverick-v1-plan.md`](../../plans/model-maverick-v1-plan.md)).
- Normative contract: [spec §13](../../specs/model-maverick-v1-spec.md).
