#!/usr/bin/env bash
# ============================================================================
# DEMO / EXPLORATION ONLY — NOT AN EVAL, NOT A CORRECTNESS SIGNAL.
#
# This fabricates `aggregates` rows so a keyless local stack produces non-null,
# differentiated recommendations you can click around by hand. The numbers are
# invented, and the small|mid|flagship classes below are a labeling convenience
# that DO NOT EXIST IN THE ENGINE (it ranks purely by measured quality/cost/
# latency). A recommendation produced against this data tells you the ranking
# code ran — nothing about how models actually perform. Do not read routing
# quality off this. Real routing needs real benchmark measurements.
#
# It is intentionally OUTSIDE evals/ so it can never be mistaken for a signal.
# Wipes and repopulates `aggregates`. Requires the `maverick-pg` container.
#
#   ./scripts/demo/seed_synthetic_matrix.sh
# ============================================================================
set -euo pipefail
PGC=(docker exec -i maverick-pg psql -U maverick -d maverick)

CAPS=(structured_extraction instruction_schema_following factual_qa
      multi_step_reasoning long_context_qa code_generation
      summarization_synthesis translation_multilingual rewriting_editing
      critique_and_judging)

# fabricated per-tier (1..5) values by a made-up class (bash 3.2: functions).
q_of()    { case "$1" in flagship) echo "0.930 0.920 0.905 0.900 0.895";; mid) echo "0.920 0.905 0.900 0.820 0.740";; *) echo "0.915 0.890 0.780 0.630 0.500";; esac; }
pr_of()   { case "$1" in flagship) echo "0.97 0.96 0.95 0.93 0.90";; mid) echo "0.96 0.94 0.90 0.82 0.72";; *) echo "0.95 0.90 0.78 0.62 0.50";; esac; }
cost_of() { case "$1" in flagship) echo 0.0090;; mid) echo 0.0030;; *) echo 0.0006;; esac; }
lat_of()  { case "$1" in flagship) echo 2400;; mid) echo 1400;; *) echo 700;; esac; }
# made-up class assignment — again, NOT an engine concept.
class_of() { case "$1" in
  claude-opus-4-1|gpt-5.6-sol|grok-4)                     echo flagship ;;
  claude-sonnet-4-6|gpt-5.5|gpt-5.6-terra|gemini-3.1-pro) echo mid ;;
  *)                                                      echo small ;;
esac; }

COLS=()
while IFS= read -r line; do [ -n "$line" ] && COLS+=("$line"); done < <("${PGC[@]}" -At -c "select id||'|'||model_id from model_columns where status <> 'retired'")
TODAY=$("${PGC[@]}" -At -c "select current_date")

rows=""
for entry in "${COLS[@]}"; do
  id="${entry%%|*}"; model="${entry##*|}"; cls=$(class_of "$model")
  read -ra qv <<<"$(q_of "$cls")"; read -ra pv <<<"$(pr_of "$cls")"
  cost=$(cost_of "$cls"); lat=$(lat_of "$cls")
  for cap in "${CAPS[@]}"; do
    for t in 1 2 3 4 5; do i=$((t-1))
      rows+="('$id','$cap',$t,'quality',${qv[$i]},0.001,10,true,'2026-07-07','$TODAY'),"
      rows+="('$id','$cap',$t,'pass_rate',${pv[$i]},0.001,10,true,'2026-07-07','$TODAY'),"
      rows+="('$id','$cap',$t,'cost_per_run_usd',${cost},0,10,true,'2026-07-07','$TODAY'),"
      rows+="('$id','$cap',$t,'latency_p95_ms',${lat},0,10,true,'2026-07-07','$TODAY'),"
    done
  done
done
rows="${rows%,}"

{
  echo "delete from aggregates;"
  echo "insert into aggregates (model_column_id,capability,difficulty_tier,dimension,ewma,variance,n,measured,matrix_version,as_of) values"
  echo "$rows;"
} | "${PGC[@]}" -q -v ON_ERROR_STOP=1
"${PGC[@]}" -At -c "select 'seeded '||count(*)||' FABRICATED aggregate rows (demo only, not a signal)' from aggregates"
