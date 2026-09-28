# Privacy: prompt fingerprints and debug capture

*The engine sees prompts but keeps only fingerprints — except inside an
explicit, time-boxed, encrypted, access-logged debug window that
zero-retention tenants can categorically refuse.*

## The problem it solves

A recommendation engine sits in front of its callers' most sensitive text.
Storing prompts by default is a liability magnet and a trust-killer; the
engine's privacy story has to be structural, not a policy promise. But two
legitimate needs pull the other way: audit ("how often does this exact
prompt arrive, and how was it classified each time?") requires *recognizing*
prompts without storing them, and debugging a misclassification sometimes
genuinely requires the text. The design serves both without weakening the
default (guarantee G5).

## How it works

- **Fingerprints, not prompts.** Every prompt-form request stores only an
  HMAC-SHA256 fingerprint keyed by a per-tenant secret generated at tenant
  creation, over the exact prompt bytes — no normalization, so a
  one-character edit produces an unrelated hash. Properties, each doing
  work: *one-way* (nothing recoverable); *tenant-keyed* (an outsider can't
  even test a guessed prompt against the log); *stable per tenant* (the
  audit log supports exact-prompt recurrence analysis, and the same hash
  keys the [classification cache](classification.md)).
- **Debug capture is opt-in, boxed, and encrypted.** A tenant opens a
  capture window with a bounded duration, optionally scoped to one API key
  and optionally sampling only low-confidence classifications (the
  interesting ones). While a window matches, prompts are encrypted with a
  per-capture key derived from a master key that never touches the
  database, and stored with a hard expiry; an hourly job hard-deletes
  expired captures.
- **Replay without exposure.** The debugging payoff is a replay endpoint:
  it decrypts the captured prompt *in memory*, re-runs classification
  (optionally under a different classifier version), and returns the
  original and replayed classifications and whether they match — **never
  the text**. Most misclassification investigations end without any human
  reading the prompt.
- **When replay isn't enough, an audited read is.** Replay answers "did the
  classifier agree with itself"; it can't answer "was the routed model
  actually right for this content" — that judgment needs the text. Two
  surfaces exist for exactly that, both the same in-memory decrypt inside
  the same expiry and access-log guarantees, with no new storage: a
  **tenant self-service read** (`GET /v1/debug/captures`, `GET
  /v1/debug/captures/{id}/prompt`) strictly limited to the caller's own
  captures — the window the tenant opened is the consent act, so reading
  its own prompt back exposes nothing it didn't already hand over — and an
  **operator read** on the internal listener (`GET /internal/v1/captures`,
  `GET /internal/v1/captures/{id}/prompt`), root and cross-tenant. No
  tenant-facing route ever returns another tenant's prompt text; another
  tenant's capture id is indistinguishable from a nonexistent one, in the
  response and in the access-log row alike. See `docs/api.md`.
- **Access is logged in a log that outlives the data.** Every replay, read,
  or delete appends to an access log with no foreign key to the capture —
  the record of *who looked* survives the capture's deletion.
- **Zero-retention is belt-and-suspenders.** Zero-retention tenants get
  window creation refused *and* the request-time capture path
  short-circuited by the same flag — two independent mechanisms enforcing
  one promise.
- **Corpus privacy (G7)** is the sibling guarantee on the other side of the
  house: task prompts, golden answers, and rubrics never leave through any
  public surface — see [Capability matrix](capability-matrix.md) and
  [Grading](grading.md).

## Dependencies

- Fingerprints key the [Classification](classification.md) cache and thread through [History & audit](history-audit.md) traces (which store requests with prompt text excluded).
- Capture expiry is an hourly job: [Jobs & scheduling](jobs-scheduling.md); the zero-retention flag lives in tenant constraints ([Tenancy & auth](tenancy-auth.md)).
- Capture/replay endpoints and error semantics: [API reference](../api.md); retention knobs: [Operations](../operations.md).
- Normative contract: [spec §10.1, §10.4, §9.8](../../specs/model-maverick-v1-spec.md).
