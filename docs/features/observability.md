# Observability and health

*Structured logs, two health endpoints, and the alert ledger — a deliberately
small operational surface for a single-node system.*

## The problem it solves

v1 is a single-node, single-operator system; a metrics stack (Prometheus,
dashboards, alert managers) would be more infrastructure than the thing it
monitors. But the system still owes its operator two answers on demand —
"is it up?" for the deployment machinery, and "what exactly is unhealthy?"
for the human — and owes every incident a reconstructible paper trail.

## How it works

- **Structured logs are the narrative.** JSON logs to stdout. Every request
  logs method, path, status, duration, tenant and key identity, and the
  recommendation id when one was created — enough to join any log line to
  its [decision trace](history-audit.md). Every batch logs its id,
  projected versus actual spend, run success/error counts, and duration —
  enough to audit the [spend guard](jobs-scheduling.md) from logs alone.
  Provisioning writes log with the acting operator label
  ([Internal provisioning](internal-provisioning.md)).
- **Two health answers for two audiences.** The public liveness endpoint
  answers cheaply (process up, database reachable) and is what the
  deployment health check and proxy consult. The detailed endpoint, on the
  internal listener only, reports per-component status: database
  connectivity, job-queue health (workers present, queues unpaused,
  heartbeats fresh — surfacing a silently dead worker before a missed
  batch does), provider client configuration, the active matrix version,
  and last batch times.
- **Alerts are the pager.** Anything requiring operator *action* is an
  [alert](alerts-delivery.md) — durable row plus push delivery — rather
  than a log line hoping to be noticed. Logs explain; alerts summon.
- **Metrics are deferred on purpose — for the engine.** The alerts ledger
  plus structured logs are v1's operational surface for `maverick` itself; a
  metrics endpoint on the engine is a v1.1-or-later addition once there is a
  fleet to aggregate over. The separate trial proxy (`cmd/maverick-proxy`) is
  a different case: it optionally emits its own OTel logs and metrics
  (`MAVERICK_PROXY_OTLP_ENDPOINT`) so an operator running it can graph
  routing behavior — cold-cell rate, classifier fallback rate — in whatever
  collector they already run, fail-open so a dead collector never touches
  request latency. That's a property of the proxy as a small, independently
  deployed component, not a reversal of the engine's own metrics-deferred
  stance.

## Dependencies

- Job-queue health reflects [Jobs & scheduling](jobs-scheduling.md); the detailed endpoint lives beside the [Internal provisioning API](internal-provisioning.md).
- Operator action paths: [Alerts & delivery](alerts-delivery.md) and the [Operations runbook](../operations.md) (process shapes, deployment, hot-path benchmarks).
- Health endpoint shapes: [API reference](../api.md).
- The trial proxy's own OTel emission: [`cmd/maverick-proxy/README.md`](../../cmd/maverick-proxy/README.md) — a separate deployable, not part of this endpoint or this process.
- Normative contract: [spec §9.9, §14.3](../../specs/model-maverick-v1-spec.md).
