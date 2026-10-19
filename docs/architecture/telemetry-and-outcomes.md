# Telemetry and outcomes

Outcome data is how the engine learns what happened to a recommendation in production. It arrives through one path only.

## One path in

Outcome data comes only from MM components: the proxy and the customer SDKs, through `POST /v1/telemetry`. There is no `POST /v1/outcomes`. A caller cannot inject outcomes directly. This keeps the outcome signal trustworthy: every item was produced by code MM controls, alongside a real relayed request.

## What an item is

The producer emits one telemetry item per dispatched attempt in a chain walk, in `maverick-core::telemetry`. Each item carries:

- `recommendation_id`: the id the engine returned for this request.
- `model_used` and `endpoint_used`: the model the attempt called and how (`direct` or `openrouter`).
- `status`: the HTTP status, or `0` for a transport failure.
- `error_class`: `timeout` or `connect_error` when the attempt failed at the transport level.
- `latency_ms`: the time until the response headers arrived.
- `tokens_in` and `tokens_out`: token counts, parsed from the settled 2xx body of the relayed attempt only.
- `settings_applied` (D14) and `check_pass` (D16, three-valued: `None` when no check ran, never defaulted).

A batch is `{"items":[...]}`, sent with `Authorization: Bearer <tenant key>`.

## The batcher

The producer batches items per tenant key and sends them fire-and-forget, so telemetry never slows a customer request:

- It flushes on an interval (default 10 s) or when a batch reaches its size (default 20 items), and it flushes once more at shutdown.
- A request that dispatched no attempt has no items and sends nothing.
- A tenant that opted out (D39, the engine's `telemetry: false` on the recommendation) sends nothing, and the request is still routed. The proxy's OTel record skips that request too (D43).
- Telemetry off (`MAVERICK_PROXY_TELEMETRY=off`) means the producer has no task and no connection: zero bytes.

## What the engine does with it

The engine records the outcomes and uses them for its production-signal views and alerting. A tenant reads its outcomes through `GET /v1/history/outcomes`. Its `outcome` filter accepts Go's six values plus `completed_unchecked`, the stored outcome of a 2xx with no check (D16), so a caller can filter on it (D40). The nightly rankings themselves come from the benchmark grades, not from telemetry; telemetry is the live-traffic signal that sits alongside the benchmark. See [Nightly benchmark](nightly-benchmark.md).

## Where this is verified

The conformance scenarios (`sdk-conformance`, groups 2, 7, 8, and 12) cover the batch shape, the flush interval and max-batch timing, the D39 opt-out, and the telemetry-off zero-bytes rule. The `telemetry.v1` test vector replays the item encoding against the engine's rules.
