# Conversation affinity (proposed D44)

Status: proposed 2026-09-30. The owner asked for this plan. Revised 2026-09-30 by owner direction: the default is the best model for the turn, and cost-aware affinity is an opt-in setting (§4.3). The owner confirms the open questions in §9 before the spec and `parity/DIVERGENCES.md` change. The beads for this work are under the epic "Conversation affinity (D44)".

## 1. The problem

A multi-turn conversation calls `/v1/recommend` once per turn. The proxy and every SDK core do this. Nothing ties a conversation to a model. Each turn is ranked alone.

The pick can move in the middle of a conversation for three reasons:

- The prompt-form projection (spec §8.1) covers the recent conversation, bounded at 16,000 characters (`CONVERSATION_BUDGET` in `crates/maverick-core/src/projection.rs`). As the conversation grows, the classifier can return a different capability or tier. A different cell gives different scores.
- The nightly aggregates change. A turn after 03:00 UTC can rank on new EWMAs.
- Two columns can be close in quality. A small score change flips the order.

A switch has a real cost that the ranking cannot see today.

- **Provider prompt caches belong to one model.** Say turns 1 to 9 ran on Opus 5.5 and turn 10 moves to Fable 5.1. Turn 10 pays full input price for the whole history. On Anthropic it also pays the cache-write premium to build a new cache. Time to first token also goes up.
- **Tone and behavior can change.** Users can notice a model switch in the middle of a conversation.
- **Context is not lost.** The provider APIs are stateless. The client sends the full history on every turn. The new model sees the same conversation. The cost is money and latency, not information.

The ranking has no cache input today. Benchmark cost is `tokens_in × input_price + tokens_out × output_price` (spec §5.1). The reference says "cache token fields ignored". The pricing snapshot has no cache prices (spec §3, `crates/maverick-domain/src/pricing.rs`).

Example with illustrative prices. The history is 120,000 tokens. The pinned column P is warm.

| | P (pinned, warm) | T (new top pick) |
|---|---|---|
| Benchmark `cost_per_run_usd` EWMA | $0.04 | $0.09 |
| Price of the 120k context this turn | cache read $0.50/Mtok: $0.06 | cache write $12.50/Mtok: $1.50 |
| Real cost of this turn | $0.10 | $1.59 |

Today the engine compares $0.04 with $0.09. The real comparison is $0.10 with $1.59.

## 2. Goals and non-goals

Goals:

- **By default, the engine returns the best model for the turn.** Affinity never changes the ranking unless the tenant or the request turns on cost-aware affinity. The owner's rule: the goal is the best model for the task at hand, and cost must not decide the pick unless a caller asks for it.
- **With cost-aware affinity on**, a conversation stays on one column unless a switch is worth it. A switch is worth it when the conversation needs a column that clears a harder tier, when a hard boundary removes the pinned column, or when another column is better by more than the incumbent bonus after the cost of the cold cache.
- In both settings the engine reports whether the pick stayed or moved, so a tenant can see how often switches happen before it turns the setting on.
- Hard boundaries always win: pool (G2), allowlist (G3), standing constraints, zero retention (G5), retirement.
- Every decision is in the trace (G6) and in the response.
- A request without affinity behaves exactly as it does today. Parity is unchanged for it.

Non-goals:

- The engine does not guarantee a provider cache hit. The provider owns its cache.
- The engine does not count tokens exactly. It takes an estimate from the caller.
- No switch happens in the middle of a stream. The chain executor rules are unchanged.
- No schema translation, no Redis, no prompt storage.
- This plan does not change how the existing modes weigh cost. The default mode is `quality` (band 0, weights 0.8/0.1/0.1; D47, decided 2026-10-24, see §9 question 7). `balanced` weighs cost at 0.35 and uses a 1.5-point band, and every mode breaks ties inside the 0.75-point noise band toward the cheaper column (spec §7.4). Changing the tie rule is a separate owner decision.

## 3. Design choice

Three options were considered.

1. **Pin in the client only.** The docs tell callers to keep the first pick. This needs no engine change. It is rejected as the main fix: the logic would be written five times (proxy plus four SDKs), it cannot see the cache cost, and it never switches when it should (escalation, retirement).
2. **Hard pin in the engine.** The first pick is kept for the life of the conversation unless a hard boundary removes it. It is rejected: a conversation that becomes harder stays on a column that no longer clears its tier.
3. **Affinity in the engine with a cache-aware ranking (chosen).** The engine stores the last pick per conversation. On the next turn, the ranking adds the real cost of the context to each candidate, at the cache-read price for the warm pinned column and at the cache-write or input price for the others. The pinned column also gets a small quality bonus inside the ranking. The normal ranking rules then decide. Both adjustments apply only when cost-aware affinity is on. The logic is written once, in the engine, and every component gets it by passing one key.

## 4. Engine contract

### 4.1 Request

`POST /v1/recommend` accepts an optional `affinity` object. It works with both workload forms.

```json
{
  "workload": { "capability": "code_generation", "difficulty": 3 },
  "mode": "balanced",
  "affinity": { "key": "conv_8f2a41", "context_tokens": 120000, "cost_aware": true }
}
```

- `affinity.key`: required when `affinity` is present. A string of 1 to 256 characters. The caller chooses it. It is opaque to the engine.
- `affinity.context_tokens`: optional. An integer from 0 to 10,000,000. It is the caller's estimate of the input tokens that this turn resends. Without it, the turn-cost adjustment (§4.3 step 1) is skipped and only the incumbent bonus applies.
- `affinity.cost_aware`: optional boolean. It overrides the tenant setting (§4.3) for this request. Absent means the tenant setting applies.
- Validation errors are `422 invalid_request` with `details: {"field": "affinity.key"}`, `details: {"field": "affinity.context_tokens"}`, or `details: {"field": "affinity.cost_aware"}`. Unknown fields inside `affinity` are `422 invalid_request` (strict decoding, as for the rest of the body).

### 4.2 Stored state

A new table, number 23:

```sql
create table conversation_affinity (
  tenant_id          text not null references tenants(id),
  affinity_hash      text not null,     -- hmac_sha256 of "affinity:" + key, per-tenant key
  column_id          text not null references model_columns(id),
  recommendation_id  text not null,     -- the recommendation that last wrote this row
  last_used_at       timestamptz not null,
  expires_at         timestamptz not null,
  primary key (tenant_id, affinity_hash)
);
create index conversation_affinity_expires_at on conversation_affinity (expires_at);
```

- `affinity_hash` is `"hmac_sha256:" + lowercase_hex(HMAC-SHA256(key = tenants.prompt_hmac_key, message = "affinity:" + key))`. The `affinity:` prefix separates it from prompt hashes. The raw key is never stored, logged, traced, or put in a metric label. A caller can put a user id or an email in the key, so it is treated like a prompt.
- Zero-retention tenants can use affinity. The row holds no prompt and no completion.
- A row expires `affinity.ttl` after its last use (default 24 hours). An expired row is treated as missing.

### 4.3 Ranking with affinity

**The setting.** `tenant_constraints` gains `affinity_cost_aware` (boolean, default `false`), written through the constraints payload of `PUT /v1/config/constraints` and `PUT /internal/v1/tenants/{id}/constraints`, like `telemetry_opt_out`. A request's `affinity.cost_aware` overrides it for that request. The effective value is reported in the response.

**Cost-aware off (the default).** The ranking is exactly the §7.4 ranking with no affinity input. No turn cost, no incumbent bonus. The pick is the best column for this turn under the request's mode. The engine still reads and writes the affinity row so the response can report `kept` or `switched` (§4.4). This is the owner's default: always the best model for the prompt at hand.

**Cost-aware on.** The four steps below apply.

Affinity runs inside §7.4. §7.1 (pool, allowlist, standing constraints, zero retention, preferences) and §7.3 (`no_data`) are unchanged. They use the unadjusted benchmark cost, so the G4 simulation stays valid.

Let P be the stored column and n be `affinity.context_tokens`. The cache is **warm** when `now - last_used_at <= affinity.cache_warm_seconds` (default 300, the Anthropic default cache TTL).

1. **Turn cost.** When n is present and every survivor has an `input_per_mtok` price, each survivor's cost for ranking is `C'_i = C_i + n × r_i / 1e6`, where:
   - `r_P` is P's `cache_read_per_mtok` when the cache is warm and P has that price;
   - otherwise `r_i` is the column's `cache_write_per_mtok` when it has one, else its `input_per_mtok`.

   `C'` replaces `C` in `nC`, in the noise-band tie rule (§7.4 step 4), and nowhere else. If any survivor has no input price, step 1 is skipped for every survivor, so a column with no price is never favored.
2. **Incumbent bonus.** When P is a survivor, the ranking uses `Q_P + b` for P, where `b = affinity.incumbent_bonus` (default 1.5 quality points, the balanced band). This applies to the band test, `nQ`, and the noise-band test. The displayed `score` stays the real `Q_P`.
3. **Escalation.** The bonus is withheld when P has `clears_difficulty: false` and at least one other survivor has `clears_difficulty: true`. A conversation that gets harder moves to a column that clears the new tier. The turn cost still applies.
4. **Partitions still win.** §7.4 step 5 (preference violators last) and step 6 (availability floor) are unchanged. A pinned column that violates a preference, or falls below the availability floor, loses to a column that does not.

Index 0 is the pick, as today. The chain after it is ordered by the same rules.

### 4.4 Decision status

| `status` | When | `reason` |
|---|---|---|
| `new` | No live row for the key | `null` |
| `kept` | The pick is P | `null` |
| `switched` | P is a survivor and the pick is another column | `escalation`, `outranked`, `preference`, or `low_availability` when cost-aware is on; `best_model` when it is off |
| `broken` | P did not survive §7.1 or §7.3 | the exclusion reason: `not_in_pool`, `not_in_allowlist`, `standing_constraint:<name>`, `status_retired`, or `no_data` |

With cost-aware off, `switched` always has reason `best_model`: the normal ranking picked another column and affinity did not weigh in. `outranked` covers both a quality gap larger than the bonus and a cost gap larger than the cache saving. The trace has the numbers.

### 4.5 Response

The response carries `affinity` only when the request did. It comes after `telemetry` and before `classification_quota_exceeded`, which stays the last key.

```json
"affinity": {
  "status": "kept",
  "previous_column_id": "col_01J...",
  "reason": null,
  "cache_warm": true,
  "cost_aware": true
}
```

`previous_column_id` is P, or `null` for `new`. `cost_aware` is the effective setting for this request. The pick's entry `reason` text says why it was kept or switched, for example `kept for conversation continuity; warm cache saves an estimated $1.49 this turn`.

### 4.6 Writing the row

- Every successful recommend with `affinity` upserts the row: `column_id` = the pick, `recommendation_id` = this recommendation, `last_used_at` = now, `expires_at` = now + `affinity.ttl`. The upsert is in the same transaction as the `recommendations` insert.
- Two turns of one conversation that race: the last write wins. That is acceptable. Both picks are valid.
- A request that fails (4xx, 409, 422) writes nothing.

### 4.7 Following the served column

The chain executor can serve a turn from entry 2 or later (a 429 or 5xx on entry 1). The provider cache is then warm on the served column, not on the pick.

When `POST /v1/telemetry` receives a 2xx item for recommendation R and the item's `model_used` and `endpoint_used` name a chain entry other than index 0, the engine updates the affinity row to that entry's column. It does this only when the row's `recommendation_id` is still R (compare and set), so a later turn is never overwritten. When two chain entries share a model and endpoint (for example thinking on and off), the first such entry after index 0 wins.

A tenant with telemetry opt-out (D39) sends no telemetry. Its row follows the engine's pick only. The docs say so.

### 4.8 Trace, metrics, logs

- The decision trace (§10.2) gains an `affinity` block: `affinity_hash`, the previous column, `status`, `reason`, `cache_warm`, `context_tokens`, `cost_aware` and where it came from (tenant or request), the bonus applied (or withheld and why), and `C'` per survivor.
- A counter `maverick_affinity_decisions_total` by `status` and `cost_aware`. No key, hash, or column id in labels.
- The request log line gains nothing new. The hash is not logged.

### 4.9 Pricing

The pricing snapshot gains two optional fields: `cache_read_per_mtok` and `cache_write_per_mtok`. They parse like the other price fields. A negative value is `pricing snapshot has a negative price`. `price_sync` fills them from the models.dev `cache_read` and `cache_write` fields. `GET /v1/models` shows them in the pricing snapshot. Benchmark run cost does not change: benchmark runs do not use provider caching.

### 4.10 Jobs and config

- A new hourly job, `affinity_prune`, deletes rows where `expires_at < now()`.
- Config (engine), with environment names in the spec's config table:

| Key | Env | Default |
|---|---|---|
| `affinity.enabled` | `MAVERICK_AFFINITY_ENABLED` | `true` |
| `affinity.ttl` | `MAVERICK_AFFINITY_TTL` | `24h` |
| `affinity.cache_warm_seconds` | `MAVERICK_AFFINITY_CACHE_WARM_SECONDS` | `300` |
| `affinity.incumbent_bonus` | `MAVERICK_AFFINITY_INCUMBENT_BONUS` | `1.5` |

With `affinity.enabled = false`, the engine still accepts and validates `affinity`, reads and writes no rows, and answers `status: "new"` with the normal ranking. A bonus of `0` turns off the bonus and keeps the turn cost. `affinity.cache_warm_seconds` and `affinity.incumbent_bonus` matter only for requests where cost-aware affinity is on.

### 4.11 Latency

The declared path target stays p95 under 50 ms. Affinity adds one primary-key read, which runs at the same time as the aggregates read, and one upsert inside the existing insert transaction. M6 measures the declared path with and without affinity.

## 5. Components

### 5.1 Proxy

- A caller sends `X-Maverick-Conversation: <id>`. The value is 1 to 256 printable ASCII characters. A present but malformed value is `400 invalid_request` with message `X-Maverick-Conversation must be 1 to 256 printable ASCII characters`, and the engine is not called.
- The proxy sends `affinity.key = <id>` and `affinity.context_tokens = ceil(request body bytes / 4)`. This is the same estimate that the spend guard uses. Base64 images make it high. That is accepted for v1 (§9 question 3).
- The header is stripped from every upstream request, like `X-Maverick-API-Key` (D30) and `X-Maverick-Settings` (D45).
- The proxy stamps `X-Maverick-Affinity: <status>` on the response next to `X-Maverick-Recommendation-Id`.
- No header means no affinity. Behavior is unchanged.
- The proxy has no per-request cost-aware switch. It uses the tenant setting.

### 5.2 SDK cores

- The shared core gains a per-call `conversation_id` option and an optional `affinity_cost_aware` boolean that maps to `affinity.cost_aware` (absent means the tenant setting). It maps to `affinity.key`. The core computes `context_tokens` the same way as the proxy, from the serialized request body.
- New conformance vectors, `crates/sdk-conformance/vectors/affinity.v1.json`: header and option validation, the token estimate, the recommend request body, and response parsing for each status.
- The TypeScript, Ruby, Go, and Python SDKs live outside this repository. They pick up the vectors when they next update.

### 5.3 MCP

The `recommend_model` tool schema gains the same optional `affinity` object, mirroring `POST /v1/recommend`.

## 6. Guarantees check

| Guarantee | Effect |
|---|---|
| G1 always a model | Unchanged. Affinity only reorders survivors, and only when cost-aware is on. |
| G2 pools | Unchanged. P must survive the pool step. Affinity never adds a column. |
| G3 allowlist | Unchanged. P must survive the allowlist step (`broken`, `not_in_allowlist`). |
| G4 write-time validation | Unchanged. Filters use benchmark cost, not `C'`. |
| G5 privacy and zero retention | The key is hashed per tenant and never logged. P must pass the zero-retention filter. No prompt is stored. |
| G6 audit | The trace records the affinity decision and its numbers. |
| G8 spend | No new provider calls. The classifier still runs once per prompt-form turn, as today. |
| G13 no `POST /v1/outcomes` | The served-column update comes through `/v1/telemetry` only. |

## 7. Proposed `DIVERGENCES.md` row

| # | Area | Go behavior (oracle) | Rust expected | Plan / spec ref |
|---|---|---|---|---|
| D44 | Recommend: conversation affinity | No affinity. Each request is ranked alone. `affinity` in the body is an unknown field, so `422 invalid_request`. `X-Maverick-Conversation` is forwarded upstream by the proxy like any other header. The pricing snapshot has no cache prices | `/v1/recommend` accepts optional `affinity {key, context_tokens?, cost_aware?}`. The engine stores the last pick per conversation (table `conversation_affinity`, key hashed per tenant, TTL 24 h). `tenant_constraints.affinity_cost_aware` (default false) turns on cost-aware affinity; the request can override it. With it off, the ranking is unchanged and the response only reports `kept` or `switched` (`best_model`). With it on, ranking adds the turn's context cost (cache-read price for the warm pinned column, else cache-write or input price) and a 1.5-point incumbent bonus, withheld on difficulty escalation. The response carries `affinity {status, previous_column_id, reason, cache_warm, cost_aware}`. Telemetry moves the row to the served column. The proxy maps `X-Maverick-Conversation` to the key, strips it upstream, and stamps `X-Maverick-Affinity`. Pricing gains `cache_read_per_mtok` and `cache_write_per_mtok`. A request without `affinity` is unchanged | `plans/conversation-affinity-plan.md`; spec §3, §4.3, §7.4, §8.1, §9.2, §9.7, §9.10, §10.2, §11; owner decision 2026-09-30 |

## 8. Spec changes

- §1.1: G5 notes that the affinity key is hashed like a prompt.
- §3: add `conversation_affinity`. The table count becomes 23. `tenant_constraints` gains `affinity_cost_aware`. The pricing comment lists the cache fields.
- §4.3: the cache price fields and their `price_sync` source.
- §7.4: new "Conversation affinity" subsection with §4.3 and §4.4 of this plan.
- §7.5: the `affinity` response block.
- §8.1 "Proxy behavior": the `X-Maverick-Conversation` header rules.
- §9.2: the request field, validation, and response placement.
- §9.7: `affinity_cost_aware` in the constraints payload of `PUT /v1/config/constraints` (and the internal tenant constraints endpoint).
- §9.10: the served-column follow rule.
- §10.2: the trace block.
- §11: the `affinity_prune` job.
- Config table: the four keys.
- `docs/api.md`: request, response, and error codes.
- `docs/onboarding.md`: how to send `X-Maverick-Conversation` from the proxy and `conversation_id` from an SDK.

## 9. Open questions for the owner

Each has a recommendation. The beads assume the recommendation unless the owner says otherwise.

Decided 2026-10-24 by the owner: accept the recommendation for 1, 2, 3, 4, 6, and 7 (bead gcd2.1). Item 5 was decided earlier, as recorded below.

1. **Defaults.** Incumbent bonus 1.5 quality points and a warm window of 300 seconds. Recommendation: accept, and retune from M6 data.
2. **Derived key.** Should the proxy derive a key when no header is sent, for example from a per-tenant hash of the system prompt plus the first user message? It makes affinity work with no caller change. Identical openings in unrelated conversations would share a row, which is harmless (same pick). Recommendation: not now. Add it later as an opt-in proxy setting.
3. **Token estimate.** `ceil(body bytes / 4)` overcounts base64 images. Recommendation: accept for v1. A later version can exclude binary content the way the projection does.
4. **Follow the served column.** Recommendation: yes (§4.7).
5. **Per-tenant settings.** Decided 2026-09-30 by the owner: cost-aware affinity is a tenant setting, `affinity_cost_aware`, default off, with a per-request override. The default is always the best model for the turn. A per-tenant bonus value is not added until a tenant asks.
6. **Client fail-open.** During an engine outage, should the SDK core keep the last pick per conversation in memory and use it first? Recommendation: not in this change. The core keeps its current last-known-good ladder.

7. **Cost in the default ranking.** The owner's goal is the best model for the task, with cost not deciding the pick. The default mode, `balanced`, weighs cost at 0.35, and all modes break near-ties toward the cheaper column. Should the default mode become `quality`, or should a tenant be able to set its default mode? Recommendation: decide separately from D44. Until then, callers who want pure quality send `mode: "quality"`. Answered 2026-10-24 by `plans/production-proxy-plan.md` §9 question 2: the engine default mode is `quality` (D47), and `maverick/auto` means quality (D45).

## 10. Build order

1. Owner confirms §9. Adopt D44 into `specs/`, `parity/DIVERGENCES.md`, and `docs/api.md`.
2. In parallel: pricing cache fields; the `conversation_affinity` migration and store; the ranking change in `maverick-domain`; the proxy and core header work against the spec with the mock engine.
3. The `/v1/recommend` and MCP wiring, response, trace, and metric.
4. The telemetry follow rule and the `affinity_prune` job.
5. An end-to-end multi-turn test through the proxy with the fake provider, a latency check, and the onboarding docs.

## 11. Tests

- Domain (golden values calculated by hand): with cost-aware off, the order equals the no-affinity order for every fixture and a moved pick reports `switched` / `best_model`; with cost-aware on, a warm pinned column is kept when the top pick is better by less than the bonus; switched when better by more; the turn cost flips a close cost-mode ranking; a cold cache charges P the write price; escalation withholds the bonus; a preference violation and the availability floor still win; a survivor with no price skips the turn cost for all; a request with no `affinity` is identical to today for every existing ranking fixture.
- Store (testcontainers): upsert, expiry, compare-and-set on the served column, prune.
- API contract: the tenant setting default (off), set through `PUT /v1/config/constraints`, and the request override in both directions; each `422` field, strict decoding inside `affinity`, response key order with and without `classification_quota_exceeded`, `broken` for each exclusion reason, zero-retention tenant with a pinned column that loses its evidence (`broken`, `standing_constraint:zero_retention`).
- Privacy: the §14.3 canary test extended with a canary in `affinity.key`. The canary appears in no span, log line, or metric label.
- Proxy and core: header validation, stripping upstream, the response header, and the conformance vectors.
- End to end: a ten-turn conversation with the fake provider where the fake aggregates move under it, checking `kept`, `switched` with `escalation`, and `broken` with `not_in_allowlist`.
- Parity: the replay harness and every existing Go fixture pass unchanged.
- `cargo-mutants` on the domain change.
