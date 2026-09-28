# Capability Matrix Design — Measurement Methodology

Status: draft for review
Companion to: `specs/model-maverick-v1-spec.md` (engineering spec) and `plans/model-maverick-v1-plan.md` (product plan)

This document specifies the measurement layer: what each capability grades, how difficulty is authored, how judges are handled, and the statistical treatment that turns raw runs into rankings. The engineering spec defines the machinery (grader kinds, formulas, storage); this document defines the content that machinery runs on and the methodology behind it. Section 9 maps every methodological choice to its research grounding.

---

## 1. Measurement principles

1. **Private corpus.** Golden answers, hidden rubrics, reference solutions, and test material never enter any benchmarked model's prompt and never leave the engine. This blocks training-data contamination and keeps scores meaningful over time.
2. **Grader-per-task.** Every task carries its grader as data, validated at authoring time. There is no global "grade everything with a judge" path.
3. **Verifiable first, rubric second.** Deterministic and reference graders answer *whether* the problem was solved; the judge answers *how well* — and only for dimensions that resist deterministic checking (idiom, readability, tone). The judge never carries a task alone in v1: every task has at least one non-judge grader.
4. **Difficulty is authored, not assumed.** Tiers 1–5 are set by explicit structural knobs per capability (§5), so failure localizes to a tier instead of averaging away.
5. **Distrust single measurements.** Three repeats, variance penalty, ambiguity penalty, EWMA smoothing, and locked-before-results prediction tracking (§7).

---

## 2. Grading dimension conventions

- Dimensions score in [0,1]; task `dimension_weights` sum to 1.
- Dimension names are stable identifiers shared across tasks within a capability (they aggregate into cells).
- Wherever a judge participates, at least one of its dimensions is also fed by a deterministic or reference grader (shared via `weight_dimensions`), so grader disagreement on a single output is measurable — the ambiguity penalty (engineering spec §6.3).
- Grader kinds referenced below: `json_schema`, `rule_scan`, `golden_answer`, `key_facts`, `judge_rubric` (engineering spec §5.3). `test_suite` is deferred past v1; code is graded without execution (see `code_generation`).

---

## 3. Grading dimensions per capability

Weights below are the capability defaults; individual tasks may shift them ±0.10 where the task's emphasis differs, and the high-stakes overlay (§4) replaces them entirely.

### 3.1 code_generation

| Dimension | Weight | Grader(s) |
|---|---|---|
| functional_correctness | 0.45 | `judge_rubric` against hidden reference solution + `rule_scan` static assertions (required signatures, API usage, forbidden patterns) |
| spec_compliance | 0.25 | `rule_scan` (constraints named in the spec: language, imports, structure) + `key_facts` (required behaviors named in output/comments where checkable) |
| code_quality_idiom | 0.20 | `judge_rubric` (idiomatic style, naming, structure for the target language) |
| safety_robustness | 0.10 | `rule_scan` (input validation present, no obvious injection/panic patterns; task-specific) |

V1 caveat, stated plainly: without execution, `functional_correctness` is judge-anchored (the judge diffs the output against the hidden reference solution and the task's failure modes). This is the weakest deterministic coverage of any capability and is why `test_suite` is the first v1.1 grader. Rankings on this capability should be read with that caveat until then; the `ambiguity` dimension is expected to run higher here.

### 3.2 structured_extraction

| Dimension | Weight | Grader(s) |
|---|---|---|
| schema_validity | 0.30 | `json_schema` |
| field_accuracy | 0.40 | `json_schema` per-field checks + `golden_answer` (normalized values, numeric tolerance) |
| completeness | 0.20 | `key_facts` (every extractable fact present) |
| hallucination_free | 0.10 | `key_facts` forbidden-facts (fields fabricated from distractor content subtract) |

Fully objective; no judge. This is why extraction ships first (plan M1).

### 3.3 summarization_synthesis

| Dimension | Weight | Grader(s) |
|---|---|---|
| key_fact_coverage | 0.35 | `key_facts` |
| faithfulness | 0.30 | `key_facts` forbidden-facts (planted contradictions and unsupported claims) + `judge_rubric` |
| compression_adherence | 0.15 | `rule_scan` (length bounds, structural requirements) |
| readability | 0.20 | `judge_rubric` |

### 3.4 multi_step_reasoning

| Dimension | Weight | Grader(s) |
|---|---|---|
| final_answer_correctness | 0.50 | `golden_answer` (exact/numeric) |
| constraint_satisfaction | 0.30 | `rule_scan` (each stated constraint checkable in the answer) |
| step_validity | 0.20 | `judge_rubric` (does the shown work support the answer; penalizes right-answer-wrong-reasoning) |

### 3.5 instruction_schema_following

| Dimension | Weight | Grader(s) |
|---|---|---|
| constraint_compliance | 0.50 | `rule_scan` (one rule per authored constraint; critical rules for hard constraints) |
| format_validity | 0.35 | `json_schema` or `rule_scan` structural rules |
| completeness | 0.15 | `key_facts` (all requested parts present) |

Fully objective; no judge.

### 3.6 long_context_qa

| Dimension | Weight | Grader(s) |
|---|---|---|
| answer_correctness | 0.50 | `golden_answer` / `key_facts` |
| grounded_refusal | 0.25 | `golden_answer` — tasks where the answer is absent from the context have "correctly declines" as the golden answer (the RAG-refusal dimension from the plan's taxonomy test) |
| multi_hop_synthesis | 0.25 | `key_facts` (facts that require combining separated passages) |

### 3.7 factual_qa

| Dimension | Weight | Grader(s) |
|---|---|---|
| accuracy | 0.50 | `golden_answer` |
| false_premise_detection | 0.20 | `golden_answer` (premise-rejecting answer is golden) |
| calibrated_hedging | 0.20 | `rule_scan` (confident wrong claims on trap items fail critical rules) + `judge_rubric` |
| precision | 0.10 | `golden_answer` numeric/tolerance forms |

### 3.8 rewriting_editing

| Dimension | Weight | Grader(s) |
|---|---|---|
| meaning_preservation | 0.35 | `key_facts` (facts that must survive the rewrite) + forbidden-facts (traps that must not appear) |
| constraint_compliance | 0.30 | `rule_scan` |
| tone_register_target | 0.20 | `judge_rubric` |
| format_conversion | 0.15 | `json_schema` / `rule_scan` structural |

### 3.9 translation_multilingual

| Dimension | Weight | Grader(s) |
|---|---|---|
| adequacy | 0.40 | `key_facts` (meaning units, accepted surface forms per language) |
| terminology | 0.25 | `rule_scan` / `golden_answer` (domain terms, required renderings) |
| fluency | 0.20 | `judge_rubric` (judge prompted in the target language) |
| register_constraints | 0.15 | `rule_scan` (formality markers) + `judge_rubric` |

### 3.10 critique_and_judging

| Dimension | Weight | Grader(s) |
|---|---|---|
| defect_detection | 0.40 | `key_facts` (the planted defect list; each defect has accepted descriptions) |
| verdict_accuracy | 0.30 | `golden_answer` (correct accept/reject or ranking of near-correct outputs) |
| severity_calibration | 0.15 | `golden_answer` (severity labels per defect) |
| rubric_adherence | 0.15 | `rule_scan` (follows the requested critique format) |

Fully objective by construction (defects are planted, verdicts are known) — deliberately, because these scores double as empirical judge selection (plan: the judge role goes to models that measurably rank well at judging).

---

## 4. High-stakes overlay

Switched on when `domain_tags` intersect {medical, legal, financial}. The overlay **replaces** the task's dimension weights:

| Overlay dimension | Weight | Source |
|---|---|---|
| factual_accuracy | 0.40 | the task's accuracy-class dimensions (accuracy, field_accuracy, answer_correctness, adequacy...) renormalized |
| calibration_safe_hedging | 0.20 | calibrated_hedging / grounded_refusal class dimensions; if the task has none, a standing `rule_scan` overlay spec (uncertainty acknowledged where the material is ambiguous) |
| citation_honesty | 0.15 | `key_facts` forbidden-facts over fabricated citations/sources + `rule_scan` (citations must resolve to supplied material) |
| task_quality | 0.25 | the task's remaining dimensions renormalized |

**Autofail:** any of the following forces the run's quality to 0 regardless of dimensions (engineering spec §6.1):

- a dangerous recommendation (task-authored critical rules: e.g. dosage errors, "you don't need a lawyer for this filing", advice contradicting planted safety-critical facts),
- a fabricated citation (cites a source not in the supplied material as if it were),
- a fabricated fact presented with confidence on a trap item.

One dangerous or fabricated answer fails the task (plan §Workload taxonomy). Autofail conditions are authored per task as `critical: true` rules or forbidden-facts with `autofail: true`.

---

## 5. Difficulty methodology

### 5.1 Knobs

Difficulty is authored by setting explicit structural knobs (plan's taxonomy table). Every task records its knob settings in `difficulty_knobs` jsonb, so failures localize ("fails when nesting_depth ≥ 3") rather than averaging away.

| Capability | Knobs |
|---|---|
| code_generation | files_touched, dependency_depth, spec_ambiguity, adversarial_tests |
| structured_extraction | document_noise, field_count, nesting_depth, distractor_content |
| summarization_synthesis | source_length, redundancy, contradictions, compression_ratio |
| multi_step_reasoning | step_count, constraint_interaction, branching, misleading_framing |
| instruction_schema_following | constraint_count, schema_strictness, format_edge_cases |
| long_context_qa | context_length, needle_depth, hops, contradictions |
| factual_qa | fact_obscurity, entity_confusability, false_premises, required_precision |
| rewriting_editing | constraint_count, tone_distance, meaning_traps, format_conversion |
| translation_multilingual | language_pair_distance, idiom_density, domain_terminology, register_constraints |
| critique_and_judging | defect_subtlety, rubric_strictness, near_correct_outputs, issue_count |

Each knob takes an ordinal setting 1–5 defined in the capability's authoring notes (e.g. structured_extraction `nesting_depth`: 1 = flat, 3 = two levels with arrays, 5 = four+ levels with heterogeneous arrays).

### 5.2 Tier assignment

A task's tier is the rounded mean of its knob settings, with two authored overrides allowed: ±1 tier where the authoring notes justify it (a single extreme knob can dominate), never ±2. Tier anchors:

- **Tier 1:** every knob at minimum; a competent small model should pass ~always. Exists to catch catastrophic regressions.
- **Tier 3:** the median real-world production task; the tier the balanced mode's economics revolve around.
- **Tier 5:** multiple knobs at maximum simultaneously; current frontier models pass sometimes, not reliably. Exists to separate the top of the market.

### 5.3 Tier calibration check

Authored difficulty is a hypothesis; observed pass rates test it. Monthly (and at every corpus version bump), for each capability: compute per-tier pass rates averaged over measured columns. Expected: monotone decreasing in tier. A task whose pass rate is >15 points out of line with its tier's neighbors across most columns is flagged for re-tiering or re-authoring; re-tiering bumps `matrix_version`. This is the pass-rate-ceiling regime that v1.1's IRT fit replaces with jointly estimated task difficulty and model ability — the nightly pass data collected from M1 onward is exactly the IRT input.

### 5.4 Corpus shape

20–25 tasks per capability: 4–5 per tier, spanning different knob combinations (not five copies of the same shape). 5 smoke tasks per capability: one tier-1 or 2 easy anchor, one tier-4 or 5 hard anchor, three spread across the middle, chosen for high signal (historically discriminative between adjacent-quality columns). Long-context smoke variants are authored ≤32k tokens; the full-length versions run in the weekly sweep only.

---

## 6. Judge handling

### 6.1 Bias taxonomy and mitigations

The 2025–2026 judge-bias literature (§9) found frontier judges exceeding 50% error on hard bias tests, with no judge uniformly reliable. Maverick's mitigations, mapped to the five documented biases:

| Bias | Mitigation |
|---|---|
| Position | Order swap: two calls per (output, rubric) with item order reversed; scores averaged; disagreement feeds ambiguity |
| Verbosity | Rubrics score named qualities, never "which is better"; length-sensitive dimensions carry explicit `rule_scan` length bounds so the judge never rewards padding |
| Self-preference | Cross-family judging: a column's outputs are never judged by a same-family judge |
| Format | Judge receives outputs stripped to content where the task permits (fenced code unwrapped); format compliance is graded deterministically, not by the judge |
| Authority | Task prompts and judge inputs never carry provider/model identity; reference-anchored rubrics (the judge compares to supplied reference material, not its own priors) |

### 6.2 Protocol

Engineering spec §5.5 is normative for mechanics (pool of two cross-family judges, temperature 0, structured output, retry/failure handling). Methodological additions:

- Judges are graded like any other column: `critique_and_judging` cells measure them, and repeat-and-variance treatment applies to judge outputs identically (the code-judge reliability finding: judges flip verdicts on identical cases).
- Judge prompts are corpus data, versioned with the corpus; a judge-prompt edit bumps `matrix_version`.

### 6.3 Monthly calibration

Each month: sample 20 judge-graded outputs (stratified across capabilities and score range), operator scores them blind against the same rubric, compute mean absolute deviation and rank agreement. MAD > 0.15 or visible systematic skew → judge column goes under review: check for provider-side model updates, re-run the sample on the alternate judge, re-point `judge.pool` if the alternate calibrates better. Calibration results are recorded (date, sample, MAD) in the corpus repository for trend visibility.

### 6.4 Empirical judge selection

Once `critique_and_judging` aggregates are `measured` for the candidate columns, the judge pool is re-pointed to the top-ranked judging columns satisfying the cross-family coverage constraint (the two pool members must come from two different families, and together must cover judging all four launch families cross-family). Reputation never assigns the judge role.

---

## 7. Statistical methodology

Formulas and constants are normative in the engineering spec §6; this section records the reasoning.

- **Three repeats** balances variance detection against cost; BenchPress-style sparse-observation results (§9) show small dense samples predict well, and repeats are the densest signal per dollar. Variance subtracts (λ_var = 0.5) because a 4-of-5 passer is riskier than its mean suggests.
- **Ambiguity is separate from variance** — variance is disagreement across runs; ambiguity is grader disagreement about a single output. Both make a score less trustworthy for a recommendation, so both subtract, with ambiguity weighted lower (λ_amb = 0.25) because it is bounded by grader design rather than model behavior.
- **EWMA (α = 0.3)** makes rankings a function of the trend, not the night: one bad night moves a smoothed score ~⅓ of the way, and the regression scan (drop > 3 points) alerts before the ranking flips silently.
- **Prediction tracking:** before each batch, predicted pass probabilities per cell = current `pass_rate` EWMA, locked to `predictions`. After grading:
  `Brier = mean over (task,column,repeat) of (p − o)²` with o ∈ {0,1}, per capability; `overconfidence = mean(p) − mean(o)`.
  Locking before results makes hindsight impossible; the per-night series shows where the benchmark predicts itself well and where it is blind. This history is the gating evidence for v1.1 matrix completion: a cell may be predicted instead of run only where the Brier history shows the benchmark reliably predicts that cell.
- **Cold start:** new columns run dense (full corpus, 5 nights) before their scores drive recommendations; `measured: false` until 3 observations per cell. Matches the BenchPress finding that roughly a third of cells observed supports prediction of the rest — but v1 predicts nothing; it only collects the evidence.

---

## 8. Corpus authoring guide

### 8.1 Task file format

One YAML file per task, in the private corpus repository, filename = task id.

```yaml
id: task_sx_017
capability: structured_extraction
domain_tags: [legal]
difficulty_tier: 3
difficulty_knobs: { document_noise: 3, field_count: 3, nesting_depth: 2, distractor_content: 4 }
prompt: |
  Extract the parties, effective date, term, and payment obligations from the
  following services agreement. Return JSON matching the provided schema.
  ...
attachments:
  - name: agreement.txt
    content: |
      ... (the contract text, including distractor clauses) ...
temperature: 0
max_tokens: 2000
pass_threshold: 0.7
smoke_member: false
dimension_weights: { schema_validity: 0.3, field_accuracy: 0.4, completeness: 0.2, hallucination_free: 0.1 }
grader_specs:
  - type: json_schema
    version: 1
    weight_dimensions: { schema_validity: 1.0, field_accuracy: 1.0 }
    spec:
      schema: { "$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object", "required": [parties, effective_date, term, payment], "properties": { ... } }
      checks:
        - { path: $.effective_date, equals: "2025-03-01" }
        - { path: $.payment.amount_usd, numeric: 12500, tolerance: 0 }
  - type: key_facts
    version: 1
    weight_dimensions: { completeness: 1.0, hallucination_free: 1.0 }
    spec:
      facts:
        - { id: party_acme, any_of: ["Acme Corp", "Acme Corporation"], weight: 1 }
        - { id: term_24mo, any_of: ["24 month", "two-year", "24-month"], weight: 1 }
      forbidden_facts:
        - { id: distractor_2019_date, any_of: ["2019-06-15"], weight: 1 }   # date from a distractor clause
```

### 8.2 Authoring rules

1. **Nothing hidden enters the prompt.** Golden values, fact lists, forbidden facts, rubrics, and reference solutions live only in grader specs (G7). If a task can't be graded without leaking its answer into the prompt, redesign the task.
2. **Every task has a non-judge grader.** The judge only grades dimensions that resist deterministic checking.
3. **Traps are deliberate.** Distractor content, false premises, planted contradictions, and forbidden facts are authored knobs, not accidents — record them in `difficulty_knobs`.
4. **Determinism first.** Temperature 0 unless the task measures something temperature-dependent; accepted answer forms enumerated exhaustively (run the grader against 3 known-good phrasings before committing).
5. **Self-test at authoring.** `maverick corpus load` validates schemas; the author additionally runs the task against two current columns and reads the graded output — a task no strong model can pass, or every weak model passes, is mis-tiered (§5.3 will catch it later, but catch it now).
6. **High-stakes tasks author their autofails** explicitly (§4): critical rules and autofail forbidden-facts, reviewed extra carefully because one of them zeroes a run.
7. **Versioning:** any change to a task's prompt, graders, or tier is a new corpus version (`matrix_version` bump). Old scores are never restated; they carry the version that produced them.

### 8.3 Review checklist (per task, before corpus activation)

- [ ] Grader specs validate; weights sum to 1
- [ ] Knobs recorded and consistent with tier (§5.2)
- [ ] Accepted-forms tested against 3 known-good phrasings
- [ ] Traps/forbidden facts genuinely absent from a correct answer
- [ ] No golden material in prompt or attachments
- [ ] Smoke flag consistent with capability's 5-task budget (loader enforces)
- [ ] High-stakes: autofail rules reviewed word by word

---

## 9. Research grounding

Sources from the plan's References, mapped to the decisions they drove:

| Source | Decision it drove |
|---|---|
| SCAN (Wang, Gu, et al., arXiv 2505.06698) | Capability-first taxonomy; per-capability feedback instead of a single leaderboard rank (§3; plan taxonomy) |
| X-RAY (Gao et al., arXiv 2603.05290) | Difficulty as explicit structural knobs so failure localizes (§5.1–5.2) |
| BenchPress (anadim/llm-benchmark-matrix) | 5-task smoke sets, cold-start dense runs, v1.1 matrix completion gated on demonstrated predictability (§5.4, §7) |
| Siciliani decision matrix | Hard pre-filters (licensing/privacy/policy) before scoring — the policy engine's step order (engineering spec §7.1) |
| Judge-bias literature: JudgeBiasBench (Zhou et al.), FairJudge (Yang et al.), RAND reliability study | Five-bias taxonomy and every mitigation in §6.1 |
| Code-judge reliability (arXiv 2604.16790) | Judges get repeat-and-variance treatment; order-swap disagreement feeds ambiguity (§6.2) |
| RLVR-style hybrid evaluation norm (2026) | Verifiable-first, rubric-second grader pairing (§1, principle 3) |
| RouteLLM (Ong et al., ICLR 2025) | Positioning: Maverick is the offline measurement layer a router consumes; banded policy as the router-facing artifact |
| RouterBench (Hu et al., arXiv 2403.12031) | Ranked list as fallback chain — a pool can beat its best single member |
| IRT-for-LLM-eval literature (2025–2026) | §5.3's pass-rate regime is explicitly the placeholder for the v1.1 IRT fit; nightly pass data is collected in IRT-ready form |
| Dynamic-routing survey (2026) | Decision-before-request, measured-past-performance, offline-computed ranking table — Maverick's placement on all three axes |
| TrueFoundry cascade postmortem (2026) | Parse-failure-rate alert as the leading indicator of silent caller-side escalation (engineering spec §11) |
| Vertex Model Garden / Bedrock-OpenAI announcement / Azure Model Router docs | Pool design for compliance-restricted tenants; the three recommendation modes |

---

*End of measurement methodology.*
