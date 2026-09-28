#!/usr/bin/env bash
# run_eval.sh — CLASSIFICATION eval for the prompt path (spec §8), plus a few
# engine-truth routing invariants. Requires a running `maverick serve`, a
# tenant API key (MK), and jq.
#
# What is SCORED (trustworthy — real engine output from a real classifier):
#   • capability  — the classifier's capability vs the labeled one
#   • band        — the classifier's difficulty (1-5) falling in the labeled
#                   band (easy 1-2 / medium 3 / hard 4-5)
#
# Engine INVARIANTS (data-agnostic, read from the response + persisted trace —
# they must hold for ANY data, so a pass means the engine behaved consistently,
# NOT that it routed "well"):
#   • constraints_met is true on the returned recommendation
#   • the recommendation persisted and its trace agrees on the pick (audit)
#
# What is deliberately NOT scored: which MODEL got picked. The engine has no
# small/mid/flagship notion; it ranks by measured quality/cost/latency. On a
# stack without real measured benchmarks a model pick proves nothing, so it is
# shown as informational only. Ranking LOGIC is covered by rank_test.go /
# ceiling_test.go; real routing QUALITY needs real benchmark data.
#
#   MK=mk_live_... ./evals/run_eval.sh
set -euo pipefail

BASE="${BASE:-http://localhost:8080}"
MK="${MK:?set MK=mk_live_... (a tenant API key)}"
EVAL="${EVAL:-$(dirname "$0")/classification_eval.json}"

in_band() { local d="$1" b="$2"; case "$b" in
    easy) [ "$d" -ge 1 ] && [ "$d" -le 2 ] ;;
    medium) [ "$d" -eq 3 ] ;;
    hard) [ "$d" -ge 4 ] && [ "$d" -le 5 ] ;; esac; }

n=0; cap_ok=0; band_ok=0; inv_ok=0; fallback=0
printf "%-15s │ %-22s │ %-26s │ %-18s │ %-3s │ %s\n" \
  "case" "expect cap/band" "classified cap/diff/method" "pick (info only)" "cls" "inv"
printf '%s\n' "────────────────┼────────────────────────┼────────────────────────────┼────────────────────┼─────┼────"

while IFS= read -r c; do
  n=$((n+1))
  prompt=$(jq -r '.prompt' <<<"$c")
  ecap=$(jq -r '.capability' <<<"$c")
  eband=$(jq -r '.band' <<<"$c")
  accept=$(jq -r '(.accept_capabilities // [.capability]) | join(",")' <<<"$c")

  body=$(jq -nc --arg p "$prompt" '{workload:{prompt:$p}}')
  resp=$(curl -s -X POST "$BASE/v1/recommend" -H "Authorization: Bearer $MK" -H 'Content-Type: application/json' -d "$body")

  gcap=$(jq -r '.classification.capability // "?"' <<<"$resp")
  gdiff=$(jq -r '.classification.difficulty // 0' <<<"$resp")
  gmethod=$(jq -r '.classification.method // "?"' <<<"$resp")
  pick=$(jq -r '.recommendations[0].model // "?"' <<<"$resp")
  cmet=$(jq -r '.constraints_met // false' <<<"$resp")
  recid=$(jq -r '.recommendation_id // ""' <<<"$resp")
  [ "$gmethod" = "fallback_default" ] && fallback=$((fallback+1))

  # scored: classification
  capm=✗; case ",$accept," in *",$gcap,"*) capm=✓; cap_ok=$((cap_ok+1));; esac
  bandm=✗; if in_band "$gdiff" "$eband"; then bandm=✓; band_ok=$((band_ok+1)); fi
  clsm=✗; [ "$capm" = ✓ ] && [ "$bandm" = ✓ ] && clsm=✓

  # engine invariants: constraints_met, and the persisted trace agrees on the pick
  tracepick=""
  [ -n "$recid" ] && tracepick=$(curl -s "$BASE/v1/history/recommendations/$recid" -H "Authorization: Bearer $MK" \
      | jq -r '.response.recommendations[0].model // ""')
  invm=✗
  if [ "$cmet" = true ] && [ -n "$tracepick" ] && [ "$tracepick" = "$pick" ]; then invm=✓; inv_ok=$((inv_ok+1)); fi

  printf "%-15s │ %-22s │ %-26s │ %-18s │  %s  │ %s\n" \
    "$(jq -r '.id' <<<"$c")" "${ecap}/${eband}" "${gcap}/${gdiff}/${gmethod}" "$pick" "$clsm" "$invm"
done < <(jq -c '.cases[]' "$EVAL")

echo
echo "SCORED   capability: $cap_ok/$n   band: $band_ok/$n"
echo "INVARIANT constraints_met + trace-consistency: $inv_ok/$n"
[ "$fallback" -gt 0 ] && echo "note: $fallback/$n used fallback_default — classifier unavailable (set MAVERICK_ANTHROPIC_API_KEY)."
echo "pick column is INFORMATIONAL — routing quality is not measurable without real benchmark data."
