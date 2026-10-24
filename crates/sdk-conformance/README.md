# sdk-conformance

The SDK conformance harness. It is a mock Model Maverick engine plus a mock provider that run
scripted scenarios. Every SDK (TypeScript, Ruby, Go, Python) runs it in CI, and the Rust
proxy runs the same scenarios, so all of them behave identically on chain advancement,
fail-open, zero retention, telemetry, and alias parsing.

The servers are dumb and deterministic. A scenario names the exact HTTP reply for each call,
and the servers record what they received. The provider wire shapes live in the scenario
JSON, not in translation code here. There is no provider schema translation and no real
network.

## The frozen contract

These files are the cross-language contract (sdk-v0 plan §2.3, openrouter execution spec §6).
An SDK is conformant when it passes every scenario that applies to it and every vector.

- `scenarios/`: one JSON file per scenario, named after its stable id.
- `vectors/`: pure-function cases, one file per set, named `<set>.<version>.json`.
- `conformance.json`: the manifest. It has the contract `version` and the sha256 of every
  frozen file.

A change to a frozen file changes the contract for every SDK. Make the change, bump
`version` in `conformance.json`, and regenerate the checksums with
`MAVERICK_UPDATE_MANIFEST=1 cargo test -p sdk-conformance --test manifest`. The test fails
until the manifest matches the files. Ids are never reused: a removed scenario keeps its id
retired.

## Run the servers

```
sdk-conformance serve [--engine-port N] [--provider-port N] [--scenario FILE]
```

It binds `127.0.0.1` (ephemeral ports by default) and prints one JSON line:

```json
{"engine":"http://127.0.0.1:54321","provider":"http://127.0.0.1:54322"}
```

A CI script in any language reads that line, drives scenarios through the control API, and
reads the recorded requests back.

### Servers

- Engine: `POST /v1/recommend` and `POST /v1/telemetry`, plus the control endpoints.
- Provider: any path. The scenario supplies the OpenAI, Anthropic, or OpenRouter-shaped
  response body.

### Control API (HTTP + JSON, on the engine server)

- `POST /control/scenario`: load a scenario (the JSON body) and clear the recorders. A
  frozen scenario file loads as is: the servers read `engine` and `provider` and ignore the
  other keys.
- `GET /control/recorded`: read the recorded requests as JSON: `{recommend, telemetry,
  provider}`, each an array of `{method, path, query, headers, body, body_len}`. Header
  names are lower case.
- `POST /control/reset`: clear the recorders and the reply cursors.

### Mock scripts

```json
{
  "engine": {
    "recommend": [ <reply>, ... ],
    "telemetry_status": 200
  },
  "provider": {
    "responses": [ <reply>, ... ]
  }
}
```

`recommend` and `responses` are lists of replies. The server sends the reply at the call
index; the last reply repeats past the end; an empty list sends `200` with `{}`. The provider
list is shared by every provider request, in arrival order.

A reply is one of:

- `{"kind":"respond","status":200,"headers":{...},"body":{...}}`. A string `body` is sent as
  its own bytes; any other JSON `body` is sent as `application/json`. A `headers` entry
  overrides the content type. Use it for 2xx, `429` with `Retry-After`, `500`, and semantic
  `400`/`422`.
- `{"kind":"sse","status":200,"headers":{...},"events":["...","..."]}`. Each event becomes a
  `data: <event>\n\n` frame. The bytes are exact, so a scenario can assert byte-identical
  pass-through.
- `{"kind":"delay","delay_ms":2500,"then":<reply>}`. Waits, then answers. A delay past the
  client's deadline drives a timeout or slow headers.

Connection refused: point the client at a port with no listener. The harness serves only the
two printed addresses, so any other `127.0.0.1` port refuses.

## Scenario files

```json
{
  "id": "S02",
  "name": "provider_429_retry_after_advances",
  "group": 2,
  "source": "sdk-v0 plan §2.3 scenario 2; D26",
  "summary": "...",
  "config": { ... },
  "engine": { ... },
  "provider": { ... },
  "steps": [ ... ],
  "recorded": { ... }
}
```

`id` is `S<two-digit group>[letter]` and never changes. `group` is the numbered scenario in
sdk-v0 plan §2.3 (1 to 9) or openrouter execution spec §6 (10 to 12). `engine` and `provider`
are the mock scripts above.

### Config

The runner configures the core under test with these values. A scenario lists only the keys
it changes.

| Key | Default | Meaning |
|---|---|---|
| `api_key` | `"mk_test_conformance"` | `MODELMAVERICK_API_KEY`, the tenant key |
| `privacy` | `"prompt"` | `MODELMAVERICK_PRIVACY`: `prompt`, `declared`, or `off` |
| `telemetry` | `"on"` | `MODELMAVERICK_TELEMETRY`: `on` or `off` |
| `default_model` | none | `MODELMAVERICK_DEFAULT_MODEL` |
| `zero_retention_verified` | `[]` | `MODELMAVERICK_ZERO_RETENTION_VERIFIED` |
| `recommend_timeout_ms` | `2000` | the recommend request timeout |
| `attempt_timeout_ms` | `30000` | the time one chain entry has to return response headers |
| `telemetry_flush_interval_ms` | `10000` | the batcher interval |
| `telemetry_max_batch` | `20` | the batcher size trigger |
| `execution` | `"direct"` | `openrouter` runs every entry through OpenRouter (the caller supplied an OpenRouter key) |
| `provider_keys` | see below | the caller's provider credentials |
| `provider_base_urls` | `"{provider}"` for each | a provider's base URL: `{provider}` is the mock provider, `{refused}` is an address that refuses connections |

Default `provider_keys`: `openai` is `sk-caller-openai`, `anthropic` is `sk-ant-caller`,
`google` is `google-caller`, `xai` is `xai-caller`, `openrouter` is `sk-or-caller`. The
engine base URL is always the mock engine. The core sends each chain entry to the base URL of
the entry's provider (in OpenRouter mode, to the `openrouter` base URL) with the caller's
credential for it. xAI goes through the OpenAI client.

### Steps

Steps run in order. Each is one of:

- `{"call": <call>, "expect": <expect>}`: the application makes one LLM call through the
  core, and the result must match `expect`.
- `{"wait_ms": N}`: sleep N milliseconds.
- `{"check_recorded": <recorded>}`: read `GET /control/recorded` and check it now.
- `{"shutdown": true}`: close the core the way an application exits (this flushes
  telemetry).

After the last step the runner shuts the core down, unless a step already did, and then
checks `recorded`.

A call:

```json
{"surface": "chat_completions", "model": "maverick/auto", "workload": "structured_extraction:3",
 "original_model": "gpt-caller", "body": {"messages": [...]}}
```

- `model` is the model string the application passes: an alias or a concrete model.
- `settings` is optional: `"caller"` preserves caller settings (D14, D45), as the proxy
  header `X-Maverick-Settings: caller`. Omit it to apply recommended settings.
- `workload` is optional: the per-call declared workload, as `X-Maverick-Workload` or the
  language's per-call option (`<capability>[:<tier>]`).
- `original_model` is optional: the application's own concrete model, when the integration
  has one (the last rung of the fail-open ladder).
- `body` is the chat completions request without `model`. The core sets `model` per attempt.

An expect is one of:

- `{"outcome": "response", "status": 200, "body": {...}}`: the application receives this
  provider response unchanged (status and JSON body). A provider error the core surfaces
  (a semantic 4xx, or the last error of an exhausted chain) is also a `response`: SDKs that
  raise it as the host library's error expose the same status and body. Optional
  `min_elapsed_ms` and `max_elapsed_ms` bound the call's wall time.
- `{"outcome": "error", "code": "no_compliant_columns", "status": 409}`: the core refuses the
  call with this code and calls no provider. `status` is present when the engine sent the
  error. Codes: `zero_retention_requires_declared`, `no_compliant_columns`,
  `no_executable_candidates`.

### Recorded checks

```json
{
  "recommend": {"count": 1, "requests": [<request check>, ...]},
  "provider":  {"count": {"min": 1, "max": 2}},
  "telemetry": {"count": 0}
}
```

A stream that is not listed is not checked. `count` is the exact number of recorded
requests, or a `{min, max}` range. `requests[i]` checks recorded request `i`. Fewer checks
than requests is fine.

A request check has any of:

- `headers`: `{name: value}`; each header must have exactly this value.
- `body`: the body parsed as JSON must equal this value.
- `checks`: a list of JSON Pointer (RFC 6901) checks on the body parsed as JSON. Each has a
  `pointer` and one assertion:
  - `equals`: the value is present and equal.
  - `absent`: `true`; nothing is at the pointer.
  - `absent_or_equals`: nothing is at the pointer, or the value is equal.
  - `contains`: the value is a string that contains this text.
  - `length`: the value is an array of this length.
  - `type` (`integer`, `string`, `boolean`, `object`, or `array`), with an optional `min`.
- `not_containing`: none of these strings may appear in any header value or in the raw body.
  This is how the scenarios check that the tenant key never reaches a provider, and that a
  prompt or a provider key never reaches MM.

JSON values compare as JSON: key order and number spelling do not matter.

### Rules the scenarios fix

Where the plans leave a detail open, the scenarios fix it as follows:

- The last-known-good cache key is the tenant plus the recommend request (pool, mode, and the
  workload sent). The scenarios repeat identical calls, so any key built from those works.
- MM itself is retried at most once, so outage scenarios allow a range of recommend
  requests.
- When a zero-retention tenant has no per-call workload, the declared form is the fallback
  `{"capability": "multi_step_reasoning", "difficulty": 3}` (`S06j`).
- A call that does not go through a recommendation (privacy `off`, a concrete model) sends no
  telemetry: an item needs a `recommendation_id`.
- A skipped chain entry (no OpenRouter alias, or an alias not verified zero) sends no request
  and has no telemetry item.
- `endpoint_used` may be absent for direct calls; the engine reads absent as `direct`.
- A core that runs a zero-retention tenant's live chain may skip entries not stamped `zero`
  or run them as the engine sent them; `S06g` checks only the outage.

### The scenarios

| Id | Group | Name | What it pins |
|---|---|---|---|
| `S01a` | 1 | `declared_happy_path` | A declared workload: the core sends only the capability and tier, calls the first entry with the caller's key, and relays the provider response. |
| `S01b` | 1 | `prompt_happy_path` | The prompt form: the core sends the workload projection with its version, never the declared fields. |
| `S02` | 2 | `provider_429_retry_after_advances` | A 429 with Retry-After: 1 makes the core wait at least one second, then call the next entry. |
| `S03a` | 3 | `provider_500_advances` | A 500 advances the chain to the next entry. |
| `S03b` | 3 | `provider_timeout_advances` | An entry whose headers do not arrive within the attempt timeout is abandoned, and the next entry answers. |
| `S03c` | 3 | `connection_refused_advances` | The first entry's provider refuses the connection; the core advances to the next entry. |
| `S04a` | 4 | `provider_400_no_advance` | A 400 is a bad request on every model: the core does not advance and surfaces the provider response unchanged. |
| `S04b` | 4 | `provider_422_no_advance` | A 422 is a bad request on every model: the core does not advance and surfaces the provider response unchanged. |
| `S05` | 5 | `chain_exhausted_surfaces_last_error` | Every entry fails: the caller gets the last provider error unchanged. |
| `S06a` | 6 | `mm_down_uses_cached_chain` | MM answers once, then fails: the same workload runs the last-known-good chain. |
| `S06b` | 6 | `mm_down_cold_cache_uses_default_model` | MM fails with nothing cached: the core calls the configured default model. |
| `S06c` | 6 | `mm_down_no_default_uses_original_model` | MM fails with nothing cached and no default: the core calls the caller's own concrete model. |
| `S06d` | 6 | `privacy_off_never_contacts_mm` | Privacy off is the kill switch: MM is never contacted, the default model is called, and no telemetry is sent. |
| `S06e` | 6 | `zero_retention_422_is_terminal_and_purges` | A 422 zero_retention_requires_declared is surfaced, never failed open, and purges the tenant's cached chains: the next outage cannot reuse the purged chain. |
| `S06f` | 6 | `zero_retention_409_is_terminal` | A 409 no_compliant_columns is surfaced; the default model is not called even though it is configured and verified. |
| `S06g` | 6 | `zero_retention_warm_outage_uses_zero_entries_only` | For a zero-retention tenant, an outage falls open only to cached entries stamped retention_mode zero. |
| `S06h` | 6 | `zero_retention_cold_outage_fails_closed` | For a zero-retention tenant, an outage with nothing cached for the workload fails closed: the default model is not verified zero-retention. |
| `S06i` | 6 | `zero_retention_cold_outage_uses_verified_default` | The same outage with the default model listed in MODELMAVERICK_ZERO_RETENTION_VERIFIED: the core calls the default model. |
| `S06j` | 6 | `zero_retention_switches_to_declared` | After a response with zero_retention: true, the core sends only the declared form for that tenant, even in prompt mode. Without a per-call workload that is the declared fallback. |
| `S07a` | 7 | `telemetry_batch_shape_and_exit_flush` | One item per attempt, sent as one batch at shutdown with the tenant key. check_pass is absent when no check ran. |
| `S07b` | 7 | `telemetry_interval_flush` | The batcher sends on its interval without waiting for shutdown. |
| `S07c` | 7 | `telemetry_size_flush` | The batcher sends as soon as the batch is full, then sends the rest at shutdown. |
| `S07d` | 7 | `telemetry_failure_is_dropped_silently` | The telemetry endpoint fails: the caller is unaffected, and the batch is retried at most once. |
| `S07e` | 7 | `telemetry_one_item_per_attempt` | A failed attempt and the successful one each produce an item with the same recommendation id. Tokens come only from the settled 2xx body. |
| `S07f` | 7 | `telemetry_settings_applied` | The entry's settings are applied to the call, and the item says so. check_pass stays absent. |
| `S07g` | 7 | `telemetry_tenant_opt_out` | The recommend response carries telemetry: false: the core sends nothing for that tenant. |
| `S08` | 8 | `telemetry_off_sends_zero_bytes` | MODELMAVERICK_TELEMETRY=off: the core never opens a connection to /v1/telemetry. |
| `S09a` | 9 | `concrete_model_passes_through` | A concrete model name is not routed: MM is not contacted, and the body reaches the provider untouched. |
| `S09b` | 9 | `entry_settings_override_caller_values` | Without a settings opt-out, the entry's settings replace the caller's value. |
| `S09c` | 9 | `settings_caller_option_keeps_caller_values` | With the settings option `caller` the entry's settings are not applied, the option never reaches MM, and the item says settings_applied false. |
| `S09d` | 9 | `workload_override_sends_declared_form` | In prompt mode, a per-call workload (X-Maverick-Workload) sends the declared form and no projection. |
| `S09e` | 9 | `pool_and_mode_alias` | maverick/<pool>:<mode> sends the pool and the engine mode. |
| `S09f` | 9 | `workload_without_tier` | A per-call workload without a tier sends the capability alone; the engine applies its default tier. |
| `S10a` | 10 | `openrouter_chain_advances_on_one_upstream` | OpenRouter mode: every entry goes to the one OpenRouter upstream with its slug as the model; a 500 advances; each item names the entry's catalog model and endpoint_used openrouter. |
| `S10b` | 10 | `openrouter_zero_retention_skips_unverified_aliases` | A zero-retention tenant in OpenRouter mode: every attempt carries provider.zdr true, and an alias not verified zero is skipped. |
| `S10c` | 10 | `openrouter_zero_retention_none_compliant` | No alias is verified zero: no_compliant_columns, no upstream request, and no direct-key fallback. |
| `S11a` | 11 | `openrouter_aliasless_entry_skipped` | An entry without an OpenRouter alias cannot run there and is skipped. |
| `S11b` | 11 | `openrouter_aliasless_chain` | No entry has an OpenRouter alias: no_executable_candidates, and no upstream request. |
| `S12` | 12 | `openrouter_telemetry_off_sends_zero_bytes` | Telemetry off in OpenRouter mode still means zero bytes to /v1/telemetry. |

## Vectors

`vectors/` holds pure-function cases that every SDK core runs without the servers. Each file
states its rules, and every expectation is hand-checked.

- `alias.v2.json`: the D45 grammar, including routes, direct model parts, and malformed
  routes. All four external SDKs move to v2. `archive/alias.v1.json` preserves the retired
  contract outside the active vectors and manifest. The harness runs v2 only.
- `projection.p1.json`: projection format p1 and the `X-Maverick-Workload` parser (spec
  §8.1). Generated by `crates/maverick-core/tests/projection.rs`.
- `normalize.d29.json`: parameter normalization from catalog constraints, with the Go
  mappings as the fallback (spec §8.1, D29).
- `telemetry.v1.json`: the usage (token count) rules, the attempt to item derivation, and,
  for reference, the engine's item to outcome rules (D16).
- `retention.v1.json`: the retention expiry rule and how a response's `retention_mode` string
  reads (spec §4.5).

## Tests in this crate

- `tests/smoke.rs`: one scenario through both servers with the Rust recommend client.
- `tests/scenarios.rs`: every scenario file loads into the harness and uses only the keys and
  checks this README defines.
- `tests/vectors.rs`: the alias, telemetry, and retention vectors replayed against
  `maverick-core` and the engine's rules in `maverick-domain`. The projection and
  normalization vectors are replayed in `maverick-core` (and the proxy).
- `tests/manifest.rs`: `conformance.json` matches the frozen files.

Running every scenario against `maverick-core` and the proxy is bead 8.3.

## Drive it from Rust

`Harness::start()` runs both servers on ephemeral ports; `load`, `engine_base_url`,
`provider_base_url`, and `recorded` drive one scenario in a test. See `tests/smoke.rs`.
