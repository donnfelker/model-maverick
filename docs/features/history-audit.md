# History, decision traces, and outcomes

*Every recommendation is settleable from stored data alone: who was
considered, who won, who was excluded and exactly why.*

## The problem it solves

The defining support conversation for a router is: "your system keeps
returning a small model where a large one was deserved." Without stored
evidence, that dispute becomes reproduce-and-guess — re-run the prompt,
hope the classifier behaves the same, argue about what the aggregates
probably were at the time. Guarantee G6 exists to kill that failure mode:
the answer to "why did it pick Haiku over Opus" must be retrievable, not
reconstructable.

## How it works

- **The decision trace.** Every recommend request persists a complete
  record: the classification, the request (prompt text excluded — see
  [Privacy](privacy-capture.md)), the response exactly as returned, and
  every column considered. Survivors carry the numbers that ranked them
  (quality, pass rate, cost, latency, weighted value, leader status, all
  flags); exclusions carry a reason from a **closed enum** (not in pool,
  not in allowlist, which standing constraint, retired, no data). Opus is
  therefore either a ranked survivor with the numbers that placed it
  second, or an exclusion with the reason it never competed. No third
  possibility exists.
- **Query surfaces.** Tenants can read: the aggregate time series per cell
  with regression markers (how a column's score moved), their
  recommendation log filterable by prompt fingerprint (how often did this
  exact prompt arrive, and how was it classified each time), any single
  full trace by id, reported outcomes, and the
  [calibration](calibration.md) series. All list endpoints paginate by
  opaque cursor.
- **Outcomes close the loop, automatically.** MM-authored SDK, gateway
  plugin, and trial-proxy components report what actually happened against
  a recommendation id as an invisible side effect of routing the call —
  success, parse failure, latency exceeded, provider error — including
  which endpoint served it. No public surface ever asks a caller to report
  this by hand (G13, zero ceremony): a component with telemetry disabled
  simply never reports, and that tenant's outcome blending has no data.
  Multiple outcomes per recommendation are expected (retries, escalation
  chains). v1 deliberately *collects and exposes* outcomes without feeding
  them into scores; blending observed signal into rankings is a v1.1
  decision with real gaming/weighting questions attached. Outcome ids are
  tenant-scoped with no cross-tenant existence oracle.
- **Retention.** Recommendations, traces, and outcomes prune daily past the
  tenant's retention setting. Aggregates and calibration are kept
  indefinitely — they are the product, not exhaust.

## Dependencies

- Written by [Recommendation](recommendation.md); the trace embeds the [Classification](classification.md) block and [Policy engine](policy-engine.md) exclusion reasons.
- Prompt exclusion and fingerprinting rules: [Privacy & debug capture](privacy-capture.md).
- Pruning is a daily job: [Jobs & scheduling](jobs-scheduling.md).
- The misclassification-fix workflow this enables: [Classification — the durable fix](classification.md).
- Endpoint shapes and pagination: [API reference](../api.md).
- Normative contract: [spec §9.5, §9.6, §10.2, §10.3](../../specs/model-maverick-v1-spec.md).
