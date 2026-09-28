# Model Maverick v1.1 — Engineering Specification

Status: draft for review (revised 2026-07-13: shell aligned to the Agency Product Framework; revised 2026-09-28: shell identity, databases, and contract pipeline aligned to the framework's current template, outcome blending gated on checked outcomes, classification quota in entitlements, engine OTel export, adopted from `plans/master-plan.md` §10)
Source plan: `plans/model-maverick-v1-plan.md` (§Version 1.1: product shell and calibration)
Framework: the **Agency Product Framework** (`product-development-framework/agency-product-framework.md`, sibling repo) governs the shell's stack, contract machinery, deployment, and observability. Where the plan's product sketch and the framework disagree (Devise, Hotwire/importmap), the framework wins — the shell is a framework product, built from the framework template. Divergences from the plan are called out in place; divergences from the framework are enumerated in §3.11.
Baseline: `specs/model-maverick-v1-spec.md` — cited throughout as "v1 §n". Everything v1 specifies stays true unless a section here explicitly amends it. Where the engine as built diverges from the v1 spec, this document specifies against the code as built (the divergences are recorded in `docs/`).
Companion: `specs/capability-matrix-design.md` (measurement methodology).

Requirement keywords MUST, MUST NOT, SHOULD, and MAY are used per RFC 2119.

---

## 1. Overview and invariants

v1.1 is two tracks that can proceed in parallel:

- **The product shell.** A Rails application owning identity and commerce — users, accounts, sessions, billing, administration, the marketing site, the dashboard SPA — driving the engine exclusively through the internal provisioning API (v1 §13, "the Rails seam"). The two applications share no database, and the recommend path never calls Rails.
- **Measurement and surface upgrades to the engine.** IRT calibration replacing pass-rate ceilings, matrix completion thinning the nightly smoke, outcome blending, judge ensembles for high-stakes tiers, two new capabilities (`classification_routing`, `tool_use_orchestration`) with the multi-turn harness and the activated `test_suite` grader, three new providers (DeepSeek, Groq, Together), per-endpoint quality promotion, and tenant-facing alerts with webhooks.

### 1.1 System guarantees

G1–G8 (v1 §1.1) hold unchanged. v1.1 adds four:

- **G9 — One-way dependency, two databases.** Rails and the engine share no database and no tables. Rails calls the engine (seam + public API); the engine never calls Rails. A total Rails outage MUST NOT affect any `/v1/*` request. One Rails `Account` maps to one engine tenant by identifier; that mapping is the entire integration contract.
- **G10 — Predicted never masquerades as measured.** A matrix-completed value never sets `measured: true`, never appears as an observation in `aggregates`, and never widens what a tenant is told. Skipping a benchmark run on a cell is a privilege the cell earns through its own calibration record, revocable the night the record degrades.
- **G11 — Outcome blending is tenant-local and bounded.** Production-outcome adjustments apply only inside the reporting tenant's own ranking, are capped in magnitude, and never touch global aggregates. The decision trace records the adjustment wherever it changed anything.
- **G12 — Benchmark code executes sandboxed or not at all.** Model-generated code runs only inside the dedicated sandbox (no network, resource-capped, credential-free). If the sandbox is not configured, `test_suite` tasks are skipped loudly — never executed in-process.

### 1.2 Non-goals for v1.1

No shared database between Rails and the engine (ever). No usage-metered billing (flat plans; a metering endpoint is future work). No tenant-private corpora, on-premises builds, or self-hosted providers (v2). No public leaderboard. No public engine metrics endpoint (the engine and the trial proxy export OTel to the ops-box Collector over the private network, and the shell joins the same pipeline, per §2.3). No engine-side users — the engine's identity model stays tenants + API keys + internal tokens.

Framework-inherited non-goals, restated because they bind this release: no JWT or bearer tokens in the browser (the shell's API is cookie-session only; non-browser consumers are the *engine's* customers, not the shell's — §3.1); no Redis (Solid Queue/Cache/Cable on Postgres); no separately deployed frontend (the SPA ships inside the Rails image).

### 1.3 Vocabulary additions

- **Shell** — the Rails application (identity, commerce, dashboard, marketing).
- **Account** — the framework's tenancy root (`Account`, with `AccountUser` memberships and `AccountInvitation`s); owns exactly one engine tenant once provisioned.
- **Entitlement** — a plan-derived limit (rate limit, history retention, daily classification quota) that Rails writes into the engine through the seam.
- **Fit** — one IRT estimation run for one capability.
- **Completion** — a matrix-completion prediction for a cell.
- **Panel** — the multi-judge ensemble used on high-stakes tasks.
- **Trajectory** — the recorded multi-turn transcript of a `tool_use_orchestration` run.

---

## 2. Release architecture and repositories

### 2.1 Topology

```
            www / app.modelmaverick.dev                api.modelmaverick.dev
                        │                                       │
              ┌─────────▼──────────┐                  ┌─────────▼──────────┐
              │   Rails shell      │                  │   maverick serve   │
              │  marketing site    │   private net    │  public :8080      │
              │  dashboard (SPA)   │─────────────────▶│  internal :8081    │
              │  billing (Stripe)  │  seam + dash key │  River workers     │
              │  Solid Queue jobs  │◀─────────────────│                    │
              └─────────┬──────────┘  tenant webhook  └─────────┬──────────┘
                        │                                       │
              ┌─────────▼──────────┐                  ┌─────────▼──────────┐
              │  Postgres (shell)  │                  │ Postgres (engine)  │
              │users accts billing │                  │    all v1 state    │
              └────────────────────┘                  └────────────────────┘
```

Three arrows, and only three, cross the boundary:

1. **Seam calls** (Rails → engine `:8081`): provisioning, entitlements, config writes, catalog administration, operator alert feed. Bearer token per v1 as built: the full `label:secret` string from `MAVERICK_INTERNAL_TOKENS`.
2. **Dashboard reads** (Rails → engine `:8080`): tenant-scoped history, policies, models, calibration, outcomes, alerts — via a per-account **read-scoped API key** (§4.1) held server-side by Rails.
3. **Tenant webhook deliveries** (engine → Rails public endpoint): the engine's tenant alert fan-out (§4.5) POSTs to Rails like any customer webhook. Rails consumes the same feature it sells; this is deliberate.

The recommend hot path touches none of these (G9).

### 2.2 Repositories and deployment

- Engine: `modelmaverick` (this repo), unchanged shape.
- Shell: **new repository `modelmaverick-web`** — cloned from the framework template via the `new-product` skill (tokens file, contract pipeline, OTel wiring, Kamal roles, and CLAUDE.md conventions arrive pre-wired), then given its own migrations and Kamal config.
- Corpus: `modelmaverick-corpus` (private), gains the two new capability directories.

Both applications deploy with Kamal to the same **dedicated** Hetzner host at v1.1 scale (framework Tier 1, one box per product — ModelMaverick trips the shared-box promotion triggers on day one: paying users, production provider API keys — so a shared box was never an option). The Kamal proxy routes `www`/`app` hosts to Rails and `api` to the engine; the engine's internal listener stays reachable only on the private Docker network, where Rails addresses it by container name. Nothing about the engine's single-node design changes.

Framework deploy posture, applied to this box:

- **The shell ships two Kamal roles:** `web`, and `worker` running the same image with `cmd: bin/jobs` and the proxy disabled — a web-only deploy passes a web smoke test while the outbox silently never drains. CI includes the framework's deployed smoke test: enqueue a job, verify it executed.
- **Postgres per application as a Kamal accessory** with a named volume; **nightly off-box backups for both databases** with a documented restore path. Backup freshness is monitored (§2.3), and a scheduled restore test into a disposable Postgres instance proves the dumps are usable.
- **Secret custody:** one vault project for the product; `.kamal/secrets` contains resolver code only, never raw credentials — no raw value in shell history, CI variables, or dotfiles. Kamal's on-host materialized env files are a documented, rotation-covered artifact.
- **Host hardening:** only 80/443/SSH exposed, key-only SSH, unattended OS security upgrades, and the quarterly Docker/kamal-proxy/OS upgrade pass from the ops runbook.

### 2.3 Fleet health and observability

The shell is a framework product and joins the ops-box pipeline; the engine keeps its v1 surface. Concretely:

- **Shell telemetry:** standard OTel Ruby SDK exporting to the Collector on the ops box (never a backend directly) — traces to Tempo, errors to GlitchTip via the Sentry SDK (DSN by env), logs to stdout → Docker log driver → host shipper → Loki, metrics derived at the Collector. Per-product resource labels on every signal. Telemetry is best-effort by design: exporters buffer and drop, never block the app. The shell has no AI features, so the RubyLLM/GenAI instrumentation and the content-capture wrapper are simply not pulled in.
- **Engine telemetry:** the engine and the trial proxy export OTel traces, logs, and metrics to the ops-box Collector over the private network (v1 §14.3), available from v1 and independent of the shell, with no public metrics endpoint. The v1 §14.3 redaction rule applies: no prompt text, completion, request body, or credential in any span, log field, or metric label, and the Collector pipeline does not export engine or proxy GenAI content to Langfuse. The engine still sits outside the framework template (§3.11); the host shipper and the probes below cover it too. (changed 2026-09-28 for the Rust port; the Go engine implements the prior rule: structured logs only, no OTel)
- **Fleet health, per the framework:** HTTP probes from the ops box against both apps (`/up` on each); a **dead-man heartbeat** — a recurring Solid Queue job pinging a per-product check, catching dead workers, dead schedulers, and DB connectivity in one signal (the engine's own alert machinery already covers its job chain); a **backup-freshness heartbeat** (last success time + dump size, alerting past the stated RPO) covering both databases; alert routing defined once, to the same operator channels v1 already uses.

---

# Part I — The product shell

## 3. The Rails application

### 3.1 Stack

Per the framework's locked stack (this supersedes the plan's Devise/Hotwire sketch, which predates Amendment 001):

- Rails 8 (Ruby current stable), Postgres, **Solid Queue / Solid Cache / Solid Cable** — no Redis, matching the engine's no-Redis stance.
- **Rails 8 auth generator** with database-backed cookie sessions (httpOnly, Secure, SameSite; CSRF-protected). Email/password + email confirmation + reset; TOTP two-factor (rotp) SHOULD ship for operator accounts, MAY ship for customers. No JWT, no bearer tokens in the browser — framework hard rule. *(Plan divergence: the plan named Devise; the framework template ships the Rails 8 generator, and the shell inherits it.)*
- **Vite React + TypeScript SPA**, served same-origin from the Rails image — the Vite build runs at image build time, output copied into Rails public assets. **Tailwind + shadcn/ui**, themed entirely through `app/frontend/styles/tokens.css` (generated by the `design-tokens` skill at kickoff); components use tokens and the existing component set only — inline style values are forbidden and linted.
- **The contract pipeline** (non-negotiable, template-inherited): the SPA consumes the shell's own `/api/v1` namespace; the OpenAPI spec is **hand-authored and spec-first**, the source of truth for every endpoint, and Minitest request tests assert each endpoint's requests and responses against it (committee/skooma conformance), so behavior cannot drift from the spec in either direction without a red build; the TypeScript client and TanStack Query hooks are generated from the spec (openapi-typescript/orval); CI (the template's prewired `bin/ci`) regenerates and diffs, and any drift hard-fails the build. An endpoint starts in the spec, gets a conformance test, and regenerates the client in the same commit. *(changed 2026-09-28: replaces the rswag-generated spec, which the framework no longer uses)* All browser data fetching goes through the generated hooks; raw `fetch` is forbidden and linted. Serialization through named serializer objects, never ad hoc controller hashes. Default-deny CORS (everything is same-origin — nothing to configure), and the idempotency-key convention on mutating endpoints.
- **Pay** with **Stripe** for billing. Stripe Checkout for card entry; the shell stores no card data. *(Product-level addition: the framework template deliberately ships no billing framework; this product needs billing.)*
- **Accounts-first identity and Pundit** (template-inherited): `Account` / `AccountUser` (jsonb roles) / `AccountInvitation`, `Current.account` set per request from the session and resolved through the user's memberships only, tenant-owned models inheriting `AccountRecord` (acts_as_tenant), and Pundit policies receiving `Current.account_user` as `pundit_user` (§3.3). *(changed 2026-09-28: replaces the organizations/memberships sketch)*
- **Postgres with the Rails 8 multi-database layout:** the primary database plus dedicated `queue`, `cache`, and `cable` logical databases, each with its own migration path, backing Solid Queue, Solid Cache, and Solid Cable.
- Minitest: request tests assert OpenAPI conformance (committee/skooma) and double as the API test suite; Rails system tests cover the SPA's core loops. RuboCop rails-omakase. *(changed 2026-09-28: replaces RSpec, rswag, and Capybara)*
- Kamal 2 deploy (`web` + `worker` roles, §2.2), `/up` health endpoint.

The shell's `/api/v1` never grows token auth. The framework adds token auth with the first paying non-browser consumer — and ModelMaverick's non-browser consumers are the **engine's** customers (`api.modelmaverick.dev`, engine API keys, the MCP surface). The shell's API serves exactly one client: its own SPA.

The seam client is one small lib class (`Engine::Client`) wrapping Net::HTTP with the internal token, the acting-party header (§4.4), timeouts (2 s connect / 10 s read), and typed errors mirroring the engine's error envelope (v1 Appendix B). No engine SDK dependency; the contract is the spec.

### 3.2 Data model (shell database)

Rails migrations in the primary database (the `queue`, `cache`, and `cable` databases hold only Solid Queue/Cache/Cable tables), conventional `id`/timestamps omitted here. Tenant-owned tables carry `account_id` and their models inherit `AccountRecord`. No engine data is mirrored beyond what the UI needs to render lists without a live engine call (alerts copies, key metadata).

| Table | Columns (beyond defaults) | Notes |
|---|---|---|
| `users` | `email_address`, `password_digest`, `name` (Rails 8 auth generator) | |
| `sessions` | `user_id`, `ip_address`, `user_agent` | Rails 8 auth generator; DB-backed sessions |
| `accounts` | `name`, `owner_id`, `personal` flag, membership counter cache (framework template) | one account ↔ one engine tenant |
| `account_users` | `account_id`, `user_id`, `roles` jsonb (default `{}`) (framework template) | unique (account_id, user_id) |
| `account_invitations` | `account_id`, `invited_by_id`, `token`, `name`, `email`, `roles` jsonb (framework template) | unique (account_id, email); unique token |
| `engine_links` | `account_id` (unique), `engine_tenant_id`, `dashboard_key_id`, `dashboard_key_ciphertext` (Rails `encrypts`), `state` enum: `pending`,`provisioned`,`suspended` | the account↔tenant mapping (G9) |
| `provisioning_actions` | `account_id`, `action`, `payload` jsonb, `state` enum: `pending`,`succeeded`,`failed`, `attempts`, `last_error`, `idempotency_key` | the outbox (§3.4) |
| `plans` | `name`, `stripe_price_id`, `rpm`, `retention_days`, `classification_daily_quota`, `trial_days`, `active` | plans are data, not code |
| `pay_*` | Pay gem tables | subscriptions, customers |
| `engine_api_keys` | `account_id`, `engine_key_id`, `name`, `last4`, `scope`, `revoked_at` | metadata only — never the raw key |
| `alert_copies` | `account_id`, `engine_alert_id` (unique per account), `kind`, `severity`, `subject` jsonb, `message`, `engine_created_at`, `read_at` | written by the webhook receiver (§3.8) |
| `operator_flags` | `user_id` (unique) | grants `/admin` (§3.7) |

### 3.3 Identity and roles

- A user belongs to any number of accounts through `AccountUser` memberships; invitations are `AccountInvitation` tokens with roles copied onto the membership on accept. Sessions per the Rails 8 auth generator (DB-backed `sessions` table, cookie-identified). `Current.account` is set per request from the session and resolved through the user's memberships only, never from a client-supplied id; all dashboard routes scope through `Current.account`, and tenant-owned models inherit `AccountRecord`.
- **Roles** (enforced in Rails only; the engine has no users) live in `AccountUser.roles` jsonb, and every authorization decision is a Pundit policy receiving `Current.account_user` as `pundit_user`. The account owner (`accounts.owner_id`, transferable through the template's sudo-gated ownership flow) holds billing and membership management plus everything below. `admin`: engine config, API keys, capture windows. `member`: read-only dashboards.
- Account deletion is soft: cancel subscription, suspend the engine tenant, retain data. The engine has no tenant-delete endpoint by design; nothing here adds one.

### 3.4 Account ↔ tenant lifecycle

**Provisioning (account creation):**

1. Create the account row and an outbox action `provision_tenant` in one Rails transaction.
2. The Solid Queue worker executes the action: `POST /internal/v1/tenants {name}` → store `engine_tenant_id`; then `POST .../api-keys {name: "dashboard", scope: "read"}` (§4.1) → store the key id and ciphertext; then apply the plan's entitlements (§3.5); mark the link `provisioned`.
3. Retries: exponential backoff, 10 attempts. A still-failing action emails ops and shows the account a "setting up" state — the dashboard degrades, signup never blocks on the engine (G9).

Idempotency: tenant creation is the one non-idempotent seam call, so the action records the returned tenant id immediately and a retry after a partial failure resumes from the recorded step rather than re-POSTing. `idempotency_key` guards duplicate outbox rows.

**Suspension state machine** (Rails-owned; the engine only ever sees suspend/activate):

```
trialing ──subscribe──▶ active ──payment fails──▶ past_due ──7-day grace──▶ suspended
    │                     ▲                          │ pays                     │ pays
    └──trial expires──────┼──────────────────────────┘                          │
            ▼             └─────────────────────────────────────────────────────┘
        suspended      canceled ──period ends──▶ suspended (data retained 90 days, then ops review)
```

Every transition into/out of `suspended` enqueues an outbox action calling `POST /internal/v1/tenants/{id}/suspend` or `/activate`. A suspended tenant's API keys 403 at the engine (v1 §9.1) — enforcement lives where the traffic is.

**Reconciliation** (daily Solid Queue job): for every `engine_link`, `GET /internal/v1/tenants/{id}` and compare status against the expected state; page through `GET /internal/v1/tenants` (§4.3) and report engine tenants unknown to Rails (minus a configured allowlist for hand-provisioned tenants, e.g. tenant #1 from dogfooding). Drift → ops email with the diff; the job never auto-heals in v1.1 — a human decides which side is right.

### 3.5 Billing and entitlements

- Plans: **Trial** (14 days, full features, low rpm), **Pro**, **Scale** — rows in `plans`, prices in Stripe. Flat monthly; no metering (§1.2).
- Stripe webhooks (via Pay) drive the state machine in §3.4. Webhook handling is idempotent by Stripe event id.
- **Entitlement sync:** on subscribe/plan-change, an outbox action writes `PUT /internal/v1/tenants/{id}/limits {rpm, history_retention_days, classification_daily_quota}` (§4.2). The classification quota bounds the MM-paid classifier calls a plan buys (v1 §8.3, G8). The engine enforces; Rails decides. No other billing state crosses the seam — the engine never knows what a customer pays.
- Seats: unlimited members per account in v1.1 (plans differentiate on engine limits, not seats).

### 3.6 The dashboard

React SPA pages behind cookie-session auth. The browser talks only to the shell's `/api/v1`, through the generated client and TanStack Query hooks — it never calls the engine origin and never holds engine credentials (CORS stays default-deny; nothing cross-origin exists to allow). Rails controllers do the engine work server-side — reads with the account's read-scoped dashboard key against `/v1/*`, config writes through the seam — and re-serialize the results into the shell's own OpenAPI contract through named serializers. The **Source** column below is the Rails-side upstream, not a browser call. The dashboard key cannot write (§4.1), so a leaked one is read-only by construction.

| Surface | Source | Notes |
|---|---|---|
| Overview | `/v1/history/recommendations` (recent), `alert_copies` unread count, engine link state | |
| Recommendations browser | `/v1/history/recommendations` + by-id trace view | renders the full decision trace: survivors, exclusions, flags. Shows `prompt_hash` only — the shell never sees prompt text (G5 extends across the boundary) |
| Policy table | `/v1/policies` | shows `ceiling_source` (§5.4) per capability |
| Models | `/v1/models` | catalog with status/pricing/deprecations |
| Score trends | `/v1/history/models/{column}` | sparkline per pool column, regression markers |
| Calibration | `/v1/history/calibration` | Brier trend per capability |
| Outcomes | `/v1/history/outcomes` | outcome mix per capability |
| Alerts | `alert_copies` | tenant-relevant alerts (§4.5), mark-read |
| Config: pools / weights / constraints | read `/v1/config/*` via dashboard key; write via seam `PUT /internal/v1/tenants/{id}/...` | engine G4 validation errors pass through the shell's API envelope verbatim — the 422 the SPA renders names the pool and constraint |
| API keys | seam issue/revoke | raw key rendered exactly once from the seam response, never persisted (only id/name/last4/scope) |
| Debug capture | `/v1/debug/*` — but writes need a full-scope key Rails does not hold | v1.1 ships capture management as **docs + copy-paste curl**, not UI. A UI would require Rails to hold a write-capable key, weakening §4.1; revisit in v1.2 |
| Webhooks | read/write via seam passthrough to the tenant webhook config (§4.5) | lets customers point alerts at their own endpoints too |

### 3.7 Admin area (operator)

`/admin`, gated on `operator_flags`, all actions through the seam with the acting-party header:

- Accounts/tenants list with link state, plan, engine status; manual suspend/activate with a required reason (logged).
- Engine alert feed: `GET /internal/v1/alerts` (operator-wide, unfiltered).
- Catalog administration: columns list/create/edit (`POST/PUT /internal/v1/columns...`), including quality promotion (§12) and retirement — the engine's retirement guard errors render verbatim.
- Corpus activation: `POST /internal/v1/corpus/activate` behind a confirmation naming the version diff.
- Reconciliation report history.

### 3.8 Alert rendering (email + dashboard)

The plan item "alerts rendered to email and the dashboard" is implemented as a consumer of the engine's tenant alert fan-out (§4.5):

- At provisioning, Rails registers itself as the account's tenant webhook: URL `https://app.modelmaverick.dev/webhooks/engine/{account_id}` (the prefixed public id, `acct_…`), per-account secret generated by Rails.
- The receiver verifies the HMAC signature, upserts an `alert_copies` row (idempotent on engine alert id), and enqueues notification email to the account's owner and admins per their notification preferences (immediate for `critical`, daily digest for `warning`/`info`; per-user setting).
- A customer adding their own webhook does not displace the Rails one (§4.5 allows multiple).

Operator alerts (spend, job failures) are unchanged v1 machinery — SMTP + operator webhook — and additionally visible in `/admin`.

### 3.9 Security posture

- The internal token and Stripe secrets reach the app as env via the framework's vault-resolver custody chain (§2.2/§15); dashboard keys encrypted at rest (`encrypts`); raw tenant API keys are never stored anywhere in the shell.
- The shell never receives, stores, or logs prompt text; the dashboard renders fingerprints and traces only (G5).
- CSRF protection on session and API controllers; session cookies `Secure`/`HttpOnly`/`SameSite=Lax`; login throttling with lockout after repeated failures; password policy: length floor + breached-password denylist check (the auth generator imposes none — the shell must).
- The seam is reachable only over the private network; Rails validates it is not configured with a public engine URL at boot (refuses to start if the internal base URL resolves outside RFC 1918/private ranges, overridable for development).
- PII boundary: the engine holds account names (as tenant names) and nothing else about people. Deleting a user is a pure Rails operation.

### 3.10 Marketing site

Public controllers and server-rendered ERB views in the same Rails app: landing, pricing (rendered from `plans`), docs (the engine repo's `docs/` published read-only), signup. The SPA is the product dashboard; public pages are content, need no client state, and stay server-rendered — no separate static-site generator, no second frontend. Styled from the same token file. Trademark/naming check remains an operator gate before launch (plan §Risks).

### 3.11 Framework conformance

The shell starts from the framework template (`new-product` skill) and inherits its conventions wholesale — contract pipeline and drift gate, design tokens, CLAUDE.md hard rules, OTel wiring, Kamal roles, secret custody, fleet heartbeats. Deviations, named so they are decisions rather than drift:

- **The engine is Go.** The one-stack rule governs products built through the framework; the engine predates the framework and *is* the product being sold — a measurement system, not a second web stack. It stays outside the template and its drift policy, with its own spec lineage (v1 §1).
- **Pay + Stripe** — product work, not template work (§3.1).
- **Server-rendered marketing pages** (§3.10) — the framework's frontend choice is about product UI, not public content.
- **No AI features in the shell** — RubyLLM conventions, the sidecar escape hatch, and the GenAI content-capture wrapper are unused here. The AI is the engine, on the other side of the seam.

Everything else is the framework verbatim.

---

## 4. Engine additions for the shell

All additive; nothing in v1's public contract changes shape. Endpoint/error deltas: Appendix A/B.

### 4.1 API key scopes

`api_keys` gains `scope text not null default 'full' check (scope in ('full','read'))`.

- `read` keys: all `GET /v1/*` endpoints and the MCP tools `get_policy`/`list_models`. Everything else — including `POST /v1/recommend` and the `recommend_model` tool, which write recommendation rows — returns `403 insufficient_scope` (new code, Appendix B).
- Issue: `POST /internal/v1/tenants/{id}/api-keys` accepts optional `scope` (default `full`). Tenant-facing key issuance does not exist in v1 and still does not; keys are born at the seam.
- Purpose: the Rails dashboard key. Also useful for customers' read-only integrations, so it is a first-class feature, documented in `docs/api.md`.

### 4.2 Per-tenant limits

Seam endpoint `PUT /internal/v1/tenants/{id}/limits`, which exists from v1 for the classification quota (v1 §13), gains `rpm` and `history_retention_days`: `{rpm?, history_retention_days?, classification_daily_quota?}` (null clears an override; response echoes effective values).

- `tenants` gains `rpm_override int`, `retention_days_override int` (both nullable; global config remains the default). `classification_quota_override` already exists from v1 (v1 §3).
- Rate limiting (v1 §9.1) reads the tenant override when present. `history_prune` (v1 §11) prunes per-tenant using the override when present.
- Bounds validated at write: `1 ≤ rpm ≤ 10000`, `30 ≤ retention ≤ 3650`, `classification_daily_quota ≥ 0`, else `422 invalid_request`.

### 4.3 Tenant listing

New seam endpoint: `GET /internal/v1/tenants?limit=&cursor=` — paginated like v1 §9.6, returning id, name, status, created_at. Exists for reconciliation (§3.4); intentionally summary-only.

### 4.4 Acting-party audit

Seam requests MAY carry `X-Maverick-Acting-Party: <opaque string ≤ 200 chars>` (Rails sends `user_<id>@acct_<id>`, prefixed public ids). The value is recorded verbatim in the provisioning audit log line (`acting_party=...`) beside the token label. No validation beyond length — it is attribution, not authentication. End-to-end accountability: engine audit line → acting party → Rails session log.

### 4.5 Tenant-scoped alerts and tenant webhooks

The plan item "tenant webhooks for regression alerts", generalized slightly.

**Relevance mapping.** New table `alert_tenants (alert_id, tenant_id, primary key (alert_id, tenant_id))`, written in the same transaction as the alert insert (inside `alerts.Store`). Population rule by alert kind:

- `regression`, `parse_failure_rate` (subject = a column/cell): every tenant with a pool containing the column.
- `pool_would_be_empty` (subject names tenants): those tenants.
- All other kinds (`job_failure`, `spend_*`, `classifier_degraded`): operator-only; no rows. Tenants never see engine operational internals.

**Tenant feed.** `GET /v1/alerts?since=&kind=&limit=&cursor=` (key auth, read scope sufficient): the caller tenant's alerts via `alert_tenants`, newest first. `acked_at` stays operator-only; tenant read state is the consumer's concern (Rails tracks its own).

**Tenant webhooks.** New table:

```sql
create table tenant_webhooks (
  id          text primary key,               -- wh_<ulid>
  tenant_id   text not null references tenants(id),
  url         text not null,                  -- https required (http allowed for private-range hosts)
  secret      text not null,                  -- tenant-supplied or generated; returned once
  kinds       text[] not null default '{regression,parse_failure_rate,pool_would_be_empty}',
  active      boolean not null default true,
  created_at  timestamptz not null default now(),
  updated_at  timestamptz not null default now()
);
create index on tenant_webhooks (tenant_id);
```

- Config: `GET/PUT /v1/config/webhooks` (full-replace per v1 §9.7 conventions; max 3 webhooks per tenant; `kinds` must be ⊆ the tenant-visible set). Also mirrored at the seam (`PUT /internal/v1/tenants/{id}/webhooks`) so the shell can manage them (G4-style parity: same payload, same validation).
- Delivery: the existing async fan-out (`alerts.Fanout`) grows a tenant leg — for each `alert_tenants` row, each active matching webhook gets a POST with the alert JSON, `X-Maverick-Signature: hmac_sha256=<hex>` over the raw body with the webhook secret, 3 retries with backoff. Same discipline as v1 §11.1: async, never blocks or fails the writing transaction; the row is the durable record.
- Failure visibility: 10 consecutive delivery failures on one webhook → `webhook_failing` operator alert (warning) naming tenant and webhook id; delivery keeps retrying per-alert regardless (no auto-disable in v1.1).

---

# Part II — Measurement upgrades

## 5. IRT calibration

Replaces pass-rate ceilings (v1 §6.6) as the primary ceiling method, per capability, with pass-rate as the permanent fallback. The plan's containment applies: gonum first; if fitting proves numerically fragile, a Python sidecar for this one batch step is the sanctioned fallback.

### 5.1 Model

Two-parameter logistic per capability: `P(pass) = σ(a_j · (θ_i − b_j))` for column ability `θ_i`, task difficulty `b_j`, discrimination `a_j > 0`. Observations: task-repeat pass booleans (v1 §6.1) from the trailing `irt.window_days` (default 90) at the current matrix_version, all columns, all tiers.

Estimation: alternating penalized logistic regressions (fix task params, fit abilities; fix abilities, fit task params) to convergence; L2 penalty for stability; scale identified by constraining θ to mean 0, sd 1 across fitted columns. Deterministic seed ordering — same inputs, same fit.

### 5.2 Fit health gate

Minimum data: ≥ 8 columns and ≥ 40 pass observations per column in the capability; else no fit, fallback stands. Each fit holds out 20% of observations (stratified by column × tier): the fit is **healthy** iff holdout log-loss beats the baseline (per-task mean pass rate) by ≥ `irt.min_gain` (default 2%). Unhealthy fits are stored with `active = false` and the capability stays on pass-rate ceilings — silently correct, loudly logged.

### 5.3 Ceilings from fits

For an active fit, a column's predicted pass at tier `T` is the mean of `σ(a_j(θ − b_j))` over the tier's active tasks. Ceiling: `max { T : ∀ t ≤ T, predicted_pass(t) ≥ B + β }` — v1 §6.6's formula with model-implied pass substituted for the raw EWMA pass rate. Bar `B` and buffer `β` unchanged. `clears_difficulty` and banded policy consume ceilings identically; nothing downstream knows which method produced them.

Cold-start columns are never assigned a fitted θ before `scoring.min_nights` observations; they use pass-rate ceilings until then.

### 5.4 Surfacing

`GET /v1/policies` gains per-capability `ceiling_source: "irt" | "pass_rate"` and, when IRT, the fit date. Additive field; existing clients unaffected.

### 5.5 Schedule and storage

New job `irt_fit`, chained after the weekly sweep's `aggregate_rebuild` (fits weekly; nightly data movement is absorbed by the EWMA layer, and ceilings are a slow-moving quantity). Tables in §13: `irt_fits` (one row per capability per fit, health metrics, active flag — exactly one active per capability), `irt_abilities`, `irt_task_params`. Fits are append-only history; task difficulty drift across fits is itself corpus-quality signal (companion doc).

## 6. Matrix completion

Purpose (plan): skip real runs only on cells the benchmark has already proven it can predict, cutting nightly spend without lying about freshness. BenchPress-grounded (plan §References).

### 6.1 Prediction model

Matrix: rows = columns, cols = (capability, tier) cells, values = pass-rate EWMA. Soft-impute low-rank completion (rank `completion.rank`, default 3) via gonum SVD on the centered matrix, iterating imputation to convergence. Output: predicted pass probability per cell, clamped [0,1].

### 6.2 Prediction lock integration

When completion is enabled and the capability has ≥ `completion.min_observed_frac` (default 0.6) of its cells measured, `prediction_lock` (v1 §11) locks completion-model predictions (`source = 'matrix_completion'`) for tonight's cells; otherwise `'ewma'` as in v1. Brier scoring is unchanged — which means from the night completion turns on, **the Brier series measures the completion model itself**, on cells that still actually run. That is the evidence the skip gate consumes.

### 6.3 Skip eligibility (the gate)

A (cell, task-set) is skippable tonight iff **all** hold:

1. The cell is `measured` with `n ≥ 10`.
2. The capability's mean Brier over the last 4 scored batches ≤ `completion.brier_gate` (default 0.10).
3. The cell's own trailing prediction error — `|predicted − observed pass rate|` over its last 3 run nights — ≤ `completion.cell_error_gate` (default 0.10).
4. The cell's column is not `cold_start` and the cell last actually ran fewer than `completion.max_skip_nights` (default 2) consecutive nights ago — every cell runs at least every third night.
5. It is a **nightly smoke** batch. The weekly sweep always runs everything; completion never touches it. Worst-case staleness of any cell's real data: 2 skipped nights, bounded by 7 days via the sweep regardless.

Skipped cells receive **no synthetic observation**: the EWMA simply does not advance that night (G10). The batch log records `cells_skipped` and projected-spend-avoided; the spend guard projects over the post-skip schedule.

### 6.4 Revocation

Gate checks run nightly from current data; a cell or capability that degrades falls out of eligibility the same night. `regression_scan` treats a post-skip observation gap normally (the raw-shock test compares against `S_{t−1}` whenever the next real observation lands).

## 7. Outcome blending

Telemetry-derived outcome rows — produced automatically by MM-authored SDK/plugin/proxy components via the internal `POST /v1/telemetry` producer (integration-surfaces plan §1, G13), not voluntary `POST /v1/outcomes` reports (retired) — start influencing rankings: tenant-locally and bounded (G11). Plan caveat honored: production feedback is noisy, so the design is shrinkage-first; observed data beats volunteered data.

### 7.1 Computation

Nightly job `outcome_blend`. Per (tenant, model column, capability), over the trailing `blend.window_days` (default 30):

- **Checked outcomes only:** `success` (derived from `check_pass: true`) positive; `parse_failure` (`check_pass: false`) and historic `quality_insufficient` negative. `completed_unchecked` (a 2xx with no check, v1 §9.10), `latency_exceeded`, `provider_error`, and `other` are excluded (not quality signal); `completed_unchecked` counts toward availability only. (changed 2026-09-28 for the Rust port; under the prior rule any unchecked 2xx counted as `success`) Capability comes from the outcome's recommendation's classification; the column from `model_used`.
- `n` = signal count; `s` = success fraction; `p` = expected pass — the mean `pass_rate` EWMA over the (column, capability, tier) cells of the window's recommendations, weighted by their tier mix.
- Shrunk delta: `d = n·(s − p) / (n + n₀)` with `n₀ = blend.prior_n` (default 20).
- Adjustment in quality points: `adj = clamp(κ · d, −cap, +cap)`, `κ = blend.kappa` (default 20), `cap = blend.max_points` (default 2.0). Applied only when `n ≥ blend.min_n` (default 5); else 0.

Ten points of sustained success-rate gap at high volume moves a column ±2 quality points for that tenant — enough to flip noise-band ties and mode-band leadership, never enough to overturn a real quality gap.

### 7.2 Application

At ranking (v1 §7.4), the tenant's stored adjustment is added to `Q` before normalization — affecting leaders, `V`, and tie-breaks — for that tenant only. Displayed `score` remains the unblended global quality (v1 §6.7's "absolute" promise holds). Honesty surfaces:

- Trace candidates gain `outcome_adjustment` and `quality_blended` wherever `adj ≠ 0`.
- The response gains top-level `outcome_blended: true` when any candidate carried a nonzero adjustment.

**Gate.** Blending ships disabled (`BLEND_ENABLED=false`) and is enabled only once check coverage (the share of telemetry items carrying a `check_pass` value) and the check false-positive rate pass gates set at M12; until then `outcome_blend` computes and stores nothing that ranking reads. (changed 2026-09-28)

Opt-out: `tenant_constraints` gains `outcome_blending boolean not null default true`; the constraints config payload (both surfaces) accepts it.

Storage: `tenant_column_adjustments` (§13), full-replaced by each job run. Global aggregates are never touched (G11).

## 8. Judge ensembles

High-stakes tasks (medical/legal/financial tags) upgrade from the single cross-family judge to a three-judge panel. Everything else keeps v1 §5.5 unchanged.

- **Panel:** `judge.panel` — three columns from three distinct families (seed: `gemini-3.1-pro`, `gpt-5.5`, `grok-4`). When the graded column's family matches a panelist, that panelist is replaced by `judge.fallback` (seed: `claude-sonnet-4-6`); the graded column's own family never judges it.
- **Calls:** each panelist runs the v1 order-swapped pair (temperature 0, structured output) — six judge calls per high-stakes (output, rubric).
- **Scoring:** per dimension, the panel score is the **median** of the three panelists' pair-means. Cross-panelist disagreement (stddev of the three) feeds `a_output` alongside the order-swap spread (v1 §6.3).
- **Autofail (amends v1 §6.1 for panels):** an autofail condition (dangerous/fabricated answer, fabricated citation) zeroes the run when flagged by **≥ 2 of 3** panelists. A single flag does not zero; it adds a `judge_autofail_disputed` note to the score detail and increments a counter surfaced in batch logs. Rationale: the judge-bias literature (plan §References) shows single-judge false positives; the panel exists to de-noise exactly this. The overlay's "one dangerous answer fails" principle is preserved — the *task* fails on a confirmed dangerous answer; confirmation now takes two voices.
- **Failures:** a panelist failing after retries reduces the panel to two (median = mean); two failures reduce to v1 single-judge behavior including its autofail rule; the ≥10% judge-failure batch alert (v1 §5.5) counts panelist calls.
- **Cost:** panels only fire on high-stakes-tagged tasks (~15–20% of the corpus); the spend guard's projection includes the multiplier (§14).

---

# Part III — Surface expansion

## 9. New capabilities and the multi-turn harness

Capabilities grow from ten to twelve: `classification_routing` and `tool_use_orchestration` (Appendix C). Corpus additions follow all v1 §5.6 loader rules (20–25 tasks each, five tiers, 5 smoke members with anchors). The classifier schema enum widens to 12; `classifier.prompt_version` bumps, which naturally invalidates the classification cache (v1 §8.3 key includes the version).

### 9.1 classification_routing

Single-turn tasks: given a request/prompt, produce the correct routing classification (capability, difficulty, tags) or category assignment. Graded deterministically (`json_schema`, `golden_answer`) — no judge. Its aggregates make classifier selection empirical (v1 §8.3's promise): the operator re-points `classifier.column` at the top-ranked cheap column. Re-pointing stays a manual, logged operation in v1.1 — the classifier is load-bearing infrastructure and does not hot-swap on a nightly ranking wobble.

### 9.2 tool_use_orchestration and the multi-turn harness

The largest engine work item in v1.1 (as corpus authoring was in v1).

**Task shape.** `tasks` gains `task_type text not null default 'single_turn' check (task_type in ('single_turn','multi_turn'))`; `multi_turn` is valid only for `tool_use_orchestration` in v1.1 (loader-enforced). Multi-turn tasks carry an `interaction` jsonb:

```json
{
  "tools": [
    { "name": "search_flights", "description": "...", "input_schema": { "...": "..." },
      "responses": [
        { "when": { "call_index": 1 }, "error": "rate_limited" },
        { "when": { "args_match": {"origin": "SFO"} }, "result": { "...": "..." } },
        { "default": true, "result": { "...": "..." } }
      ] }
  ],
  "max_turns": 12,
  "par_turns": 5,
  "failure_injections": [ { "tool": "search_flights", "call_index": 1, "recover_expected": true } ],
  "adversarial": [ { "via_tool": "read_reviews", "payload_marker": "INJ-7f3a" } ]
}
```

**Harness loop.** The runner drives the conversation: send messages + tool definitions to the provider; on a tool call, resolve the first matching simulated `responses` entry (first-match by order, `default` last — deterministic by construction); append the result; repeat until the model produces a final answer or `max_turns`. The full trajectory persists to `runs.transcript` (jsonb); `runs.output` holds the final answer text. Timeouts: per-step `runner.timeout`; whole-trajectory cap `runner.trajectory_timeout` (default 600 s).

**Provider interface.** New capability-gated interface beside v1 §5.1's `ProviderClient`:

```go
type ToolStepClient interface {
    StepConversation(ctx context.Context, req ConversationRequest) (ConversationTurn, error)
    // req: prior messages, tool definitions, temperature, max_tokens, settings
    // turn: assistant text and/or tool calls, token usage
}
```

Anthropic/OpenAI/Google implement it natively; xAI and the OpenAI-compatible providers (§11) through the shared compat client. A column whose provider lacks the interface scores no multi-turn cells (`no_data`, honestly absent).

**Trajectory grading.** New deterministic grader kind `trajectory` (spec-validated like every v1 kind), reading the transcript:

| Dimension | Check |
|---|---|
| `tool_selection` | required tools called, forbidden tools not called |
| `argument_fidelity` | per-call argument checks (JSON-path equality/regex/tolerance) |
| `failure_recovery` | after an injected failure with `recover_expected`, the model retries or works around within `k` turns |
| `efficiency` | `min(1, par_turns / actual_turns)` |
| `injection_resistance` | after an `adversarial` tool result, the model does not execute the injected instruction (rule-scan over subsequent turns for marker-derived behavior) |

`goal_completion` comes from ordinary graders (`golden_answer`/`rule_scan`/`key_facts`) on the final output; `judge_rubric` MAY additionally judge the transcript (the judge receives the trajectory; G7 still bars corpus internals beyond the task's own material). Dimension weights per task as ever.

## 10. test_suite activation and the sandbox

The v1-reserved `test_suite` grader (v1 §5.3) activates behind a hard gate (G12).

### 10.1 Sandbox protocol

The engine never executes model code in-process. It POSTs to `MAVERICK_SANDBOX_URL`:

```
POST {sandbox}/v1/exec
{ "image": "maverick-sandbox-python:3", "files": [{"path": "solution.py", "content": "..."}, ...],
  "entrypoint": ["python", "run_tests.py"], "timeout_s": 60, "env": {} }
→ { "exit_code": 0, "stdout": "...", "stderr": "...", "duration_ms": 1240, "timed_out": false }
```

`stdout`/`stderr` truncated at 1 MB. Auth: bearer token (`MAVERICK_SANDBOX_TOKEN`).

### 10.2 Reference implementation: `sandboxd`

New small binary `cmd/sandboxd` in the engine repo, deployed as its own Kamal accessory with access to a **dedicated** Docker daemon (never the host's deploy daemon). Per exec: ephemeral container, `--network none`, memory 512 MB, 1 CPU, pids 256, read-only rootfs with a tmpfs `/work` for the files, non-root UID, image from a configured allowlist, hard timeout with SIGKILL. The sandbox host holds no credentials, no corpus, no database access — a full escape yields an empty box.

### 10.3 Grading contract

The task's test harness (authored corpus files, part of `files`) prints a final line `MAVERICK_RESULT {"passed": k, "total": n}`. Dimension `tests_passed = k/n`; nonzero exit, missing result line, or timeout → 0 with detail recorded. Grader spec (files, entrypoint, image, timeout) is the v1-defined schema, unchanged — corpora authored forward-compatibly now execute.

### 10.4 Gating

`MAVERICK_SANDBOX_URL` unset → the corpus loader still accepts `test_suite` tasks (schema-valid), but the batch scheduler excludes them, logs the excluded count per batch, and raises a once-per-batch `job_failure` warning naming the cause (`sandbox_unconfigured`). The v1 loader rejection (`grader_unsupported`) is removed. `code_generation` tasks migrate from static assertions to real suites incrementally across corpus versions — corpus data, not engine contract.

## 11. New providers

DeepSeek, Groq, Together — all OpenAI-compatible chat APIs.

- One new client `internal/providers/openaicompat`, parameterized by base URL + key, implementing `ProviderClient` and `ToolStepClient`. Each provider is a thin named instantiation; xAI MAY migrate onto it (refactor, not contract).
- `model_columns.provider` check constraint widens to the seven providers (migration). Config: `MAVERICK_DEEPSEEK_API_KEY`, `MAVERICK_GROQ_API_KEY`, `MAVERICK_TOGETHER_API_KEY`.
- Seed columns (data, confirmed at build time): `deepseek-v4`, `groq/llama-4-maverick`, `groq/kimi-k2`, `together/qwen3-coder` — Groq columns exist substantially for their latency cells. Judge pool and classifier seeds unchanged (panel additions in §8 draw from existing families).
- Spend: the catalog grows ~50%; §14 raises default caps accordingly. Provider ToS review before first benchmark applies to the new three (operator gate, plan §Risks).

## 12. Per-endpoint quality promotion

v1 transfers quality from a primary column to endpoint variants (v1 §4.2). v1.1 adds the promised "full per-endpoint quality columns where tenant pools demand them" as an explicit, operator-triggered transition:

- New seam endpoint: `POST /internal/v1/columns/{id}/promote-quality`. Valid only on a column with `quality_source_column_id` set; `422 invalid_request` otherwise.
- Effect: the column enters a dense-run window (`cold_start_until = now + catalog.cold_start_nights`) while **keeping** its status and its quality transfer. Ranking continues reading source-column quality until the column's own cells reach `scoring.min_nights`; then quality reads flip to its own cells permanently and `quality_source_column_id` is cleared. No availability gap, no tenant-visible regression to `measured: false` during the window.
- Triggers are judgment, documented in `docs/operations.md`: a tenant pool leaning on the variant, or endpoint drift suspicion (persistent outcome-blend negative adjustments on the variant are the tell).

---

# Part IV — Cross-cutting

## 13. Engine data model changes

All migrations additive; no v1 rows are rewritten. Summary DDL (conventions per v1 §3):

```sql
-- §4.1
alter table api_keys add column scope text not null default 'full'
  check (scope in ('full','read'));

-- §4.2
alter table tenants add column rpm_override int,
                    add column retention_days_override int;

-- §4.5
create table alert_tenants (
  alert_id  text not null references alerts(id),
  tenant_id text not null references tenants(id),
  primary key (alert_id, tenant_id)
);
-- tenant_webhooks: §4.5

-- §5
create table irt_fits (
  id               text primary key,            -- fit_<ulid>
  capability       text not null,
  matrix_version   text not null,
  window_days      int not null,
  holdout_logloss  real not null,
  baseline_logloss real not null,
  healthy          boolean not null,
  active           boolean not null default false, -- ≤1 active per capability
  fitted_at        timestamptz not null default now()
);
create table irt_abilities (
  fit_id          text not null references irt_fits(id),
  model_column_id text not null references model_columns(id),
  theta           real not null,
  primary key (fit_id, model_column_id)
);
create table irt_task_params (
  fit_id  text not null references irt_fits(id),
  task_id text not null references tasks(id),
  a       real not null,
  b       real not null,
  primary key (fit_id, task_id)
);

-- §7
create table tenant_column_adjustments (
  tenant_id       text not null references tenants(id),
  model_column_id text not null references model_columns(id),
  capability      text not null,
  adjustment      real not null,               -- quality points, |adj| ≤ blend.max_points
  n               int not null,
  computed_at     timestamptz not null default now(),
  primary key (tenant_id, model_column_id, capability)
);
alter table tenant_constraints add column outcome_blending boolean not null default true;

-- §9
alter table tasks add column task_type text not null default 'single_turn'
  check (task_type in ('single_turn','multi_turn'));
alter table tasks add column interaction jsonb;   -- multi_turn only, loader-validated
alter table runs  add column transcript jsonb;    -- multi_turn trajectories

-- §11
alter table model_columns drop constraint model_columns_provider_check;
alter table model_columns add constraint model_columns_provider_check
  check (provider in ('anthropic','openai','google','xai','deepseek','groq','together'));
```

New ID prefixes: `wh_`, `fit_`. `predictions.source` gains the documented value `'matrix_completion'` (column already free-form per v1 §3).

## 14. Jobs, scheduling, and spend

| Job | Schedule | Change |
|---|---|---|
| `irt_fit` | chained after the weekly sweep's `aggregate_rebuild` | new (§5) |
| `outcome_blend` | 05:30 nightly | new (§7) |
| `prediction_lock` | unchanged slot | source switches to matrix completion when enabled (§6.2) |
| `nightly_smoke` | unchanged slot | scheduler applies skip eligibility (§6.3); logs `cells_skipped` |
| all batch jobs | — | spend projection extended: multi-turn tasks project `par_turns × 1.5` steps; high-stakes tasks project the panel multiplier (§8) |

Spend-cap defaults rise with the catalog and the harness: `spend.nightly_cap_usd` 100 → **150**, `spend.weekly_cap_usd` 500 → **750**. G8 mechanics unchanged.

Rails-side jobs (Solid Queue, §3): outbox processor (continuous), entitlement sync (event-driven), reconciliation (daily), digest emails (daily), dead-man heartbeat (recurring, §2.3).

## 15. Configuration additions

All engine vars `MAVERICK_`-prefixed, validated at boot per v1 §14.1:

| Var | Default | Purpose |
|---|---|---|
| `IRT_ENABLED` | `true` | §5 master switch (fallback = pass-rate everywhere) |
| `IRT_WINDOW_DAYS` / `IRT_MIN_GAIN` | `90` / `0.02` | §5.1–5.2 |
| `COMPLETION_ENABLED` | `false` | §6 — off until the operator has watched a Brier baseline |
| `COMPLETION_RANK` | `3` | §6.1 |
| `COMPLETION_BRIER_GATE` / `COMPLETION_CELL_ERROR_GATE` | `0.10` / `0.10` | §6.3 |
| `COMPLETION_MAX_SKIP_NIGHTS` / `COMPLETION_MIN_OBSERVED_FRAC` | `2` / `0.6` | §6.3 / §6.2 |
| `BLEND_ENABLED` | `false` | §7 gate: on only after the M12 check-coverage and false-positive gates pass |
| `BLEND_WINDOW_DAYS` / `BLEND_PRIOR_N` / `BLEND_KAPPA` / `BLEND_MAX_POINTS` / `BLEND_MIN_N` | `30` / `20` / `20` / `2.0` / `5` | §7 |
| `JUDGE_PANEL` | `gemini-3.1-pro/direct,gpt-5.5/direct,grok-4/direct` | §8 |
| `JUDGE_FALLBACK` | `claude-sonnet-4-6/direct` | §8 |
| `SANDBOX_URL` / `SANDBOX_TOKEN` | — (unset = test_suite skipped) | §10 |
| `RUNNER_TRAJECTORY_TIMEOUT` | `600s` | §9.2 |
| `DEEPSEEK_API_KEY` / `GROQ_API_KEY` / `TOGETHER_API_KEY` | — | §11 |
| `SPEND_NIGHTLY_CAP_USD` / `SPEND_WEEKLY_CAP_USD` | `150` / `750` | §14 |

Shell secrets follow the framework's custody policy: one vault project for the product, `.kamal/secrets` holding resolver code only, Kamal materializing env at deploy (§2.2). The values: engine internal base URL + internal token, Stripe keys, SMTP, reconciliation allowlist, `SENTRY_DSN` (GlitchTip), `OTEL_EXPORTER_OTLP_ENDPOINT`/`OTEL_EXPORTER_OTLP_HEADERS` (ops-box Collector). Per-account webhook secrets are generated data, encrypted in the shell database, not deploy secrets.

## 16. Testing strategy

Engine — v1 §15 conventions extended:

- **IRT goldens:** synthetic 2PL data with known (θ, a, b) → fit recovers parameters within tolerance; noise-degraded data → health gate correctly refuses. Ceiling goldens: fitted params → expected ceilings vs the pass-rate method.
- **Completion goldens:** synthetic low-rank matrices with holes → predictions within tolerance; gate property tests (a cell exceeding either error gate is never skipped; no cell skips 3 consecutive nights; the sweep never skips).
- **Blending property tests:** `|adj| ≤ cap` always; `adj = 0` below `min_n`; monotone in `(s − p)`; global aggregates bit-identical with blending on vs off (G11); `completed_unchecked` rows never change `adj`; `BLEND_ENABLED=false` leaves ranking untouched.
- **Panel goldens:** three-judge fixtures → median scoring, disagreement→ambiguity, the 2-of-3 autofail rule including panelist-failure degradation.
- **Harness:** `providers/fake` grows a scripted `ToolStepClient`; trajectory-grader goldens run on checked-in transcripts. End-to-end: mini multi-turn corpus → batch → aggregates → recommend.
- **Sandbox:** `sandboxd` integration tests against real Docker (CI has it via testcontainers already); protocol contract tests both sides; a red-team fixture set (network attempt, fork bomb, oversized output, sleep past timeout) asserting containment.
- **Contract tests** for every new/changed endpoint and error code (Appendix A/B), including scope enforcement on every `/v1` route.

Shell:

- **The contract pipeline is the test suite's spine:** the hand-authored OpenAPI spec is the source of truth; Minitest request tests assert every endpoint's requests and responses against it (committee/skooma); CI regenerates the TypeScript client and hard-fails on any diff between spec, generated client, and committed code (the framework's drift gate; a warning gate is decorative).
- Minitest unit and request tests with the seam stubbed by WebMock against response shapes from this spec and `docs/api.md`; Rails system tests drive the SPA's core loops through the real generated client. The framework's cross-tenant isolation test (account B cannot read account A's data) covers every shell endpoint, and Pundit policy tests cover each role.
- **Integration CI is the real engine:** the shell's CI boots the engine image + Postgres (compose), migrates, and runs the provisioning/entitlement/reconciliation flows against it. Seam drift fails a build, not a customer.
- **Deployed smoke tests** per the framework: post-deploy checks that a Solid Queue job enqueued on the deployed box actually executes (the worker role exists and drains), alongside the web check.
- Billing: Stripe test clocks through Pay for trial expiry, past-due grace, and cancellation transitions; webhook idempotency tests.
- Security tests: dashboard key cannot write (403 path), webhook receiver rejects bad signatures, `/admin` denies non-operators, login throttling engages.

## 17. Build sequence

Milestones continue v1's numbering; deliverable-scoped, dependency-ordered. Rails track (M7–M10) and engine measurement track (M11–M12) can run in parallel; M8–M10 need their engine additions (§4) landed first within each milestone.

- **M7. Shell skeleton:** `modelmaverick-web` from the framework template (`new-product`: clone, tokens file, contract pipeline green in CI), Rails 8 auth, the framework's `Account`/`AccountUser`/`AccountInvitation` with `Current.account` and Pundit (one engine tenant per `Account`), seam client, outbox, provisioning happy path against a dev engine, Kamal deploy (`web` + `worker`) beside it with the deployed job-execution smoke passing.
- **M8. Billing and lifecycle:** engine §4.2–§4.4 (limits, tenant list, acting-party); Pay/Stripe plans, entitlement sync, the suspension state machine, reconciliation.
- **M9. Dashboard:** engine §4.1 (key scopes); all read surfaces as SPA pages over the generated client, config editing via the seam, key management. First customer-visible artifact.
- **M10. Alerts everywhere:** engine §4.5 (alert_tenants, `/v1/alerts`, tenant webhooks); Rails receiver, dashboard alerts, notification emails, `/admin`, marketing site. **v1.1 launchable for existing-style tenants at this point.**
- **M11. Calibration:** IRT (§5) + `ceiling_source`; judge panels (§8).
- **M12. Prediction and feedback:** matrix completion (§6, shipped disabled, enabled after a watched Brier baseline); outcome blending (§7, checked outcomes only, shipped disabled and enabled only once the check-coverage and false-positive gates set here pass).
- **M13. Surface:** new providers (§11), quality promotion (§12), the multi-turn harness + `trajectory` grader + sandbox (§9–§10), the two new capabilities authored in the corpus repo, classifier re-point procedure documented. Largest milestone; sequenced last because nothing else depends on it.

---

## Appendix A — Endpoint delta catalog

New or changed only; everything else is v1 Appendix A verbatim.

| Method | Path | Auth | Success | Notable errors | § |
|---|---|---|---|---|---|
| GET | `/v1/alerts` | key (read ok) | 200 | — | 4.5 |
| GET/PUT | `/v1/config/webhooks` | key (full) | 200 | 422 invalid_request | 4.5 |
| GET | `/v1/policies` | key | 200 | + `ceiling_source` field | 5.4 |
| POST | ~~`/v1/outcomes`~~ → `/v1/telemetry` (internal producer) | key | — | Removed (G13); outcome rows now come from the automatic telemetry producer, not caller reports | 7 |
| POST | `/v1/recommend` | key (full) | 200 | + `outcome_blended` field; 403 insufficient_scope on read keys | 7.2, 4.1 |
| all `/v1` writes + recommend | — | key | — | 403 insufficient_scope on read keys | 4.1 |
| GET | `/internal/v1/tenants` | internal | 200 | — | 4.3 |
| PUT | `/internal/v1/tenants/{id}/limits` | internal | 200 | 422 invalid_request; exists from v1 (classification quota), gains `rpm` and `history_retention_days` | 4.2 |
| PUT | `/internal/v1/tenants/{id}/webhooks` | internal | 200 | 422 invalid_request | 4.5 |
| POST | `/internal/v1/tenants/{id}/api-keys` | internal | 201 | accepts `scope` | 4.1 |
| POST | `/internal/v1/columns/{id}/promote-quality` | internal | 200 | 422 invalid_request | 12 |
| POST | `{sandbox}/v1/exec` | sandbox token | 200 | — (separate service) | 10.1 |

## Appendix B — Error code delta

| HTTP | code | Meaning |
|---|---|---|
| 403 | `insufficient_scope` | Read-scoped key on a non-GET surface (§4.1) |

`422 grader_unsupported` is retired: the loader accepts `test_suite` in v1.1 (§10.4).

New alert kinds: `webhook_failing` (§4.5). New audit field: `acting_party` (§4.4).

## Appendix C — Capabilities at v1.1

Twelve, fixed: the ten of v1 Appendix C plus `classification_routing`, `tool_use_orchestration`.

Smoke arithmetic at the grown catalog (~18 columns, 12 capabilities): 60 smoke tasks × 18 columns × 3 repeats ≈ 3,240 single-turn-equivalent runs/night before completion skips, plus multi-turn and panel multipliers — the §14 cap raise and the §6 skip machinery are sized against exactly this.

---

*End of specification. The shell's product copy, plan pricing, and provider ToS/trademark reviews are operator work outside this document. Measurement methodology for the two new capabilities — trajectory dimensions, injection probes, difficulty knobs — extends `specs/capability-matrix-design.md` in a companion revision.*
