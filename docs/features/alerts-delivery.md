# Alerts and delivery

*A durable alert ledger with push delivery — because a single-operator
product cannot depend on someone watching a dashboard.*

## The problem it solves

v1 is operated by one person. Regressions, spend refusals, degraded
classifiers, and pools drifting toward empty are all conditions that demand
action within hours, not at the next dashboard visit. The operational
surface therefore has to *push*. At the same time, delivery infrastructure
(SMTP, webhooks) is exactly the kind of dependency that fails — so the
alert's existence must never depend on its deliverability, and a failing
delivery must never take a request or a job down with it.

## How it works

- **The row is the record.** Every alert is first a database row: a closed
  set of kinds (regression, parse-failure rate, job failure, spend warning,
  spend refusal, pool-would-be-empty, classifier degraded), a severity
  (info/warning/critical), a machine-readable subject naming the exact
  column/cell/job/tenant, a human message, and an acknowledgement
  timestamp. The internal API exposes the feed for programmatic
  consumption ([Internal provisioning](internal-provisioning.md)).
- **Fan-out at insert.** The moment a row is written it is handed to the
  delivery fan-out: email over SMTP and an operator webhook signed with
  HMAC so the receiver can verify origin. Destinations deliver
  independently and retry independently with backoff — one sink failing or
  retrying never delays the other. Delivery is asynchronous and its failure
  logs loudly but **never blocks or fails the write that created the
  alert**. Unconfigured destinations simply mean the ledger accrues without
  push — the record is never lost.
- **Every writer delivers.** Alert rows originate in two places, and both
  fan out. Background jobs raise most kinds
  ([Jobs & scheduling](jobs-scheduling.md)). Request-time writers raise the
  time-critical ones: sustained classifier failure
  ([Classification](classification.md)), constraint drift emptying a pool
  mid-request ([Policy engine](policy-engine.md)), and a blocked retirement
  ([Model catalog](model-catalog.md)). Request-time writes couple insert to
  delivery through a notifying store, so no critical signal is ever merely
  stored-and-logged; one process-wide fan-out is shared by workers and the
  API and drained at shutdown so in-flight deliveries finish.
- **Signal quality is designed, not hoped for.** Regression alerts dedupe
  (one per cell per week unless the drop deepens); severity escalates to
  critical only when a drop crosses a quality bar of a tenant actually
  pooling the column; the parse-failure watch requires both an absolute
  rate and a multiple of the column's own trailing baseline. Alerts page
  for meaning, not motion.
- **Tenant-facing webhooks are deliberately v1.1** — v1's delivery targets
  the operator; the Rails product shell owns tenant notification.

## Dependencies

- Raised by: [Scoring](scoring.md) (regression), [Jobs & scheduling](jobs-scheduling.md) (spend, job failure, parse-failure watch, drift check), [Classification](classification.md) (degraded), [Policy engine](policy-engine.md) (request-time drift), [Model catalog](model-catalog.md) (retirement guard).
- Read via the alert feed on the [Internal provisioning API](internal-provisioning.md).
- Per-kind response runbook and delivery configuration: [Operations — alerts](../operations.md).
- Normative contract: [spec §11.1 and the alert kinds in §3](../../specs/model-maverick-v1-spec.md).
