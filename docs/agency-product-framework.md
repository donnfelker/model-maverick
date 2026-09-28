# Agency Product Framework

**Purpose:** Ship 10+ web products per year, for clients and for ourselves, with zero per-product decisions about stack, design, deployment, observability, or AI tooling. Every decision below is made once, encoded in a template repo and a set of agent skills, and inherited by every product.

**Status:** v1 strategy. Extracted from existing infrastructure (Kamal, VPS hosting, Rails, Docker) and proven patterns, not designed speculatively.

---

## 1. Operating Principles

These override everything else in this document when they conflict.

1. **Extract, don't design.** The framework codifies what has already worked across real products. Anything speculative gets cut. New standards enter the framework only after two or more products prove the pattern.
2. **Standards over adapters.** No custom abstraction layers. Vendor swappability comes from industry standards: OpenTelemetry for observability, the Sentry protocol for errors, S3-compatible storage, SMTP for mail, OTLP for LLM traces, OpenAPI for the API contract (the spec format is the contract; the tooling — the conformance gem, openapi-typescript/orval — is replaceable behind it). Swapping a vendor is never an application code change — an env var for traces and errors, a per-box shipper/Collector config change for logs and metrics (Section 6).
3. **One stack, no exceptions.** Two stacks doubles template maintenance and splits agent conventions. Every product uses the locked stack in Section 2. A product that genuinely cannot fit the stack is a signal to decline the project, not to fork the framework.
4. **Ship through it to validate it.** The framework is validated by shipping product #1 through it end to end, not by polishing the template. The template is built only to the point where product #1 can ship through it; everything else is finished against real usage.
5. **Only backport security fixes.** Shipped products do not chase template parity. At 10+ products per year, drift is managed by policy, not by heroics. The policy presupposes a discovery mechanism, so it ships with one: template CI runs a dependency audit (bundler-audit, npm audit) routed to the fleet-health alert channel, and the `new-product` skill appends each product's Ruby/Rails/key-gem versions to a one-page fleet inventory — "which products need this backport" is a lookup, not an investigation.
6. **The agent is the operator.** Every convention in this document exists so an agent can execute it without a human decision. If a rule can't be followed by an agent reading CLAUDE.md, it isn't a rule yet.

---

## 2. The Stack (Locked)

| Layer | Choice | Notes |
|---|---|---|
| Backend | Ruby on Rails 8, **API mode** | `config.api_only` with the cookie, session, and flash middleware re-enabled — the SPA signs in with cookie sessions and the admin/ops engines render HTML. Auth, jobs, caching, mailers built in |
| Frontend bridge | Rails JSON API + same-origin SPA | A hand-authored OpenAPI spec is the contract; Minitest request tests assert conformance to it; TypeScript client generated from the spec. Browser auth is Rails cookie sessions — no JWT, no token auth in the browser (see "The API surface" below) |
| Frontend | React + TypeScript | Vite-built, served by Rails from the same image; TanStack Query over generated hooks for all data fetching. Covers the rich-UI half of the product mix; largest hiring pool for client handoff |
| API contract | OpenAPI spec-first + committee/skooma conformance tests + openapi-typescript/orval codegen | CI hard-fails on drift in either direction. The spec is the source of truth |
| Styling | Tailwind CSS + shadcn/ui | Themed entirely through the design token file (Section 4) |
| Database | Postgres | Rails 8 multi-database layout: primary plus dedicated `queue` / `cache` / `cable` logical databases (each with its own migration path) backing Solid Queue / Solid Cache / Solid Cable. No Redis until a product proves the need |
| Identity & tenancy | Accounts-first model on Rails 8 auth | `Account` / `AccountUser` (jsonb roles) / `AccountInvitation`; `Current.account` per request; tenant-owned models inherit `AccountRecord` (acts_as_tenant). Every product, day one — single-user products just hide the switcher |
| Authorization | Pundit | Policies receive `Current.account_user` as `pundit_user` — every authorization decision reads the membership and its roles, so the same user authorizes differently per account |
| Background jobs | Solid Queue | **The** job processor, no alternatives per product. Postgres-backed, zero extra infrastructure; Mission Control Jobs dashboard at `/admin/jobs`; `bin/jobs` runs the worker |
| WebSockets | Action Cable via Solid Cable | Chat streaming and realtime updates (Section 5); Postgres-backed, same no-Redis rule |
| Transactional email | Action Mailer → per-client provider | Development delivers to Mailbin at `/mailbin`, never a real send; production provider is one config value + one credential (see "Transactional email" below) |
| Admin | Madmin + Mission Control Jobs | `/admin`, gated by the user admin flag: CRUD on template models, job dashboard, impersonation |
| File storage | Active Storage → S3-compatible service | One private bucket per product; endpoint and credentials via env; local disk only in dev/test |
| Packaging | Docker | Single image per app; the shared artifact across every deploy tier |
| Deploy (tier 1) | Kamal to a VPS (Hetzner, DigitalOcean, etc.) | Section 7 |
| AI (in-process) | RubyLLM | Section 5 |
| AI (sidecar, when triggered) | TypeScript service (Hono/Express) | Section 5 |

### Why Rails and not full-stack TypeScript

For products that are CRUD, dashboards, background jobs, and an API, Rails remains the fastest solo-shipping stack available, and agents write it well. The honest risks were client handoff (thin Rails hiring pool) and rich frontends (Hotwire's weakness). The React SPA resolves both: the layer clients most often want to modify is React/TypeScript, and rich UI is native React. Next.js was rejected because background jobs there require bolting on BullMQ + Redis + a separate worker, all of which Rails provides free.

**Why a standalone SPA doesn't reopen the Next.js question.** With the frontend a standalone React SPA, "why not make the backend TypeScript too" looks reopened. It isn't, because the rejection never depended on the bridge layer. Next.js has no first-party background job system and reintroduces Redis twice (BullMQ for jobs, a custom cache handler for multi-replica ISR). Its support policy retires previous majors quickly, which breaks a security-backports-only drift policy across a growing fleet. Its router, server component, and caching semantics have churned across recent majors, so agents produce plausible code from the wrong era, a failure mode no CLAUDE.md rule can fully prevent. Self-hosting it under Kamal works, while fighting the framework's economic pull toward Vercel. Full analysis, steelman, and rerun triggers: `why-not-nextjs.md`.

### Why same-origin API + SPA and not Inertia or Hotwire

Hotwire fights the rich-UI half of the product mix and forfeits the React component libraries (shadcn, Radix, TanStack Table) that make the volume target feasible. Inertia was the closest contender — it eliminates the per-feature API contract entirely — and lost for two reasons: an API-first template makes mobile apps and partner integrations an extension rather than a retrofit, and agents have seen far more SPA + JSON API code than Inertia-on-Rails, which reduces per-product correction work in an agent-operated shop. The known cost of the SPA pattern, silent contract drift across the HTTP boundary, is paid once in the template with a spec-first OpenAPI contract enforced by conformance tests, generated TypeScript clients, and a hard-failing CI drift gate. The "Why Rails and not full-stack TypeScript" rationale is unaffected: the jobs-and-API argument never depended on the bridge layer.

### The API surface

The product's own UI is the first consumer of the API: the SPA calls the `/api/v1` namespace same-origin, authenticated by Rails cookie sessions (httpOnly, secure, SameSite, CSRF-protected) — no JWT, no bearer tokens in the browser (next subsection). The contract machinery is non-negotiable and lives in the template: the OpenAPI spec is the hand-authored source of truth for every endpoint, and Minitest request tests assert each endpoint's requests and responses against it (committee/skooma), so behavior cannot drift from the spec in either direction without a red build; TypeScript client types and hooks are generated from the spec (openapi-typescript or orval), TanStack Query consumes those generated hooks for all browser data fetching, and a CI gate regenerates and diffs — any drift between spec, generated client, and committed code hard-fails the build, because a warning gate is decorative and agents will route around it. Non-browser consumers (partner integrations, mobile clients, webhooks) reuse the same controllers: token/API-key auth is added alongside cookie sessions when the first paying non-browser consumer arrives (Rails 8 auth generator, or Doorkeeper if OAuth is required), never replacing them. Public identifiers are prefixed IDs (`acct_…`, `user_…` — Stripe-style): API paths and payloads never expose raw integer primary keys, lookups go through `find_by_prefix_id`, and an ID pasted into a support ticket says what it is. Default-deny CORS (nothing to configure while everything is same-origin), rate limiting, an idempotency-key convention on mutating endpoints, conformance request tests, and the OpenAPI spec in the handoff package all stand. An agent adding an endpoint starts in the OpenAPI spec, asserts conformance in a request test, and regenerates the client in the same commit; it never invents auth.

**Rails runs in API mode** (`config.api_only = true`): controllers inherit the lean API stack, no view layer accretes around the JSON surface, and the defaults match what the app actually is. Three middleware groups are deliberately re-enabled, with a comment saying why: cookies + session store (the SPA authenticates with cookie sessions), CSRF protection on session-authenticated requests, and flash (the admin and ops engines — Madmin, Mission Control Jobs, Mailbin in dev — render HTML and expect it). Static file serving stays on so Rails serves the built SPA from `public/`. This is template configuration, decided once; an agent never toggles middleware per product.

**Inbound webhooks are a template pipeline, not per-product improvisation:** an `InboundWebhook` record captures the raw payload, the endpoint returns 200 immediately, a job verifies and processes it asynchronously, and processed records are incinerated after a retention window. A generator scaffolds the controller + job pair for each new provider, so "add a Stripe/Postmark/partner webhook" is a generator invocation with the processing logic as the only per-product code.

### Why cookie sessions and not JWT

Recorded so this does not get relitigated per product. JWTs solve cross-origin and non-browser auth; this architecture is same-origin by design, so a JWT adds cost without benefit:

- **Storage.** A token in localStorage is readable by any injected script — an XSS bug becomes credential theft. The standard mitigation is an httpOnly cookie, which is sessions rebuilt with extra steps.
- **Revocation.** Stateless tokens cannot be invalidated before expiry. Production systems end up with a denylist table, hitting the database per request anyway, which deletes the statelessness that justified the token.
- **Refresh flow.** Access tokens, refresh tokens, rotation, and concurrent-refresh races form an entire subsystem and a new class of agent-written bugs.

Rails sessions provide httpOnly, secure, SameSite cookies, CSRF protection, and server-side revocation with zero custom code. The "SPA means bearer tokens" pattern belongs to split-origin architectures this framework rejected — even Next.js-style stacks default to cookie sessions (Auth.js). When the first non-browser consumer arrives, a token scheme is added in an additive `/api` auth strategy (Rails 8 auth generator tokens, or Doorkeeper if OAuth is required), coexisting with cookie sessions: same controllers, different authentication strategy per consumer.

### Auth hardening: two-factor, sudo mode, and social login

All three ship in the template on top of Rails 8 auth, exposed to the SPA as endpoints in the OpenAPI spec like everything else. Rails-native `rate_limit` guards the sign-in and sign-up endpoints.

- **TOTP two-factor with backup codes.** Standard TOTP: a provisioning URI and QR code for the authenticator app at enrollment, sixteen single-use backup codes, and timestep replay protection — the last accepted OTP timestep is stored so a captured code cannot be replayed inside its validity window. When 2FA is enabled, password sign-in returns a two-factor challenge instead of a session; the SPA renders the OTP step and completes the same session flow.
- **Sudo mode.** Sensitive endpoints — password and email changes, disabling 2FA, API token management, account ownership transfer, account deletion — require recent re-authentication: a sudo challenge prompts for the password (or OTP), stamps the session for a bounded window (~30 minutes), and the SPA surfaces it as a re-auth modal. The rule lives in CLAUDE.md so an agent adding a new sensitive endpoint marks it sudo-gated instead of deciding case by case.
- **Social login and connected accounts.** OmniAuth (with its Rails CSRF protection gem), providers per product: Google, GitHub, Microsoft, Facebook, X — each enabled only when both its config flag and its credentials exist. Three flows share the wiring: register with a provider, connect a provider to an existing user, and sign in. Identities live in a `ConnectedAccount` record with encrypted access/refresh tokens and automatic refresh — which also makes it the storage for any OAuth integration a product needs beyond login (a user's Google Drive, a workspace's Slack), not just identity.

### The account model

Every product is multi-account from day one — template schema, not per-product design. Two facts force this: retrofitting `account_id` across every tenant-owned table of a shipped product with live data is a migration under production traffic, times however many products shipped without it; and without a template model, agents improvise a different tenancy per product, where the missing-scope query is a silent cross-tenant data leak. The degenerate case costs nothing — an account with one admin member behaves like a single-user app.

| Table | Columns that matter | Constraints |
|---|---|---|
| `accounts` | `name`, `owner_id`, `personal` flag, membership counter cache | index on owner |
| `account_users` | `account_id`, `user_id`, `roles` jsonb (default `{}`) | unique (account_id, user_id) |
| `account_invitations` | `account_id`, `invited_by_id`, `token`, `name`, `email`, `roles` jsonb | unique (account_id, email); unique token |

The model is built directly on Rails 8's built-in authentication and its cookie sessions (above): the auth generator's `Current` — `session`, with `user` delegated — is extended with `account`, membership resolution, and role helpers, one CurrentAttributes object with no parallel context; and registration, which the generator leaves to the app so the template owns it, is where the default account is created (or an invitation token is accepted instead, creating a membership rather than a fresh account). A `require_account` guard composes with the generator's `require_authentication`, and when token auth for non-browser consumers arrives, tokens resolve to the same membership and role checks — account scoping never varies by auth scheme. `Account` is the tenancy root. Users join accounts through `AccountUser` memberships carrying jsonb roles (`[:admin, :member]` baseline, extended per product in the spec interview), so one email address belongs to many accounts with a different role in each — the same person is a gym owner in one tenant and a member in another. The account creator becomes admin automatically, and single-user products run the same model with the switcher hidden — team support is later *enabled*, not *built*. Invitations are token-based, unique per (account, email), with roles assigned at invite and copied onto the membership on accept — inviting is the role-granting flow, nothing separate to design per product. Authorization is Pundit, wired once: policies receive `Current.account_user` as `pundit_user`, so every authorization decision reads the membership and its roles — never bare user identity — and adding a role to a product means extending `ROLES` and writing policy methods, not inventing an auth pattern. Team ownership is transferable without support surgery: a template flow (owner-only, sudo-gated, teams with two or more members) moves `owner_id`, grants the new owner an admin membership, and notifies both parties. `Current.account` is set per request from the session and resolved through the user's memberships only — never from a client-supplied ID without a membership check; switching accounts is an authenticated endpoint that updates the session (the documented escalation, when multi-account-tabs workflows appear, is a Basecamp-style `/:account_id/` path prefix). Tenant-owned models inherit `AccountRecord` — an abstract base class carrying `acts_as_tenant :account` — so account scoping is enforced by inheritance, not remembered per query, and every tenant-owned table carries `account_id`. The account switcher, members screen, and invitation flow are template SPA components consuming template endpoints in the OpenAPI spec through generated hooks. A cross-tenant isolation regression test — account B cannot read account A's data through any template endpoint — ships in template CI.

### Transactional email

Development and test never send real mail. Development delivers to **Mailbin**: every outbound email is captured and browsable at `/mailbin` (the engine is mounted in local environments only), so "did the invite email render correctly" is a browser tab, not a mail account. Test uses the test adapter.

Production selects the deliverability provider **per client** with one config value and one credential — no application code. Postmark, Mailgun, Resend, and MailPace get first-class API delivery methods (the provider's gem loads conditionally; settings come from credentials); SendGrid, SES, Mailjet, and anything else ride the SMTP fallback with per-provider presets (port 587, STARTTLS). SMTP remains the standards floor per Principle 2 — any provider works with zero gems — and the API delivery methods are a one-gem upgrade where the provider offers one. Mailer URL hosts are set per environment (`lvh.me` + port in development, the product domain in production and staging). The chosen provider, from-address, and credential land in the handoff package's env inventory, so a client taking over deliverability rotates one credential and owns their sending reputation.

### Application plumbing (in the template)

Product features every SaaS ends up needing, built once:

- **Notifications.** Noticed: notifier events scoped to the account, per-recipient database notifications, email delivery where the notifier declares it, and an Action Cable (Solid Cable) nudge for realtime. The SPA's notification bell reads `/api/v1/notifications` through generated hooks with unseen counts; the Cable event triggers a refetch, so the payload of record stays behind the API contract.
- **Announcements / changelog.** An `Announcement` model (rich text, draft/published/scheduled) managed in Madmin and surfaced in the SPA as a "What's new" panel with an unread badge driven by the user's `announcements_read_at`. Product updates reach users without a deploy or an email blast.
- **Referrals.** Referral codes and links per user, cookie capture at signup, and a completion hook that fires when the referred user converts. What "converts" means is per-product (first payment, an activation milestone) since billing is not in the template; the tracking plumbing is.
- **Internationalization.** Ships English-only, structured for localization: Rails-side strings (mailers, API errors, validation messages) go through I18n with nested locale load paths and `raise_on_missing_translations` in development; locale resolution is param → user preference → `Accept-Language` header → default. The SPA keeps its strings in a locale-keyed message catalog following the same resolution order. Localizing a product for a client is translation files, not a refactor.
- **Outbound API clients.** Every third-party HTTP integration gets a client class: an `ApplicationClient` base (JSON in/out, auth header, typed error mapping — a 422 raises, it never returns quietly) plus a generator that produces `app/clients/<service>_client.rb` with a test stub. Inline HTTP in jobs, models, or controllers is forbidden; an agent integrating a new service starts from the generator, and every product's integrations end up shaped the same way.

---

## 3. Spec Process (Not Spec Document)

A comprehensive up-front spec is where speed goes to die. The framework replaces it with a 30-minute agent-run interview that produces a spec with a hard cap of two pages:

- Problem statement
- User roles (the per-account role vocabulary beyond the built-in admin/member — the account and membership machinery already exists in the template)
- Core loop (the one workflow that must work)
- Data model sketch
- Explicit non-goals for v1
- "Done" criteria for v1

The `spec-interview` skill (Section 8) runs the interview, forces the decisions, and writes the artifact into the repo. Anything the spec doesn't cover is decided during the build by convention, not by meeting.

---

## 4. Design Token System

One-time structural setup, then one small file per product.

**Structure (built once, in the template):** Every component references CSS variables only: `--primary`, `--background`, `--border`, `--radius`, `--font-sans`, chart palette, density scale. shadcn is token-based out of the box; the template adds brand fonts and a few custom variables.

**Per product:** The entire visual identity lives in `app/frontend/styles/tokens.css`. Swap the file and every button, card, table, and dialog restyles itself.

**Workflow:** At kickoff, the `design-tokens` skill takes brand inputs (logo, one or two hex values, a reference product like "feels like Linear," font preference) and generates the token file: full palette with hover/muted/destructive variants, dark mode values, type scale, radius. It renders a style-check page (buttons, forms, cards, a table, alerts) for a two-minute visual review.

**Two rules that keep it alive:**

1. Components may only use tokens and the existing component set. Inline values like `bg-[#3b82f6]` are forbidden. Enforced in CLAUDE.md and by a lint check.
2. Tokens are for theming, not layout. Custom page structures and new components are normal product work; new components get built once in the template using tokens and every future product inherits them.

---

## 5. AI / Agent Features in Products

**Default: in the monolith, with RubyLLM.**

- Background analysis agent = a Solid Queue job calling RubyLLM with tools. Retries, scheduling, and concurrency come free from the queue.
- Chat = streamed tokens to a React chat component over **ActionCable via Solid Cable** (already in the stack), not SSE. This is a decided default, not a preference: `ActionController::Live` SSE holds a Puma thread — and typically a DB connection — for the entire LLM generation (10–60s per message), so a handful of concurrent chats can saturate web capacity on a tier-1 box and hang the whole app, and kamal-proxy ships with response buffering enabled by default, a failure dev never surfaces because dev doesn't run kamal-proxy. If a product has a hard SSE requirement, the runbook requires all three: `proxy.buffering.responses: false` for that app, explicit Puma thread sizing with a documented concurrent-stream cap, and a deployed-through-kamal-proxy streaming smoke test in CI. Broadcasts are batched: the template's streaming helper coalesces LLM output on a ~150ms interval before broadcasting, because Solid Cable turns every broadcast into a Postgres write — per-token fan-out at even eight concurrent chats (~160 writes/sec) degrades the same database serving web, queue, and cache. Concurrent chat sessions per tier-1 box are capped, a deployed chat load smoke test lives in CI, and Redis-backed Action Cable is the documented escalation when measured broadcast volume exceeds the Postgres budget (this is exactly the "product proves the need" trigger from Section 2). Ordering is part of the protocol, not an accident: Action Cable delivers concurrently and can reorder updates — RubyLLM's own streaming docs warn about it — so every broadcast carries a monotonically increasing revision per message and the full accumulated content at that revision — snapshots, not token deltas, which is what makes discarding stale revisions correct — the client discards stale revisions, and completion triggers a final fetch of the persisted message so the rendered text always converges to stored truth. The shared chat component implements this once; a test simulates delayed delivery and asserts no mid-stream regression, not just post-completion convergence. The chat surface is part of the OpenAPI contract like everything else: message endpoints and the completion fetch go through the generated client and TanStack Query, and streaming endpoints the conformance tests can't fully exercise (the Cable channel, or SSE where the runbook applies) still get a documented spec entry, not an exemption from the contract. API keys never leave the server; the browser only talks to the Rails endpoint.
- RubyLLM is multi-provider (Anthropic, OpenAI, Gemini, Bedrock) behind one interface, so the model provider is a config swap, consistent with Principle 2.

**Template conventions:** an `app/ai/` directory (agents, tools, prompts as versioned objects, never strings scattered through jobs), a `Tool` base class, a streaming chat controller, and a generic token-themed React chat component. Chats, messages, and agent runs are account-owned (`AccountRecord`), and every AI job takes `account_id` in its arguments and sets tenant context before its first query. Adding an AI feature is a skill invocation, not an architecture discussion.

**Queue isolation defaults:** LLM jobs are long-running and hold workers while waiting on a slow external dependency. The template ships Solid Queue with its own connection pool and per-queue concurrency caps, and every LLM call sets RubyLLM's `request_timeout` explicitly, well under its 300-second default — that timeout, not a queue-level knob, is what bounds a stuck LLM call. Solid Queue has no per-job execution timeout, and `Timeout.timeout` inside jobs is banned as thread-unsafe. Together these keep a burst of agent jobs from starving web requests of database connections. The template ships the Rails 8 multi-database layout — queue, cache, and cable each write to their own logical database with their own migration paths (Section 2) — so the documented escalation is moving the queue database to its own server: a runbook trigger, not a rebuild.

**Concurrency and orchestration in Ruby (the `async` gem):** RubyLLM is automatically non-blocking under the `async` gem's fiber scheduler, so fan-out inside a single job — parallel analyses, batch embeddings, concurrent agents — is an `Async` block, not new infrastructure, and tool execution supports `concurrency: :fibers`. Rate limits are enforced with async semaphores, not sleep-and-retry. **The fiber-safety rule that makes this survivable:** Rails defaults to thread-based ActiveRecord connection isolation, so sibling fibers share one leased connection and interleaved queries produce intermittent connection-state errors that look like DB flakiness. The hard rule, in CLAUDE.md: inside `Async` fan-out blocks, LLM/HTTP I/O only — no ActiveRecord; collect results and persist after fan-in — and DB-touching tools do not get `concurrency: :fibers`. The documented unlock, if a product genuinely needs DB work inside fibers, is `config.active_support.isolation_level = :fiber` (Rails 7.2.1+/8.x), enabled only after one-time verification against Solid Queue and CurrentAttributes in the template. Multi-step and multi-agent orchestration also stays in Ruby: RubyLLM exposes the agentic loop as discrete verbs (`step` until `complete?`, with `generate`/`run_tools` underneath), so one turn per Solid Queue job with the chat persisted between steps gives resumable agents with iteration budgets, and its documented workflow patterns — sequential, routing, parallel, evaluator, handoffs — are plain Ruby classes, no framework. When a product's LLM job volume pins thread-based workers (each thread 99% idle waiting on tokens), the documented escalation is routing LLM jobs only to `async-job` via a `self.queue_adapter = :async_job` base class while Solid Queue keeps everything else; under Puma that processor is Redis-backed and runs as its own always-on dequeue process (`async-job-adapter-active_job-server` — a new Kamal role running the app image with a different command), which is one more explicit "product proves the need" Redis trigger, not a default.

**Escape hatch: the TypeScript sidecar.** Triggered only when a product needs an SDK or capability that does not exist in Ruby: the Claude Agent SDK, computer use, or similar. Multi-step or multi-agent orchestration alone is not a trigger — RubyLLM's step-driven loop and workflow patterns cover that in-process (above). Implementation: a small stateless Hono/Express service in the same repo with its own Dockerfile, deployed as a **Kamal accessory** on the same box, called by Rails over private HTTP with a shared secret. (Accessory, not role: Kamal roles all run the single app image — only accessories run a separate image — so "sidecar as a role" is not deployable as written.) Brief downtime on `kamal accessory reboot` is acceptable for an internal service that Rails fronts with retries; if a product ever needs zero-downtime sidecar deploys, the documented alternative is a second Kamal deploy config (`kamal deploy -c config/deploy.sidecar.yml`) in the same repo. Rails stays the system of record and job orchestrator. TypeScript over Python because TS is already in the stack and the Anthropic/Vercel tooling there is first-class.

**Decision tree:** AI feature? RubyLLM in-process. Multi-step or multi-agent? Still RubyLLM — loop verbs and workflow patterns, in-process. Needs an SDK Ruby doesn't have? Add the sidecar accessory. Everything traces to the same observability pipeline either way.

---

## 6. Observability (Fully Self-Hosted, Fully Swappable)

**The contract is with OpenTelemetry, never with a vendor.** Apps emit standard OTel signals; one Collector on the ops box routes them. Swapping any backend, including for a client takeover, is a Collector config change or one env var. Zero application code changes.

### Ops box (one VPS, serves every MVP)

| Concern | Self-hosted backend | Client swap target |
|---|---|---|
| Traces | Tempo | Datadog, Honeycomb, any OTLP backend |
| Logs | Loki | Datadog, CloudWatch |
| Metrics/dashboards | Grafana + Prometheus | Grafana Cloud, Datadog |
| Errors | GlitchTip (Sentry protocol) | Paid Sentry: one-line DSN change, same SDK. Client-mandated non-Sentry vendors: conditional wiring, below |
| LLM traces | Langfuse OSS (self-hosted, MIT core, OTLP ingestion) | Langfuse Cloud, Braintrust, Datadog LLM: endpoint + auth header change |

**Error-vendor flexibility (same pattern as email providers):** the default is the Sentry protocol — GlitchTip self-hosted, paid Sentry a DSN change, zero code. When a client mandates a vendor that doesn't speak the Sentry protocol (Honeybadger, AppSignal, Rollbar, Bugsnag), the template wires it the way it wires email providers: a provider flag at `new-product` time pulls in that vendor's gem plus its initializer template, configured from credentials. Conditional wiring per product, not an abstraction layer — Principle 2 still holds: no custom adapter code, and OTel traces, logs, and metrics are unaffected by the error-vendor choice.

### Ops box operating policy

One box serving telemetry for every product is a cross-product blast radius unless bounded. The template and ops runbook encode:

- **Telemetry is best-effort by design.** Products must serve traffic with the ops box down; exporters buffer and drop, never block the app. Losing telemetry is an incident, not an outage — and the handoff package says so.
- **Per-product labels on every signal** (`service.name` plus a product resource attribute), so cost, retention, and emergency drops can be scoped to one product.
- **Retention caps and disk quotas per backend** (Loki, Tempo, Prometheus, Langfuse), sized so one verbose product cannot evict the fleet's telemetry. Disk and ingest-rate alerts fire before the box fills.
- **An emergency drop rule:** a documented Collector config change that drops a single product's spans/logs without touching the others.
- **Backups for what is actually stateful:** Grafana dashboards, GlitchTip and Langfuse Postgres. Time-series data is declared expendable, in writing, so nobody discovers that policy during a restore.
- **Ingest is authenticated and encrypted.** The Collector's OTLP port is never open-to-the-internet plaintext: TLS with a per-product bearer token, or a private network (WireGuard/Tailscale) between product boxes and the ops box. The 80/443/SSH firewall rule in Section 7 stays true because telemetry rides the private network.
- **Telemetry tenancy mirrors the shared-box promotion triggers (Section 7).** The shared ops box serves MVPs and internal products, with per-product logical separation (Langfuse projects, Grafana folders). A product crossing into client production gets its telemetry off the shared box entirely: a dedicated stack on client-paid infrastructure, or the client's own vendor — priced on the tier-2 cost sheet. The handoff package states the actual tenancy; "nobody else's data shares the box" is only writable for the dedicated tier, and the `handoff-package` skill refuses to write it otherwise.
- **The box is sized for what it actually runs.** Langfuse v3 alone brings ClickHouse, Redis, and S3-compatible storage alongside its Postgres; GlitchTip adds its own Postgres, Redis, and worker; plus Grafana, Loki, Tempo, Prometheus, and the Collector — roughly fifteen containers of stateful infrastructure. Minimum 16GB RAM with per-backend disk budgets, or the OOM killer eats telemetry components intermittently and the gaps surface mid-incident. (Redis inside the ops box's own services doesn't violate the no-Redis rule, which is about product infrastructure.) The real footprint and maintenance load get measured during product #1; if standing up Langfuse threatens the build, GenAI spans already land in Tempo, and Langfuse follows when the first product needs prompt inspection.

### Fleet health (knowing a product is down)

Telemetry is best-effort by design, which makes uptime detection a separate concern: nothing above tells the agency a product died, and at 10+ products on commodity VPSes, silent worker crash-loops and dead boxes are a when, not an if. Kamal's `--restart unless-stopped` handles clean crashes; it does not catch crash loops, hung workers, full disks, or dead boxes. Encoded once in the template and ops runbook:

- **Per-product HTTP probes** from the existing ops box (Prometheus blackbox exporter or an Uptime Kuma container; Grafana alerting is already in the stack).
- **A dead-man heartbeat:** the template ships a recurring Solid Queue job that pings a per-product heartbeat check — one missed heartbeat catches dead workers, dead schedulers, and DB connectivity in a single signal. A green deploy proves the worker exists; the heartbeat proves it stays alive.
- **Alert routing defined once** (email/Slack/webhook), with the handoff package stating the monitoring posture and who gets paged for client-run products.
- **One external check on the ops box itself** — the monitor cannot monitor itself; a free external pinger or a cron on any product box covers it.
- **Backups are monitored like uptime.** Every product emits a backup-freshness heartbeat (last success time and dump size) that alerts when older than the stated RPO — HTTP probes and worker heartbeats stay green while a backup job dies silently after a credential rotation or a full disk. A scheduled restore test into a disposable Postgres instance proves the dump is actually usable; a backup that has never been restored is a hope, not a backup. RPO/RTO and the last restore-test timestamp ship in the handoff package. The ops box's own stateful backups (Grafana dashboards, GlitchTip and Langfuse Postgres) emit the same freshness heartbeat and join the scheduled restore test — the alerting layer's state is not exempt from the rule it enforces.

### Application instrumentation

- **Rails:** standard OTel Ruby SDK + `opentelemetry-instrumentation-ruby_llm` (thoughtbot). Auto-instruments every RubyLLM chat completion, tool call, and token count following the OTel GenAI semantic conventions. Three gems and one initializer. Pin the version; the gem is young (March 2026) and under active development, so verify coverage of streaming and embeddings against what the product actually uses. Fall back to a thin manual wrapper only for uncovered operations.
- **TS sidecar:** OpenLLMetry. Emits the same `gen_ai.*` attributes to the same Collector.
- **Exporter target:** always the Collector on the ops box, never a backend directly. The Collector fans out (Tempo + Langfuse for LLM spans). Note: Langfuse's OTLP endpoint is HTTP only (`http/protobuf`), no gRPC.
- **Logs:** Rails logs to stdout; Docker's log driver captures it; a host-level shipper (Grafana Alloy or Promtail, installed by the same script that provisions each box) ships to Loki. This is deliberate: OTel Ruby's logs signal is not production-mature, and Kamal's logging model is Docker log-driver based. Swapping the log backend is a per-box shipper config change, not an app env var.
- **Metrics:** derived at the Collector (hostmetrics on each box, spanmetrics from traces) rather than an experimental in-app metrics SDK. Dashboards read from Prometheus; swapping means repointing the Collector.

### Two settings baked into the template

1. **LLM content capture flag.** The RubyLLM instrumentation does not capture message content by default. Template exposes an env flag: on for internal products and dev, off by default for client production. Documented in the handoff package; the off-by-default posture is a selling point for regulated clients, and the on posture provides a full audit trail (what was asked, which tools ran, what came back) where that's the requirement. **Known gap, verified against the gem source:** the gem's `capture_content` flag gates chat message content only — `execute_tool` spans emit `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result` unconditionally, and tool payloads are exactly where sensitive data lands (`lookup_customer(email:)`). The template ships a wrapper that puts tool arguments and results behind the same flag, with a regression test asserting that `capture_content=false` emits no prompt, argument, or result payloads. The flag is the gem's own standard env var (`OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT`) — never a parallel homegrown flag — and the wrapper delegates to the gem's `capture_content?` logic, because the env var overrides the installer config and a homegrown flag would split-brain chat capture from tool capture. The regression test covers both the env var and the config option, in both states. Until that test passes, the "content capture off" claim does not appear in any handoff package or sales conversation.
2. **User/session/account propagation.** A small span processor stamps `user.id`, `session.id`, and `account.id` on every span, fed from CurrentAttributes that jobs populate from their arguments — not Rack middleware, which never sees the spans emitted inside Solid Queue workers, and those are exactly the LLM spans per-user and per-account aggregation needs. One template test asserts a job-emitted span carries `user.id` and `account.id`. Painful to retrofit across a dozen products; free to include from day one.

### Cost visibility

Langfuse gives per-product token cost, latency, and prompt inspection; the `account.id` span attribute (above) breaks cost down per account within a product — the number a per-seat or usage-priced product bills from. Monthly AI cost per product feeds both internal margin tracking and the client handoff cost sheet.

---

## 7. Deployment Tiers

The Docker image is the shared artifact across all tiers. Build once, deploy anywhere.

**Tier 1: MVP and internal products.** Kamal to a VPS (Hetzner, DigitalOcean, or any provider offering plain Linux servers; the framework is provider-agnostic). One box per product, or a shared box for pre-revenue experiments. This tier already exists; the template ships with the Kamal config pre-wired.

**Deploy topology (explicit, because a web-only deploy passes a web smoke test while jobs silently never run):** the template's Kamal config ships two roles on every product — `web`, and `worker` running the same app image with `cmd: bin/jobs` and the proxy disabled — with the queue's own pool settings and a drain-friendly stop timeout. Template CI includes a deployed smoke test that enqueues a job and verifies it executed; a green deploy proves the worker exists, not just the homepage.

**Secret custody (fleet-wide policy):** one vault project per product. `.kamal/secrets` contains resolver code only (Kamal's password-manager/secret-manager adapters), never raw credentials. The vault is the single durable, human-managed source; no raw value lives in shell history, CI variables, dotfiles, or ad-hoc `.env` files. Kamal itself materializes resolved secrets onto each host at deploy time — a `0600` env file under `.kamal/apps/<app>/env/`, passed to the container via `--env-file` and visible through `docker inspect` — and that operational artifact is documented, listed in the handoff secret inventory, and covered by rotation (rotating a credential invalidates the on-disk copies). The handoff package includes the secret inventory with owners and rotation dates, and client takeover ends with a mandatory rotation checklist — every credential the agency ever held gets rotated, so "who still has our keys?" has a provable answer: nobody.

**Shared-box promotion triggers (hard rules, agent-checkable):** a product on a shared box moves to its own box (or Tier 2) the moment any of these becomes true — real client data, paying users, an external client demo, production API keys for third-party services, or scheduled jobs that another tenant's load could starve. "Pre-revenue experiment" is a status checked against these triggers, not a judgment call. Every box, shared or dedicated, gets the same firewall defaults (only 80/443/SSH exposed) and a nightly off-box Postgres backup with a documented restore path. Provisioning also enables unattended OS security upgrades and key-only SSH (no root password auth), and the ops runbook schedules a quarterly Docker/kamal-proxy/OS upgrade pass across the fleet, reboots included — ten internet-facing boxes running month-1 kernels for a year is how a fleet gets owned. File storage lives in the product's S3-compatible bucket, never on the box — losing a box loses no user data, and a Postgres backup that looks healthy is not mistaken for a full backup. Per-product buckets enable object versioning (or the provider's soft-delete equivalent) with lifecycle expiry on old versions — an agent-written cleanup job with the wrong scope, or a leaked credential, must not be able to permanently destroy user files — and the handoff bucket inventory states the setting. Postgres itself runs as a Kamal accessory on the product box with a named volume; the nightly off-box backup is its restore path.

**Tier 2: Client production.** Executed only when a client pays for the migration. Three documented paths, in order of preference:

1. **EC2 + Kamal.** If the requirement is loosely "must be on AWS," EC2 is a server, Kamal works unchanged, and the deploy mechanism stays uniform.
2. **Cloud Run.** `gcloud run deploy` against the image in Artifact Registry. Managed HTTPS, the simplest graduation path that exists — with one non-negotiable caveat: every product here runs Solid Queue and scheduled jobs, and scale-to-zero plus request-based CPU throttling silently kills both (at zero instances schedules never fire; throttled CPU stalls an in-flight LLM job the moment the HTTP response completes). The runbook therefore deploys the web service (which may scale to zero) plus an always-on Solid Queue worker as a Cloud Run **worker pool** (`gcloud run worker-pools deploy`; a plain Cloud Run service would fail its startup probe because `bin/jobs` listens on no port — if worker pools are unavailable in the client's region or org policy, the fallback is a trivial health-check listener in the worker entrypoint), instance-based billing, min instances 1 — a real line item on the monthly cost sheet, priced in the handoff package. Chat over WebSockets gets a session-affinity review as part of the same migration.
3. **ECS Fargate.** ECR + Copilot or Terraform. More moving parts (task definitions, ALB, service config); justified when the client is an AWS shop or has compliance requirements. **The Terraform for this path is written when the first client pays for it, not before.**

**The database moves with the migration, and the runbook says so:** EC2 + Kamal keeps Postgres as a Kamal accessory (or RDS where the client requires managed); Cloud Run pairs with Cloud SQL; Fargate pairs with RDS. Each path's runbook includes the dump–restore–cutover procedure and puts the managed-database line on the cost sheet — the riskiest step of a paid migration is not left as an exercise.

Kamal is SSH-based and does not deploy to Fargate or Cloud Run; those use their native mechanisms. The `deploy-runbook` skill encodes all three paths.

---

## 8. The Agent Layer

The framework ships as three things: a versioned **template repo**, a **CLAUDE.md** conventions file, and **six skills**. This is the whole product.

The template is a runnable Rails application — it boots, passes `bin/ci`, and deploys to a canary box — cloned **at a tagged release** by the `new-product` skill and never regenerated per product: regeneration would make every product a snowflake, re-roll every hard-won fix, and turn the drift policy's backport lookup into a per-CVE investigation. Per-product differences are config and data (name, tokens, credentials, deploy targets, provider flags), never structure. How the template is built, versioned, and kept current by agents and tooling is specified in `template-build-spec.md`.

### Template repo contents (pre-wired)

**Application core:** Rails 8 in API mode with the named middleware re-enabled (Section 2), Rails 8 auth with cookie sessions (httpOnly, secure, SameSite, CSRF), Rails-native `rate_limit` on the sign-in and sign-up endpoints, TOTP two-factor with backup codes, sudo mode on sensitive endpoints, OmniAuth social login with the encrypted `ConnectedAccount` token store (providers per product config), the accounts-first identity model (`Account` / `AccountUser` with jsonb roles / `AccountInvitation`, `Current.account` plumbing, the `AccountRecord` scoping base class via acts_as_tenant, SPA account switcher + members + invitation screens over the generated client, and the cross-tenant isolation regression test), Pundit policies wired to `Current.account_user`, the sudo-gated account ownership transfer flow, branded error handling routed through `exceptions_app` (JSON error responses + SPA-rendered error states, no default Rails error pages), seeds.

**API & contract:** the Vite React + TypeScript SPA served same-origin from the Rails image, the spec-first OpenAPI contract (committee/skooma conformance tests in Minitest) with the codegen pipeline (openapi-typescript/orval) and the hard-failing CI drift gate, TanStack Query wired over the generated client, Tailwind + shadcn + tokens file, the `/api/v1` surface (default-deny CORS, rate limiting, idempotency convention, prefixed public IDs, OpenAPI generation; token auth arrives with the first non-browser consumer), the inbound-webhook pipeline with its provider generator, and the `ApplicationClient` base + generator for outbound integrations (Section 2).

**Product plumbing:** Noticed notifications (account-scoped, Cable-nudged, SPA bell over generated hooks), announcements/changelog (Madmin-managed, "What's new" panel with unread badge), referral tracking with the product-defined conversion hook, i18n plumbing (locale resolution chain, SPA message catalog, `raise_on_missing_translations` in dev).

**Solid stack:** Solid Queue, Solid Cache, and Solid Cable on the Rails 8 multi-database layout (dedicated `queue` / `cache` / `cable` databases, own migration paths); `bin/jobs` runs the worker; Mission Control Jobs mounted at `/admin/jobs` (its own HTTP basic auth disabled — `/admin` is already gated).

**Admin:** Madmin at `/admin`, gated by the user admin flag at both the route and controller layer, with CRUD resources for users, accounts, memberships, invitations, announcements, and Active Storage blobs, and a dashboard root. User impersonation for support, with the true-admin vs impersonated-user distinction preserved. Admin actions run `ActsAsTenant.without_tenant` so support can see across accounts — the one sanctioned hole in tenant scoping, confined to `/admin` and the admin flag.

**Email:** Mailbin delivery + `/mailbin` engine in development, the per-client production provider switch, per-environment mailer URL hosts (Section 2).

**AI & observability:** `app/ai/` conventions, OTel instrumentation (the RubyLLM gem, the user/session/account span processor, the content-capture wrapper and its regression test), Sentry-protocol SDK with DSN via env (GlitchTip default), conditional wiring for client-mandated error vendors (Section 6).

**Storage & deploy:** Active Storage wired to the per-product S3-compatible bucket (a deployed smoke check uploads and reads back a blob), multi-stage Dockerfile (Vite build stage copies output into Rails public assets; jemalloc; non-root runtime; Thruster in front of Puma; entrypoint runs `db:prepare` on server boot), Kamal config (web + worker roles, secrets via vault resolvers), and a first-class **staging** environment (its own `config/environments/staging.rb` and databases) for client-facing preview deploys.

**CI:** `bin/ci` on the Rails CI DSL — RuboCop (rails-omakase), bundler-audit, Brakeman, `npm audit`, unit + system tests, seed replant — mirrored in GitHub Actions with Dependabot, plus the deployed job-execution, chat streaming, and blob smoke checks.

**Developer experience:** `bin/setup` (idempotent: Brewfile → gems → `db:prepare` → exec `bin/dev`, with `--reset` and `--skip-server` flags), `bin/dev` (auto-installs overmind, runs `Procfile.dev`: web + Vite dev server + jobs), a devcontainer (compose: app + Postgres 17 + Selenium for system tests — no Redis; Rails devcontainer features; `postCreateCommand: bin/setup`), and a Brewfile for native deps.

### CLAUDE.md (hard rules for the agent)

- Tokens and existing components only; no inline style values
- All browser data fetching goes through the generated API client and TanStack Query hooks; raw `fetch` calls and hand-written request functions are forbidden and linted
- Every API change starts in the OpenAPI spec; the conformance request test and the regenerated client land in the same commit. CI fails on drift — do not bypass the gate
- Browser auth is cookie sessions. Never introduce JWT or token auth for the browser; token schemes are added only for non-browser consumers, alongside sessions
- API serialization goes through named serializer objects, never ad hoc hashes in controllers
- Every tenant-owned table carries `account_id` and its model inherits `AccountRecord`, never `ApplicationRecord`; tenant data is never queried outside the current account scope
- Roles live on the membership (`AccountUser.roles` jsonb), never on `User`. Authorization goes through Pundit policies with `Current.account_user` as `pundit_user` — never bare user identity, never ad hoc role checks scattered through controllers
- Credential-changing or destructive endpoints (password/email change, 2FA disable, token management, ownership transfer, deletion) are sudo-gated — new sensitive endpoints get the sudo guard, not a judgment call
- Outbound HTTP to third-party services goes through an `ApplicationClient` subclass in `app/clients/`, never inline HTTP in jobs, models, or controllers
- User-facing strings go through I18n (Rails side) or the SPA message catalog — never hardcoded
- Jobs touching tenant data take `account_id` as an argument and set the tenant context before the first query
- Prompts are versioned objects in `app/ai/`, never inline strings
- Postgres for queue/cache/cable; no new infrastructure without explicit approval
- File uploads go through Active Storage against the product's S3-compatible bucket; local disk only in dev/test
- All LLM calls go through RubyLLM (or the sidecar contract); never a raw provider SDK
- No `Timeout.timeout` in jobs; LLM calls are bounded by RubyLLM's `request_timeout`, set explicitly
- Inside `Async` fan-out blocks: LLM/HTTP I/O only, no ActiveRecord — collect results, persist after fan-in; DB-touching tools never get `concurrency: :fibers`
- All outbound email goes through Action Mailer; the delivery provider is config + credentials, never a provider SDK call in application code
- Every external service is configured by env var against a standard protocol

### Skills

| Skill | Does |
|---|---|
| `spec-interview` | 30-minute interview producing the two-page spec (Section 3) |
| `new-product` | Clone template, rename, apply tokens, configure env, wire the OpenAPI codegen pipeline, first deploy to the tier 1 VPS |
| `design-tokens` | Generate a client theme from brand inputs; render the style-check page |
| `add-ai-feature` | Scaffold agent job or chat feature per Section 5 conventions; wire tracing |
| `deploy-runbook` | Execute the correct tier/path from Section 7 |
| `handoff-package` | Assemble the client takeover artifact (below) |

### The handoff package

With mixed inheritance across clients, a standardized takeover artifact is both risk reduction and a sales asset. Contents: architecture overview, env var inventory, storage bucket inventory, the generated OpenAPI spec (it doubles as API documentation for the inheriting team), runbook, monthly cost sheet (infrastructure + AI token costs from Langfuse), observability swap instructions (env vars for traces and errors; shipper/Collector repoints for logs and metrics), telemetry data residency (where telemetry lives and whose data shares the box), the secret inventory with owners, rotation dates, and the takeover rotation checklist, backup posture (RPO/RTO and the last restore-test timestamp), LLM content-capture posture, and the deploy path executed. Most agencies never produce this. Every product here gets one generated by a skill.

---

## 9. Explicitly Not Building

Named so nobody builds them by accident:

- Custom logging/error/observability adapter layers (standards already solve this)
- A bespoke design system (tokens + shadcn is the system)
- A second backend stack, or Python anywhere in the product path
- Terraform for clouds no client has paid to deploy into
- JWT or bearer-token auth for the browser (cookie sessions solve this; tokens arrive with the first paying non-browser consumer)
- A separately deployed frontend (separate CDN, separate repo, separate deploy) until a client contract explicitly pays for that split
- Redis, until a specific product proves Postgres-backed queue/cache insufficient
- Template-parity backports beyond security fixes
- Per-tenant infrastructure (a database or schema per tenant), billing frameworks, or any other "products will eventually need it" infrastructure ahead of a product that needs it — the accounts-first tenancy *data model* is in the template (Section 2) precisely because schema cannot be retrofitted; infrastructure can
- Payment processors (Stripe, Paddle, Braintree, etc.) — wired per product when it actually charges money, never preemptively in the template

---

## 10. Build Plan

The template is built in workstreams, in dependency order, and only to the point where product #1 can ship through it (Principle 4):

**Core:** Extract the template repo from the best existing Kamal/VPS product. Implement the accounts-first identity model with its cross-tenant isolation test (Section 2). Wire the Vite React SPA into the Rails image + tokens + shadcn. Set up the OpenAPI contract: the spec, the Minitest conformance tests, the OpenAPI→TypeScript codegen pipeline, and the hard-failing CI drift gate. Wire the rest of the `/api/v1` surface. Wire Pundit, prefixed IDs, and the auth-hardening endpoints (2FA, sudo mode, social login).

**Product infrastructure:** Wire Active Storage to the per-product bucket plus the deployed blob smoke check. Wire Mailbin and the production email provider switch. Stand up Madmin with Mission Control Jobs behind the admin flag. Add the product plumbing (notifications, announcements, referrals, i18n) and the `ApplicationClient` generator. Add the streaming batcher and the deployed chat load smoke test (Section 5).

**Observability and fleet:** Add OTel + RubyLLM instrumentation + GlitchTip SDK + the span processor, plus the content-capture wrapper and its regression test (Section 6). Stand up Langfuse OSS on the ops box and point the Collector at it. Stand up fleet health checks (probes, worker and backup-freshness heartbeats, the restore test, alert routing — Section 6).

**Agent layer:** Write CLAUDE.md. Extract `bin/dev`, `bin/setup`, `bin/ci`, and the devcontainer. Build `spec-interview`, `new-product`, and `design-tokens` skills. Write the deploy runbook (Kamal path fully; Cloud Run and ECS as documented procedures). Draft the handoff package template.

**Validation:** Ship product #1 through the full pipeline.

**If the build drags, the relief valves are named:** defer Langfuse (GenAI spans already land in Tempo), defer the external-consumer extras of `/api/v1` (token auth already arrives with the first non-browser consumer by design), and defer OmniAuth social login and referrals to the first product that needs them (both are additive — no schema the SPA depends on). The contract pipeline itself — spec, conformance tests, codegen, drift gate — cannot be deferred: it is the frontend bridge, and every SPA page depends on it. The account model cannot be deferred at all: it is schema, and schema is the one thing that cannot be retrofitted. The content-capture wrapper and its test cannot be deferred for any product that will make the capture-off claim; the Active Storage wiring and its blob smoke check cannot be deferred for any product that accepts uploads; fleet health cannot be deferred past product #1's first production week.

`add-ai-feature` and `handoff-package` skills are finished during product #1, against real usage, per Principle 1.

**Observed during product #1, not fixed in advance:** do agents keep the OpenAPI spec as the source of truth under pressure, or route around the codegen when it is inconvenient? Watch for endpoints added without spec entries and conformance tests, generated files hand-edited, and drift-gate failures "fixed" by regenerating without reviewing the diff. If routing-around appears, tighten the gate (pre-commit hook, protected generated paths) before product #2. The serializer-object rule in CLAUDE.md exists for the same reason; expect to need lint enforcement for it by product #3. Watch the same way for tenancy: do agents inherit from `AccountRecord`, or reach for `ApplicationRecord` with hand-written `where(account_id:)` scopes — or neither? If they route around the base class, CI grows a lint that flags any `account_id`-bearing table whose model does not inherit `AccountRecord`, hard-failing by product #2.

---

## Appendix: The Vendor Swap, Concretely

Client takes over a product and wants their own vendors. What moves by app env var:

```
# Errors: GlitchTip -> paid Sentry
SENTRY_DSN=<their Sentry DSN>

# Traces + LLM spans: ops box -> their backend
OTEL_EXPORTER_OTLP_ENDPOINT=<their collector or vendor endpoint>
OTEL_EXPORTER_OTLP_HEADERS=<their auth header>
```

Logs and metrics move at the infrastructure layer, not the app: logs by repointing the host log shipper, metrics by repointing the Collector — documented per-box config changes in the handoff package, zero application code either way. The swap story is honest about that split; claiming logs follow an app env var would be discovered as false during the first client takeover and poison trust in the rest of the package.

Model provider swap is a RubyLLM config change. Storage is S3-compatible. Mail is SMTP. Nothing in application code knows a vendor exists.

---

## References

- **`why-not-nextjs.md`** — the full decision record for rejecting full-stack TypeScript/Next.js: the steelman, the per-axis verdict, and the explicit triggers that would rerun the decision.
- **`template-build-spec.md`** — how the template repo is built, versioned (tagged releases, calver), cloned per product, and kept current by agents and tooling; includes the rejected regeneration-per-product analysis and the done bar before product #1.
- **`template-development-spec.md`** — the phase-by-phase build blueprint for `template/`: thirteen dependency-ordered phases (skeleton → auth → accounts → contract pipeline → SPA → auth hardening → email → plumbing → AI → admin → observability → deploy/CI → done bar), each with acceptance gates, plus the pattern reference map.
- The account and membership modeling in Section 2 (`Account` / `AccountUser` / `AccountInvitation`, jsonb roles on the membership, creator-admin and default-account callbacks, ownership transfer, the `AccountRecord` scoping base class) and a set of template conventions — Mailbin development email and the per-client production email provider switch, the Madmin admin with Mission Control Jobs and support impersonation, auth hardening (TOTP two-factor with backup codes, sudo mode, OmniAuth social login with the encrypted connected-accounts store), Pundit authorization wired to the membership, prefixed public IDs, Noticed notifications, announcements, referrals, the `ApplicationClient` outbound-integration convention, the inbound-webhook pipeline, i18n plumbing, conditional error-vendor wiring, the staging environment, and the `bin/dev` / `bin/setup` / `bin/ci` / devcontainer developer tooling — are based on patterns in **Jumpstart Pro Rails**. The reference informed those specific conventions and their constraints; auth is Rails 8 built-in authentication, the frontend is this framework's SPA + generated client, and billing/payment machinery is deliberately not adopted (Section 9).
