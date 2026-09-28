// ModelMaverick test agent — plays the role a real SDK will play (G13/G14):
// ask /v1/recommend, execute the chain with fallback, auto-report telemetry,
// then verify the engine derived the right outcomes. Zero dependencies;
// run with Node >= 23.6 (native type stripping):
//
//   MAVERICK_API_KEY=mk_live_... node agent.ts
//
// Env:
//   MAVERICK_BASE_URL         default http://localhost:8080
//   MAVERICK_API_KEY          required (tenant key)
//   MODELMAVERICK_TELEMETRY   "off" disables all telemetry traffic (G13)
//   MODELMAVERICK_EXECUTION   "direct" (default) | "openrouter" — openrouter
//                             mode uses each entry's execution_aliases slug as
//                             the wire model and stamps endpoint_used
//   ANTHROPIC_API_KEY         optional; when set, the happy-path scenario
//                             makes a real Anthropic call instead of simulating

const BASE = process.env.MAVERICK_BASE_URL ?? "http://localhost:8080";
const KEY = process.env.MAVERICK_API_KEY;
const TELEMETRY_ON = (process.env.MODELMAVERICK_TELEMETRY ?? "on") !== "off";
const EXECUTION = (process.env.MODELMAVERICK_EXECUTION ?? "direct") as
  | "direct"
  | "openrouter";

if (!KEY) {
  console.error("MAVERICK_API_KEY is required");
  process.exit(2);
}

interface ChainEntry {
  provider: string;
  model: string;
  endpoint: string;
  column_id: string;
  score: number | null;
  execution_aliases: Record<string, string>;
}

interface RecommendResponse {
  recommendation_id: string;
  measured: boolean;
  recommendations: ChainEntry[];
}

interface AttemptResult {
  status: number; // 0 = transport failure
  latency_ms?: number;
  tokens_in?: number;
  tokens_out?: number;
  error_class?: "timeout" | "connect_error" | "provider_error" | "other";
  check_pass?: boolean;
}

interface TelemetryItem extends AttemptResult {
  recommendation_id: string;
  model_used: string;
  endpoint_used?: string;
}

async function api(path: string, init?: RequestInit): Promise<Response> {
  return fetch(`${BASE}${path}`, {
    ...init,
    headers: {
      Authorization: `Bearer ${KEY}`,
      "Content-Type": "application/json",
      ...init?.headers,
    },
  });
}

async function recommend(workload: object): Promise<RecommendResponse> {
  const res = await api("/v1/recommend", {
    method: "POST",
    body: JSON.stringify({ workload }),
  });
  if (!res.ok) throw new Error(`recommend failed: ${res.status}`);
  return (await res.json()) as RecommendResponse;
}

// --- the chain executor (the heart of what an SDK does) -------------------

function shouldAdvance(r: AttemptResult): boolean {
  if (r.error_class === "timeout" || r.error_class === "connect_error") return true;
  return r.status === 0 || r.status === 429 || r.status >= 500;
}

// In openrouter mode the wire model is the entry's alias; alias-less entries
// are skipped (spec: no_executable_candidates when none remain).
function wireModel(entry: ChainEntry): string | null {
  if (EXECUTION === "openrouter") return entry.execution_aliases?.openrouter ?? null;
  return entry.model;
}

async function realAnthropicCall(model: string): Promise<AttemptResult> {
  const started = Date.now();
  const res = await fetch("https://api.anthropic.com/v1/messages", {
    method: "POST",
    headers: {
      "x-api-key": process.env.ANTHROPIC_API_KEY!,
      "anthropic-version": "2023-06-01",
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      model,
      max_tokens: 64,
      messages: [{ role: "user", content: "Reply with exactly: pong" }],
    }),
  });
  const latency_ms = Date.now() - started;
  if (!res.ok) return { status: res.status, latency_ms };
  const body = (await res.json()) as {
    usage?: { input_tokens?: number; output_tokens?: number };
  };
  return {
    status: 200,
    latency_ms,
    tokens_in: body.usage?.input_tokens,
    tokens_out: body.usage?.output_tokens,
    check_pass: true,
  };
}

type Simulator = (entry: ChainEntry, attempt: number) => AttemptResult | "real";

async function executeChain(
  rec: RecommendResponse,
  simulate: Simulator,
  telemetry: TelemetryItem[],
): Promise<{ used: string | null; attempts: number }> {
  let attempt = 0;
  for (const entry of rec.recommendations) {
    const model = wireModel(entry);
    if (model === null) continue; // alias-less entry in openrouter mode
    attempt++;
    const planned = simulate(entry, attempt);
    const result =
      planned === "real" ? await realAnthropicCall(model) : planned;
    telemetry.push({
      recommendation_id: rec.recommendation_id,
      model_used: entry.model,
      ...(EXECUTION === "openrouter" ? { endpoint_used: "openrouter" } : {}),
      ...result,
    });
    if (!shouldAdvance(result)) return { used: entry.model, attempts: attempt };
  }
  return { used: null, attempts: attempt }; // chain exhausted
}

// --- telemetry batcher: one fire-and-forget flush, never throws (G14) ------

async function flushTelemetry(
  items: TelemetryItem[],
): Promise<{ accepted: number; dropped: number } | null> {
  if (!TELEMETRY_ON || items.length === 0) return null;
  try {
    const res = await api("/v1/telemetry", {
      method: "POST",
      body: JSON.stringify({ items }),
    });
    if (!res.ok) return null; // best-effort: drop silently
    return (await res.json()) as { accepted: number; dropped: number };
  } catch {
    return null;
  }
}

// --- scenarios --------------------------------------------------------------

interface Scenario {
  name: string;
  workload: object;
  simulate: Simulator;
  expectOutcomes: string[]; // derived kinds, in telemetry order
}

const scenarios: Scenario[] = [
  {
    name: "happy path",
    workload: { capability: "structured_extraction", difficulty: 3 },
    simulate: (entry) =>
      process.env.ANTHROPIC_API_KEY &&
      entry.provider === "anthropic" &&
      EXECUTION === "direct"
        ? "real"
        : { status: 200, latency_ms: 1200, tokens_in: 800, tokens_out: 150, check_pass: true },
    expectOutcomes: ["success"],
  },
  {
    name: "provider 503 → fallback to next entry",
    workload: { capability: "summarization_synthesis", difficulty: 2 },
    simulate: (_e, attempt) =>
      attempt === 1
        ? { status: 503, latency_ms: 90 }
        : { status: 200, latency_ms: 900, check_pass: true },
    expectOutcomes: ["provider_error", "success"],
  },
  {
    name: "output fails check spec",
    workload: { capability: "structured_extraction", difficulty: 4 },
    simulate: () => ({ status: 200, latency_ms: 700, check_pass: false }),
    expectOutcomes: ["parse_failure"],
  },
  {
    name: "timeout",
    workload: { capability: "multi_step_reasoning", difficulty: 3 },
    simulate: (_e, attempt) =>
      attempt === 1
        ? { status: 0, error_class: "timeout" }
        : { status: 200, latency_ms: 1100, check_pass: true },
    expectOutcomes: ["latency_exceeded", "success"],
  },
];

// --- run --------------------------------------------------------------------

async function main(): Promise<void> {
  let failures = 0;
  const check = (label: string, ok: boolean, detail = "") => {
    console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? ` — ${detail}` : ""}`);
    if (!ok) failures++;
  };

  console.log(`agent: base=${BASE} execution=${EXECUTION} telemetry=${TELEMETRY_ON ? "on" : "off"}\n`);

  const batch: TelemetryItem[] = [];
  const perScenario: { s: Scenario; rec: RecommendResponse }[] = [];

  for (const s of scenarios) {
    const rec = await recommend(s.workload);
    const { used, attempts } = await executeChain(rec, s.simulate, batch);
    console.log(
      `scenario: ${s.name}\n  rec=${rec.recommendation_id} attempts=${attempts} settled_on=${used ?? "(exhausted)"}`,
    );
    perScenario.push({ s, rec });
  }

  // one garbage item to prove per-item best-effort drop counting
  batch.push({
    recommendation_id: "rec_doesnotexist",
    model_used: "nope",
    status: 200,
  });

  const expectedAccepted = batch.length - 1;
  const flushed = await flushTelemetry(batch);
  console.log(`\ntelemetry flush: ${JSON.stringify(flushed)}`);
  if (TELEMETRY_ON) {
    check("batch accepted count", flushed?.accepted === expectedAccepted, `want ${expectedAccepted}, got ${flushed?.accepted}`);
    check("garbage item dropped", flushed?.dropped === 1, `got ${flushed?.dropped}`);
  }

  // verify the engine derived exactly the outcomes each scenario predicts
  if (TELEMETRY_ON) {
    for (const { s, rec } of perScenario) {
      const res = await api(
        `/v1/history/outcomes?recommendation_id=${rec.recommendation_id}`,
      );
      const body = (await res.json()) as {
        outcomes: { outcome: string; endpoint_used: string }[];
      };
      const got = body.outcomes.map((o) => o.outcome).sort();
      const want = [...s.expectOutcomes].sort();
      check(
        `derived outcomes [${s.name}]`,
        JSON.stringify(got) === JSON.stringify(want),
        `want ${want}, got ${got}`,
      );
      const wantEndpoint = EXECUTION === "openrouter" ? "openrouter" : "direct";
      check(
        `endpoint_used=${wantEndpoint} [${s.name}]`,
        body.outcomes.every((o) => o.endpoint_used === wantEndpoint),
      );
    }
  }

  // the ceremony is dead: POST /v1/outcomes must 404
  const gone = await api("/v1/outcomes", { method: "POST", body: "{}" });
  check("POST /v1/outcomes returns 404", gone.status === 404, `got ${gone.status}`);

  console.log(`\n${failures === 0 ? "ALL CHECKS PASSED" : `${failures} CHECK(S) FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((err) => {
  console.error("agent crashed:", err);
  process.exit(1);
});
