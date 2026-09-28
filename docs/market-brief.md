# Model Maverick: Ground-Truth Market Brief

Compiled 2026-09-27 from live web research. Every non-obvious claim has a URL. "Accessed" means fetched on 2026-09-27. Items marked UNVERIFIED could not be confirmed from a primary source. Secondary/vendor-marketing sources are labeled as such.

## Key facts that challenge or support the plan

1. CHALLENGE: The plan's "closest neighbors as of early 2026" list is stale. Martian no longer sells a router (site is interpretability research; spun out Thesean AI "Ship", launched 2026-07-21, which executes inference). Not Diamond narrowed to coding-agent routing on 2026-08-04. Unify.ai is now a "continual learning" research lab.
2. CHALLENGE: OpenRouter is being acquired by Stripe for a reported $7B+ (Stripe announced 2026-08-19). Stripe's stated rationale is routing requests "to the optimal model based on task complexity, price, speed, and reliability." "Generic" auto routing now has Stripe's distribution behind it.
3. CHALLENGE: OpenRouter Auto Router is no longer Not Diamond-powered. It classifies prompts into ~30 task types, ranks models by trailing 7-day community spend, and supports allow/deny model patterns and 5 cost tiers, at no fee. That overlaps with MM's "classify + tenant pool + cost ceiling" feature set.
4. CHALLENGE: Azure Foundry Model Router is cross-provider, not OpenAI-only: it routes across OpenAI, Anthropic (Claude Opus 4.6 to 4.8, Sonnet 4.5, Haiku 4.5), xAI (Grok 4), DeepSeek and Meta, with Balanced/Cost/Quality modes, customer model subsets, and built-in failover (docs updated 2026-09-02).
5. CHALLENGE: Ramp launched "Router" on 2026-08-20, free through end of 2026, with a mode that picks models "based on up to three user-specified benchmarks." That is close to MM's value proposition, bundled with a spend-management product.
6. SUPPORT: "Most of the LLM Routing Gap Is Task Type" (arXiv, 2026-08-24): a static task-type table beat the best single model (262 vs 245 correct) at 43% of the cost ($3.33 vs $7.69), and beat learned routers. This supports a capability-table approach like MM's (small study: 14 models, 294 questions).
7. SUPPORT/CHALLENGE: LLMRouterBench (2026-01-12, 33 models, 400K instances): several routers, including OpenRouter's commercial router (-24.7%), fail to beat the Best Single model. "Routing Plateau" (2026-05-27, 21 methods) shows routers plateau far below oracle and fail on hard queries. Routing value is real but modest and hard to prove; buyers can reasonably ask for proof.
8. CHALLENGE: Gateway consolidation and exits in 2026: Helicone to Mintlify (2026-03-03, product in maintenance mode), Portkey to Palo Alto Networks ($117M consideration, closed 2026-05-29), Katanemo/Arch to DigitalOcean (2026-04-02), Langfuse to ClickHouse (2026-01-16), TensorZero (Rust gateway, $7.3M seed) shut down and archived its repo 2026-06-12.
9. CHALLENGE (ToS): Anthropic Commercial Terms D.4 bar using the Services "to build a competing product or service ... or resell the Services." Anthropic cut off OpenAI's Claude access on 2025-08-01 over benchmarking/development use. MM's trial proxy executes inference; if it uses MM's own provider keys, that is arguably reselling.
10. CHALLENGE (ToS): Google Cloud Service Terms s.7 allow benchmarking but public disclosure requires "all necessary information to replicate the Tests." Publishing results from a private corpus conflicts with that for Vertex/Agent Platform. xAI's AUP (effective 2026-08-14) bars use of the Service to develop "any products or services that compete with SpaceXAI, whether directly or indirectly" and "using bots to access" the Service.
11. SUPPORT: Multi-model use is the norm. a16z (2025-06-10): 37% of enterprises use 5+ models (up from 29%), driven mainly by use-case performance differences. OpenRouter volume grew from 5T to 25T tokens/week in six months (TechCrunch, 2026-05-26).
12. CHALLENGE (buyers): Gartner (Oct 2025) sized the AI gateway TAM at only $50M to $100M for 2025, but forecasts 70% of multimodel teams using gateways by 2028. Security and data privacy is the top adoption barrier (44%, Kong 2025 survey). Competitors sell SOC 2, EU residency, and self-hosting, and a solo operator on one Hetzner box cannot match that soon.
13. CHALLENGE (stack): No provider ships an official Rust SDK (Anthropic lists 7 SDK languages, none Rust; OpenAI and Google also none). All three ship official Go SDKs (anthropic-sdk-go v1.75.0, openai-go v3.66.0, go-genai v1.71.0, all released in Aug/Sep 2026). A Rust engine means hand-written clients for every provider.
14. SUPPORT (stack): rmcp (official MCP Rust SDK) is mature: v3.4.1 on 2026-09-23, ~30M downloads. sqlx 0.9.0 shipped 2026-05-21 (moved to transact-rs org). CHALLENGE: apalis 1.0 is still RC (rc.10) and apalis-postgres has no stable release. River (Go) is at v0.47.0 (2026-08-31) with first-class unique jobs.
15. SUPPORT: Gateway latency overhead is a solved problem in compiled languages. Bifrost (Go) reports ~11 microseconds overhead at 5k RPS versus ~600 microseconds for LiteLLM (vendor benchmark). Go matches Rust on this metric for MM's purposes, so it does not argue for Rust.

---

## 1. LLM routing / model-selection market (2025-2026)

| Player | What it is now | Executes inference? | Per-capability benchmarking? | Pricing | Corporate news |
|---|---|---|---|---|---|
| Not Diamond | Router for coding agents (pivoted/narrowed 2026-08-04) | No. "executed in your model gateway and harness of choice" | Learned meta-router over 60+ models, retrained on new model releases; personalizes from each developer's implicit feedback | PAYG $0.05 per 1M tokens routed; Enterprise custom | Seed ~$2.3M (Defy, 2024); SOC 2 + ISO 27001 claimed |
| Martian | Interpretability research; router product gone | n/a | n/a | n/a | $9M seed (Nov 2023); Accenture investment (Sep 2024); incubated Thesean AI |
| Thesean AI (Martian spinout) | "Ship": per-request best execution (model, cascade, ensemble, harness) | Yes | Not disclosed | "50% less expensive" than reference model, quality SLA | Reported $30M (UNVERIFIED primary) |
| OpenRouter Auto Router | Classifies prompt to ~30 task types, ranks by community spend | Yes (OpenRouter is the gateway) | No. Spend-share based, not benchmark based | No fee on top of the selected model | Stripe acquisition, $7B+ reported |
| Azure Foundry Model Router | Trained router LLM, deployed as one model | Yes | Trained per-prompt quality prediction; not per-customer | Charged on input tokens at a router rate (secondary source: $0.14/1M input) | Versions 2025-05-19, 2025-08-07, 2025-11-18 (active) |
| AWS Bedrock Intelligent Prompt Routing | Routes between exactly 2 models in the same family | Yes | Predicts response quality per request | $1.00 per 1,000 requests (secondary source) | English-only; "can't adjust routing ... based on application-specific performance data" |
| Google Vertex Model Optimizer | Meta-endpoint across Gemini models (cost/quality preference) | Yes | Gemini-only | UNVERIFIED | Vertex AI rebranded as Gemini Enterprise Agent Platform (announced 2026-04-22); old doc URL now redirects to a models overview, current status UNVERIFIED |
| Ramp Router | API router over OpenAI, Anthropic, DeepSeek, Moonshot, MiniMax, Nvidia, xAI, Z.ai | Yes | Mode that picks by up to 3 user-specified benchmarks | Free through 2026; inference paid | Launched 2026-08-20, US only |
| RouteLLM (LMSYS) | OSS research framework | Self-host | Binary strong/weak router | Free | Repo last pushed 2024-08-10 (inactive) |
| Unify.ai | Now "a research lab working on continual learning" | n/a | n/a | n/a | Router product not on site as of 2026-09-27 |
| Requesty | Managed gateway, 400-600+ models, EU residency | Yes | Cost/latency/availability routing | 5% markup | $3M raised 2024 (secondary) |
| Katanemo Arch-Router / Plano | OSS data plane (Rust) + 1.5B routing model | Self-host proxy | Preference/policy-based | OSS | Acquired by DigitalOcean 2026-04-02; CEO became DO SVP of AI |
| Portkey | Gateway: conditional routing, fallbacks, guardrails | Yes (proxy) | No | Free tier; paid from $49/mo | Acquired by Palo Alto Networks: announced 2026-04-30, closed 2026-05-29, $117M |
| LiteLLM | OSS Python SDK + proxy | Self-host proxy | No (rules, fallbacks) | OSS; Enterprise from ~$250/mo to ~$30K/yr (secondary) | v1.102.1 (2026-09-23), 59.7K stars |
| Vercel AI Gateway | Managed gateway, fallbacks, rewrite/deny routing rules | Yes | No | Zero token markup; $5/mo credits; add-ons billed | Routing rules launched 2026 |
| Cloudflare AI Gateway | Gateway with Dynamic Routing (splits, geo, budget fallbacks) | Proxy | No | Core free; 5% fee on Unified Billing credits | Cloudflare agreed to acquire Replicate (2025-11-17) |
| Braintrust | Evals platform + gateway (beta) | Yes | Quality-based selection from experiments/traces | Pro $249/mo | |
| TrustedRouter | Unified API over 600+ models | Yes | UNVERIFIED | UNVERIFIED | $1.25M seed (Sep 2026) |

Sources:
- Not Diamond home and pricing: https://www.notdiamond.ai/ , https://www.notdiamond.ai/pricing (accessed 2026-09-27). Not Diamond Code launch, 2026-08-04: https://www.notdiamond.ai/blog/not-diamond-code-intelligent-model-routing-for-coding-agents . Claims: "20%+" savings, 39% on Poly-SWE-Bench, 61% on LongCodeQA; runs as a "privacy-preserving local proxy." Funding: https://tracxn.com/d/companies/notdiamond/__-ifpQNdltj98K5h7wki2GzegvCScnOt_vhbDrwaZaA0
- OpenRouter's Auto Router was "powered by Not Diamond" at its January 2025 relaunch (19 models): https://x.com/OpenRouterAI/status/1876465116293808179 . Current docs describe community-spend ranking: https://openrouter.ai/docs/guides/routing/routers/auto-router (accessed 2026-09-27). Quote: "The router looks up which models the OpenRouter community actually spends on over a trailing 7-day window." Pricing: "There is no additional fee for using the Auto Router." Options: allowed/excluded model wildcards, cost tier `low|medium|high|xhigh|max`. An `openrouter/auto-beta` variant was listed 2026-07-17 (secondary: https://dev.to/kamya_shah_e69d5dd78f831c/top-7-auto-routing-platforms-for-model-selection-in-2026-26cn).
- OpenRouter also launched "Fusion" (multi-model then synthesize), July 2026. Secondary: https://pakodas.substack.com/p/llm-routers
- Martian current site: https://withmartian.com/ (accessed 2026-09-27, no router product). Seed: https://www.hpcwire.com/bigdatawire/this-just-in/martian-raises-9m-for-advanced-model-mapping-to-enhance-llm-performance-and-accuracy/ . Accenture: https://newsroom.accenture.com/news/2024/accenture-invests-in-martian-to-bring-dynamic-routing-of-large-language-queries-and-more-effective-ai-systems-to-clients . A Medium claim that Martian was "nearing a $1.3B valuation" (April 2026) is UNVERIFIED and likely conflated with OpenRouter.
- Thesean Ship, 2026-07-21: https://www.thesean.ai/blog/introducing-ship . The $30M figure comes from search snippets of https://www.thesean.ai/ (UNVERIFIED).
- Azure Model Router concepts, updated 2026-09-02: https://learn.microsoft.com/en-us/azure/foundry/openai/concepts/model-router . Quote: Balanced mode "considers all underlying models within a small quality range (for example, 1% to 2% ...) and picks the most cost-effective model"; Cost mode uses a "5% to 6%" band. Routing decisions use "text input only." Pool includes gpt-5.6-sol/terra/luna (2026-07-09), gpt-5.5, gpt-5.4 family, claude-opus-4-8/4-7/4-6, claude-sonnet-4-5, claude-haiku-4-5, grok-4, grok-4-1-fast-reasoning, DeepSeek-V3.2, Llama-4-Maverick. Claude models must be deployed separately. Pricing figure is secondary: https://futureagi.com/llm-cost-calculator/azure-ai-foundry/model-router/ ($0.14/1M input, verified 2026-08-06 per that site). Savings of 4.5% Balanced / 4.7% Cost / 14.2% Quality are from a search snippet of https://techcommunity.microsoft.com/blog/azuredevcommunityblog/optimising-ai-costs-with-microsoft-foundry-model-router/4494776 (UNVERIFIED, page is JS-rendered).
- AWS Bedrock IPR docs: https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-routing.html (accessed 2026-09-27). "You must choose exactly two models within the same family." Supported list still shows Claude 3.x, Nova, Llama 3.x. AWS claims "up to 30%" savings: https://aws.amazon.com/bedrock/intelligent-prompt-routing . The $1/1K requests price is secondary: https://cloudburn.io/blog/amazon-bedrock-pricing
- Vertex Model Optimizer: https://cloud.google.com/blog/products/ai-machine-learning/gemini-2-5-pro-flash-on-vertex-ai (2025 launch). Vertex rebrand: https://dev.to/om_shree_0709/google-just-killed-vertex-ai-heres-what-the-gemini-enterprise-agent-platform-4fh4 (secondary).
- Ramp Router, 2026-08-20: https://techcrunch.com/2026/08/20/ramp-launches-its-own-ai-model-router-called-router/
- RouteLLM repo: https://github.com/lm-sys/RouteLLM (GitHub API: 5.5K stars, last push 2024-08-10).
- Unify: https://unify.ai/ (accessed 2026-09-27).
- Requesty: https://www.requesty.ai/ (5% markup, EU residency; no SOC 2 claim on homepage). A secondary source calls it "built in Rust" (UNVERIFIED): https://www.requesty.ai/blog/best-llm-routing-platforms-compared-2026-requesty-portkey-litellm-openrouter
- DigitalOcean/Katanemo, 2026-04-02: https://www.businesswire.com/news/home/20260402272982/en/DigitalOcean-Acquires-Katanemo-Labs-to-Accelerate-the-Inference-Cloud-for-the-Agentic-Era . Plano repo: https://github.com/katanemo/plano (Rust, 7K stars, v0.4.36 on 2026-08-19).
- Portkey/Palo Alto: https://www.paloaltonetworks.com/company/press/2026/palo-alto-networks-completes-acquisition-of-portkey-to-secure-ai-agents ; the $117M consideration is from the 10-K, https://www.sec.gov/Archives/edgar/data/0001327567/000132756726000023/panw-20260731.htm (via search summary). Portkey OSS gateway repo last pushed 2026-05-25.
- LiteLLM: https://www.litellm.ai/pricing ; secondary pricing: https://www.truefoundry.com/blog/litellm-pricing-guide
- Vercel: https://vercel.com/docs/ai-gateway/pricing ; routing rules: https://vercel.com/changelog/ai-gateway-routing-rules
- Cloudflare: https://developers.cloudflare.com/ai-gateway/reference/pricing/ ; Replicate: https://www.businesswire.com/news/home/20251117400765/en/Cloudflare-to-Acquire-Replicate-to-Build-the-Most-Seamless-AI-Cloud-for-developers
- Braintrust: https://www.braintrust.dev/articles/best-llm-routers-2026 (vendor article).
- TrustedRouter: https://www.finsmes.com/2026/09/trustedrouter-raises-1-25m-in-seed-funding.html

### OpenRouter / Stripe details
- Series B: $113M led by CapitalG at ~$1.3B post-money. Previous valuation ~$547M; $40M Series A (June 2025, a16z + Menlo). 8M users; 5T to 25T tokens/week over six months. TechCrunch 2026-05-26: https://techcrunch.com/2026/05/26/openrouter-more-than-doubles-valuation-to-1-3b-in-a-year/
- Stripe announcement 2026-08-19: https://stripe.com/newsroom/news/stripe-agrees-to-acquire-openrouter . Collison: "together with OpenRouter we'll help businesses maximize profitability by routing their requests intelligently and spending their tokens efficiently." Price not disclosed by Stripe. Bloomberg reported "more than $7 billion" (2026-08-16): https://www.bloomberg.com/news/articles/2026-08-16/stripe-nears-deal-to-buy-ai-firm-openrouter-for-over-7-billion . OpenRouter said close in "a few weeks": https://openrouter.ai/blog/announcements/openrouter-is-joining-stripe/ . Closing as of 2026-09-27 is UNVERIFIED.
- Menlo Ventures (OpenRouter investor), 2026-08-19: "OpenRouter's primary product is not routing tokens; it's being the best AI gateway." 10M users, 500+ models, 80+ providers, 4.5+ quadrillion tokens/yr run rate. https://menlovc.com/perspective/stripe-to-acquire-openrouter-why-everyone-is-obsessed-with-model-routing/
- Outages 2026-02-17 (38 min) and 2026-02-19 (35 min), secondary: https://www.bestaiweb.ai/openrouter-martian-and-not-diamond-the-2026-llm-router-race-and-where-agent-cost-optimization-is-heading/

### Entrants and exits, 2025-2026 (chronological)
- 2025-08: TensorZero raises $7.3M seed (Rust LLMOps gateway).
- 2025-11-17: Cloudflare agrees to acquire Replicate.
- 2026-01-16: ClickHouse acquires Langfuse alongside a $400M Series D. https://clickhouse.com/blog/clickhouse-raises-400-million-series-d-acquires-langfuse-launches-postgres
- 2026-03-03: Helicone joins Mintlify. Standalone product in maintenance mode. https://www.helicone.ai/blog/joining-mintlify . Helicone's Rust `ai-gateway` repo was last pushed 2025-11-21.
- 2026-04-02: DigitalOcean acquires Katanemo (Arch-Router, Plano).
- 2026-04-30 / 05-29: Palo Alto Networks announces, then closes, the Portkey acquisition.
- 2026-05-26: OpenRouter Series B at $1.3B.
- 2026-06-12: TensorZero shuts down, archives repo, returns capital. CEO Gabriel Bianconi: "An open-source company has to find product market fit twice: first for the OSS project and again for a commercial product. The AI market moves very quickly so it's easy to take a step in the wrong direction and fall behind." https://news.ycombinator.com/item?id=48518120 ; https://byteiota.com/tensorzero-shuts-down-what-oss-llmops-cant-survive/
- 2026-07-21: Thesean "Ship" beta.
- 2026-07-21: vLLM Semantic Router "Mixture of Models" announcement (secondary: https://pakodas.substack.com/p/llm-routers).
- 2026-08-04: Not Diamond narrows to coding agents.
- 2026-08-19: Stripe agrees to acquire OpenRouter.
- 2026-08-20: Ramp Router launches.
- 2026-09: TrustedRouter $1.25M seed.

## 2. Provider-side routing

- OpenAI GPT-5 (Aug 2025): in ChatGPT, a "real-time router" picks between fast and thinking models. In the API, developers get the models directly and set `reasoning_effort` (minimal/low/medium/high) and verbosity. There is no API "auto" model. The router broke on launch day. Altman, 2025-08-08: "the autoswitcher broke and was out of commission for a chunk of the day, and the result was GPT-5 seemed way dumber." https://en.wikipedia.org/wiki/GPT-5 ; https://openai.com/index/introducing-gpt-5/ . The model picker returned: https://techcrunch.com/2025/08/12/chatgpts-model-picker-is-back-and-its-complicated
- Anthropic: no API auto-model endpoint found. Claude Code has aliases (`default`, `best`, `opusplan` = Opus in plan mode then Sonnet for execution), fallback chains (`--fallback-model sonnet,haiku`), classifier-triggered model fallback, and effort levels (`low` to `max`, `ultracode`). "Opus 5.5 at `medium` matches or exceeds Opus 5 at `high`" per Anthropic's testing. https://code.claude.com/docs/en/model-config (accessed 2026-09-27). Intra-provider routing now happens through effort and adaptive thinking rather than model choice.
- Google Gemini: dynamic thinking by default. The model adjusts reasoning to request complexity; `thinking_level` / `thinking_budget` (-1 = dynamic). https://ai.google.dev/gemini-api/docs/thinking . Vertex Model Optimizer (Gemini-only meta-endpoint) existed from 2025. Current status UNVERIFIED (see s.1).
- Hyperscalers bundle cross-provider routers: Azure Model Router routes to Anthropic and xAI models (s.1). AWS IPR is same-family only.
- Erosion assessment (our synthesis, not a sourced claim): providers are shrinking intra-family routing value (effort knobs, adaptive thinking, opusplan). Cross-provider selection stays outside any single model lab, but Azure, OpenRouter/Stripe, Ramp, Vercel and Cloudflare all bundle routing into a gateway or spend product at zero or near-zero marginal price. Menlo's own framing is that the gateway, not the routing, is the product. TensorZero's shutdown notice (via byteiota) cites "native observability, gateway, and evaluation features being shipped directly by Anthropic, OpenAI, and major cloud providers" (secondary interpretation).

## 3. Evidence on routing value and demand

Savings claims (research):
- RouteLLM (ICLR 2025, arXiv 2406.18665): >85% cost reduction on MT Bench while keeping 95% of GPT-4 quality. Matrix-factorization router sent ~14% of queries to the strong model. The pairing was GPT-4 Turbo vs Mixtral 8x7B, which is 2024-era and binary. https://arxiv.org/abs/2406.18665
- RouterBench (2024): https://arxiv.org/pdf/2403.12031

Critiques (research):
- LLMRouterBench (2026-01-12; 400K+ instances, 21 datasets, 33 models, 10 routers): "several recent approaches, including commercial routing systems, failed to consistently surpass" Best Single. OpenRouter showed -24.7% vs Best Single (search summary of paper). The gap is driven by "persistent model-recall failures" ("lone expert" queries). https://arxiv.org/abs/2601.07206
- The Routing Plateau (2026-05-27; 21 methods, 5 benchmarks): methods "converge to a narrow performance range that remains far below the oracle router" and "collectively fail on hard queries that require instance-specific routing decisions." https://arxiv.org/abs/2606.07587
- Opportunity Is Not Realizability (2026-08-08): oracle gains of 9.7 to 30.7 pp, but deployable routers recover only 7.5% to 14.4% of that. The best of 11 policies has a simultaneous CI lower bound of zero vs best single. https://arxiv.org/abs/2608.08265
- Most of the LLM Routing Gap Is Task Type (2026-08-24; 14 models, 294 questions, 7 task types, 3 languages): a static task-type table got 262 correct at $3.33/run vs best single 245 at $7.69. 5.37% of model-question pairs flipped across identical runs, which is run-to-run nondeterminism MM's nightly runs must handle. https://arxiv.org/abs/2608.23023
- Router fragility (2025-03-20): a BERT router sent all coding and math to the strongest model and routed jailbreaks to weaker models. https://arxiv.org/abs/2504.07113
- RouterEval (EMNLP Findings 2025): https://aclanthology.org/2025.findings-emnlp.208/

Vendor savings claims (unaudited): Not Diamond "20-40%"; Bedrock "up to 30%" (AWS internal test showed 60% for the Anthropic family); Thesean "~50%"; Requesty cites "40-85%" (marketing).

Adoption and demand:
- a16z CIO survey (2025-06-10): 37% use 5+ models in production (29% prior year). The main driver is use-case performance differences. Switching costs rising: "all the prompts have been tuned for OpenAI." https://a16z.com/ai-enterprise-2025/ . A search snippet also reports "81% use 3+ model families" (UNVERIFIED against primary).
- a16z (2026-01-30 data, published 2026-02): OpenAI 78% production adoption, Anthropic 44%; ~80% of enterprises comfortable hosting directly with providers. https://a16z.com/leaders-gainers-and-unexpected-winners-in-the-enterprise-ai-arms-race/
- Menlo Ventures 2025 State of GenAI (Dec 2025): enterprise LLM API share Anthropic 40%, OpenAI 27%, Google 21%; enterprise GenAI spend $37B in 2025. https://menlovc.com/perspective/2025-the-state-of-generative-ai-in-the-enterprise/
- OpenRouter + a16z "State of AI" (100T tokens, 2025-12; arXiv 2601.10088): open-weight ~1/3 of tokens; reasoning models >50% of tokens; average prompts ~4x to >6,000 tokens. https://openrouter.ai/state-of-ai ; https://arxiv.org/abs/2601.10088
- Gartner Market Guide for AI Gateways (Oct 2025): 70% of teams building multimodel apps will use AI gateways by 2028 (25% in 2025). TAM $50M to $100M in 2025 (via TrueFoundry summary). https://www.gartner.com/en/documents/7051698 ; https://www.truefoundry.com/blog/building-the-enterprise-ai-control-plane-gartner-r-insights-and-truefoundrys-approach

## 4. Evaluation methodology evidence

- LLM-as-judge: MT-Bench/Chatbot Arena (Zheng et al., 2023) found GPT-4 judges agree with humans >80%, similar to human-human agreement, and documented position, verbosity and self-enhancement bias. https://arxiv.org/abs/2306.05685
- Self-preference bias: https://arxiv.org/abs/2410.21819 ; quantification and mitigation (2026-04): https://arxiv.org/abs/2604.22891 ; mitigation evaluation (2026): https://arxiv.org/abs/2604.23178 ; scoring bias: https://arxiv.org/abs/2506.22316 . Implication for MM: a judge drawn from one of the benchmarked providers (Anthropic/OpenAI/Google/xAI) can favor its own family. Cross-family judge panels and position swapping are standard mitigations.
- Nondeterminism: 5.37% of model-question pairs differ across identical runs (arXiv 2608.23023). Single nightly samples per task will be noisy.
- Contamination / private sets:
  - Scale SEAL: private, unpublished expert datasets. https://scale.com/blog/leaderboard ; https://labs.scale.com/leaderboard
  - LiveBench: new questions monthly, full refresh every 6 months; latest release LiveBench-2026-06-25. https://github.com/livebench/livebench ; https://arxiv.org/abs/2406.19314
  - Artificial Analysis: continuous live API testing of 100+ models. Pro $417/seat/mo; Enterprise custom (150+ employees); no trials. https://artificialanalysis.ai/pricing
- Do evaluators sell per-customer recommendations?
  - LMArena: $150M Series A at ~$1.7B (2026-01-06). Paid evaluation product launched Sep 2025, >$30M annualized run rate by Dec 2025. Customers are mostly labs plus some enterprises. https://techcrunch.com/2026/01/06/lmarena-lands-1-7b-valuation-four-months-after-launching-its-product/
  - Vals AI: $40M Series A at $400M led by a16z (2026-08-13). Sells independent benchmarking to labs and enterprises for "high-stakes decisions about AI model adoption." "Vals Smith" builds a custom coding benchmark from any GitHub repo. Revenue 8x its 2025 total. https://www.finsmes.com/2026/08/vals-ai-raises-40m-in-series-a-funding-at-400m-valuation.html . Vals is the closest per-customer "which model" seller, well funded, and one step from routing.
  - Braintrust sells quality-based model selection from a customer's own experiments/traces (s.1).
  - Artificial Analysis sells data/reports, not per-customer routing (pricing page).

## 5. Provider terms of service (quotes)

Anthropic, Commercial Terms, effective 2025-06-17, https://www.anthropic.com/legal/commercial-terms (fetched 2026-09-27):
> "D.4. Use Restrictions. Customer may not and must not attempt to (a) access the Services to build a competing product or service, including to train competing AI models or resell the Services except as expressly approved by Anthropic; (b) reverse engineer or duplicate the Services; or (c) support any third party's attempt at any of the conduct restricted in this sentence."
- No clause on publishing benchmark results found.
- Enforcement precedent: Anthropic revoked OpenAI's API access around 2025-08-01, alleging staff used Claude to "develop and benchmark GPT-5." Anthropic said it would keep access "for benchmarking and safety evaluation." https://developers.slashdot.org/story/25/08/01/2237220/anthropic-revokes-openais-access-to-claude-over-terms-of-service-violation ; https://www.technology.org/2025/08/04/anthropic-terminates-claude-ai-model-access-for-openai/

OpenAI, Services Agreement ONLINE v.010126, https://cdn.openai.com/osa/openai-services-agreement.pdf (text extracted 2026-09-27):
> "3.3. Restrictions. Customer will not, and will not permit End Users to: ... (d) Reverse Engineer any aspect of the Services ...; (e) except for a Permitted Exception, use Output to develop artificial intelligence models that compete with OpenAI's products and services; (f) extract data from the Services other than as permitted through the Services; (g) buy, sell, or transfer API keys from, to, or with a third party; (h) interfere with or disrupt the Services, including circumvent any rate limits ..."
> "'Permitted Exception' means Customer using Output to: (a) develop artificial intelligence models primarily intended to categorize, classify, or organize data (e.g., embeddings or classifiers), if these models are not distributed or made commercially available to third parties; ..."
- Relevance: an MM prompt classifier trained on OpenAI outputs is inside the exception only if it is not "made commercially available to third parties." Selling classification as a service arguably falls outside it. No benchmark-publication clause found.

Google, Gemini API Additional Terms, last modified 2026-03-23, https://ai.google.dev/gemini-api/terms :
> "You may not use the Services to develop models that compete with the Services (e.g., Gemini API or Google AI Studio). You also may not attempt to reverse engineer, extract or replicate any component of the Services, including the underlying data or models (e.g., parameter weights)."

Google Cloud Service Specific Terms (Vertex / Agent Platform), https://cloud.google.com/terms/service-terms (fetched 2026-09-27; Cloud terms last modified 2026-09-02):
> "7. Benchmarking. Customer may itself (but may not permit a third party to): (a) conduct benchmark tests of the Services (each a "Test"); and (b) publicly disclose the results of such Tests only if (i) the public disclosure includes all necessary information to replicate the Tests, and (ii) Customer allows Google to conduct benchmark tests of Customer's publicly available products or services and publicly disclose the results of such tests."
> "17. Use Restrictions for AI/ML Services ... a. Competitive Use. Customer will not, and will not allow End Users to use an AI/ML Service or Generated Output to develop a similar or competing product or service. ... This restriction does not apply to Gemini Enterprise Agent Platform (formerly Vertex AI Platform) so long as Customer does not use a Google Pre-Trained Model."
- Relevance: if MM calls Gemini via Vertex/Agent Platform, publishing scores from a private corpus conflicts with s.7(b)(i). The AI Studio Gemini API terms have no equivalent clause (not found).

xAI (now "SpaceXAI"), Terms of Service Enterprise, last updated 2026-08-14 (Wayback copy of https://x.ai/legal/terms-of-service-enterprise). They incorporate the AUP and state "SpaceXAI may monitor usage ... [and] throttle or block specific Inputs or Outputs." The AUP, effective 2026-08-14, https://x.ai/legal/acceptable-use-policy (Wayback copy), prohibits:
> "Modifying, copying, translating, leasing, selling, reselling, distributing, distilling, manipulating, using bots to access, reverse engineer, decompile, disassemble or otherwise seek to obtain the source code of our Service ..."
> "Using the Service or any Output to develop (or assist anyone in developing) machine learning models or any products or services that compete with SpaceXAI, whether directly or indirectly"
> "Scraping, harvesting or reselling any Input or Output, or distilling model data or Outputs"
- A secondary claim that xAI terms bar use "for benchmarking or competitive analysis of the Service" was NOT found in the 2026-08-14 enterprise terms or AUP text (UNVERIFIED; may be from an older version).
- Relevance: "using bots to access" read literally covers any automated API use, so it is almost certainly not enforced against normal API clients, but it is broad. "Any products or services that compete ... directly or indirectly" is the broadest competition clause of the four providers.

Summary for MM: no provider explicitly forbids private automated benchmarking for selection purposes. Risk points: (a) Anthropic's "resell the Services" and "competing product" language if the trial proxy serves tenants on MM's keys; (b) Google Cloud's replication requirement for publishing results; (c) OpenAI's classifier exception excludes commercially available models; (d) xAI's broad "directly or indirectly" competition clause.

## 6. Rust vs Go ecosystem facts (as of 2026-09-27)

Official provider SDKs:
- Anthropic: official client SDKs for "Python, TypeScript, C#, Go, Java, PHP, and Ruby". No Rust. https://platform.claude.com/docs/en/cli-sdks-libraries/overview . Go: anthropics/anthropic-sdk-go v1.75.0 (2026-09-22).
- OpenAI: official JS/TS, Python, .NET, Java (beta), Go (listed as beta on docs page), Ruby. "No official Rust SDK"; the community `async-openai` is listed "use at your own risk". https://developers.openai.com/api/docs/libraries . Go: openai/openai-go v3.66.0 (2026-09-23). async-openai 0.42.0 (2026-09-09, ~8.6M downloads).
- Google GenAI SDK: GA for Python, JS/TS, Go, Java, C#. No Rust. https://ai.google.dev/gemini-api/docs/libraries . Go: googleapis/go-genai v1.71.0 (2026-08-31).
- xAI: not checked (UNVERIFIED). xAI's API is OpenAI-compatible, so OpenAI SDKs are commonly used.

Rust crates (crates.io API, 2026-09-27):
- rmcp (official MCP Rust SDK, https://github.com/modelcontextprotocol/rust-sdk): v3.4.1 released 2026-09-23; ~29.7M total downloads, ~15.6M recent; 4K stars. Implements MCP spec 2026-07-28 (stateless by default per SEP-2567) and stays compatible with 2025-11-25. Go equivalent: modelcontextprotocol/go-sdk v1.8.0 (2026-09-14), 5.2K stars.
- sqlx: 0.9.0 released 2026-05-21; breaking changes (query functions take `impl SqlSafeStr`); repo moved to the transact-rs org because "SQLx has not been owned or maintained by LaunchBadge, LLC for a few years." https://github.com/launchbadge/sqlx/discussions/4271
- apalis: stable 0.7.4 (2025-11-18); 1.0.0-rc.10 (2026-09-15); apalis-postgres has only 1.0.0-rc.9, no stable release. https://github.com/apalis-dev/apalis
- graphile_worker (Rust port): 0.13.5 (2026-07-20), supports job keys (dedupe), priorities, retries; small (84 stars, ~61K downloads). https://github.com/leo91000/graphile_worker_rs
- tokio 1.53.1 (2026-07-20); axum 0.8.9 (2026-04-14).

Go:
- River: v0.47.0 (2026-08-31), 5.7K stars, pre-1.0, MPL-2.0. Has unique jobs (https://riverqueue.com/docs/unique-jobs), a web UI, and resumable multi-step jobs added in 2026. https://github.com/riverqueue/river

Comparable production gateways by language:
- Rust: TensorZero (shut down 2026-06-12; said it "fuels ~1% of global LLM API spend"; claimed <1ms p99 gateway latency), Helicone ai-gateway (dormant since 2025-11), Katanemo Plano (now DigitalOcean, active). Requesty is described as Rust by a secondary source (UNVERIFIED).
- Go: Bifrost (Maxim), v1.10.4 core (2026-09-25), 8.4K stars. Claims ~11 microseconds overhead at 5k RPS on t3.xlarge (59 on t3.medium) vs ~600 for LiteLLM (vendor benchmark). https://github.com/maximhq/bifrost ; https://www.getmaxim.ai/bifrost/resources/benchmarks
- Python: LiteLLM. TypeScript: Portkey gateway, Helicone.
- Takeaway: both Rust and Go have credible gateway precedents. Go has the official SDKs from all three majors, a mature Postgres queue with uniqueness (River), and an official MCP SDK. Rust has rmcp and sqlx but no official provider SDKs and no stable Postgres job queue with uniqueness beyond graphile_worker.

## 7. Buyer concerns (enterprise routing/gateway)

- Security and privacy is the top barrier: 44% of enterprises cite data privacy and security as the top barrier to LLM adoption (Kong, 550 respondents, 2025). https://konghq.com/blog/enterprise/enterprise-ai-spending-2025 ; https://www.prnewswire.com/news-releases/study-finds-72-of-enterprises-plan-to-ramp-spending-on-genai-in-2025-302484025.html
- Data residency is table stakes for competitors. OpenRouter offers in-region routing (us./eu.openrouter.ai) on Business/Enterprise plans plus per-request Zero Data Retention: https://openrouter.ai/docs/guides/features/sovereign-ai . Azure Model Router honors "data zone boundaries" and supports Data Zone Standard deployments (s.1). Requesty sells EU residency.
- Certifications: Not Diamond claims SOC 2 + ISO 27001 and "privacy-preserving deployment"; Vercel sells team-wide ZDR ($0.10/1K requests); LiteLLM Enterprise sells SSO/SAML, audit logs, SLA.
- Prompt handling: Azure states the router "does not store your prompts." Not Diamond Code keeps payloads local and sends only "anonymized, derived metadata." Buyers will ask MM the same question about its classifier, because MM sees prompts in order to classify them.
- Latency overhead: gateway vendors compete on microseconds (Bifrost 11 microseconds; TensorZero <1ms p99). A recommend-then-call design adds a network round trip. Secondary (vendor) guidance: "choose on data residency first, then high availability and overhead, then governance depth," and managed-only gateways are "generally disqualifying for regulated workloads." https://www.getmaxim.ai/articles/best-self-hosted-ai-gateway-in-2026/
- Deployment: self-hosted, in-VPC or air-gapped options are standard asks (LiteLLM, Bifrost, Plano are all self-hostable OSS).
- Vendor viability: the 2026 exits (Helicone maintenance mode, TensorZero archive, Portkey absorbed into a security suite) give buyers a concrete reason to ask a solo-operator vendor about continuity.
- Governance gap (secondary): 71% of orgs use GenAI but only 21% have mature governance (a "McKinsey 2026 AI Trust Maturity Survey," cited by https://www.index.dev/blog/llm-enterprise-adoption-statistics). UNVERIFIED primary.

## Not covered / gaps
- Current Vertex Model Optimizer status and price, Azure router's official price page (JS-rendered), xAI SDK status, and the Stripe/OpenRouter closing date: all UNVERIFIED.
- No primary pricing found for Martian (defunct router), Unify (defunct router) or Arch-Router (OSS).
