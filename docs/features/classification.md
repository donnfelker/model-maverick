# Workload classification

*Callers can declare what their workload is — or send the raw prompt and let
a small, cached, fail-soft classifier figure it out.*

## The problem it solves

The matrix is keyed by capability and difficulty, but most callers don't
know the taxonomy and shouldn't have to learn it before getting value.
Requiring correct labels from every caller kills adoption; guessing wrong
silently routes work to the wrong models. At the same time, classifying via
a model call introduces three hazards: it adds latency to the hot path, it
can fail (and an engine whose *recommendations* fail because its
*classifier* is down has inverted its priorities), and it sends the prompt
to a provider — a privacy-relevant event that must be avoidable.

## How it works

- **Two workload forms.** A request declares its capability (plus optional
  difficulty and domain tags) — no model call, nothing leaves the engine,
  full confidence — or sends the raw prompt. Exactly one form per request.
  Declared requests with no difficulty default to the middle tier, honestly
  labeled as defaulted.
- **The prompt path.** The prompt is fingerprinted (per-tenant HMAC — see
  [Privacy](privacy-capture.md)), and the cache is consulted under (tenant,
  fingerprint, classifier version). A hit skips the model call entirely —
  repeated prompts, common in production template traffic, classify for
  free. A miss calls a small classifier column (seed: Haiku, temperature
  zero, structured output) that returns capability, difficulty, domain
  tags, and its own confidence. High-stakes flagging is derived from
  medical/legal/financial tags.
- **The workload prompt is data, never instructions.** It is fenced between
  explicit markers in the classifier call with a standing instruction never
  to answer, execute, or continue it — otherwise a conversational or
  imperative workload ("hi", "ignore prior instructions and...") could pull
  the classifier into behaving like an assistant instead of a grader. On the
  Anthropic client, the structured output itself is enforced the same way:
  a forced tool call rather than a hope that the system prompt keeps the
  model in JSON-only mode. Both are versioned into the classifier's cache
  key, so a prompt-handling change invalidates every stale cache entry
  rather than silently mixing old and new classifications.
- **Fail-soft, loudly (G1).** The classifier call has a tight timeout and
  one retry. On failure the request does **not** fail: it degrades to a
  fallback classification (multi-step reasoning, middle difficulty, zero
  confidence, method honestly marked as fallback). Sustained failure raises
  a critical `classifier_degraded` [alert](alerts-delivery.md) — delivered
  to the operator, who can re-point the classifier column via config.
- **Oversize prompts** are classified on head-plus-tail with confidence
  capped and the trace marked truncated, rather than rejected.
- **Per-tenant cache by construction.** The fingerprint is tenant-keyed, so
  cache entries cannot collide across tenants. The duplication (the same
  public prompt classifying once per tenant) is an accepted cost of privacy.
- **The durable fix.** When traces show one code path repeatedly
  misclassified, the correction is standing, not forensic: that path
  switches to the declared form and skips the classifier permanently. The
  audit trail in [History](history-audit.md) makes the pattern visible; the
  method field on every response makes the fix verifiable.

## Dependencies

- Feeds the resolved workload into [Recommendation](recommendation.md); the classification block appears verbatim in responses and in [decision traces](history-audit.md).
- Fingerprinting and the no-prompt-storage guarantee: [Privacy & debug capture](privacy-capture.md) — replay of captured prompts re-runs this classifier.
- Degradation alerts: [Alerts & delivery](alerts-delivery.md).
- Classifier accuracy is measured by the eval harness: [`evals/ACCURACY_PLAN.md`](../../evals/ACCURACY_PLAN.md).
- Wire contract (request/response shapes): [API reference](../api.md).
- Normative contract: [spec §8](../../specs/model-maverick-v1-spec.md).
