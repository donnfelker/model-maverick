#!/usr/bin/env bash
# Launch Claude Code routed through the local ModelMaverick proxy, with OTel
# going to the local Grafana stack. Edit the knobs below; put your real key in
# .env.maverick.local (gitignored) next to the repo root:
#
#   echo 'ANTHROPIC_API_KEY=sk-ant-...' > .env.maverick.local
#   ./scripts/claude-via-maverick.sh            # interactive session
#   ./scripts/claude-via-maverick.sh -p "hi"    # headless one-shot
set -euo pipefail

# ---- knobs -----------------------------------------------------------------
PROXY_URL="${PROXY_URL:-http://localhost:8090}"
ENGINE_URL="${ENGINE_URL:-http://localhost:8080}"
GRAFANA_URL="${GRAFANA_URL:-http://localhost:3001}"
OTLP_ENDPOINT="${OTLP_ENDPOINT:-http://127.0.0.1:4318}"  # http/protobuf; 127.0.0.1 avoids IPv6-localhost ambiguity
MAIN_MODEL="maverick/auto"        # maverick/auto | maverick/quality | maverick/cost
FAST_MODEL="maverick/cost"        # Claude Code's background/small-model calls
METRIC_INTERVAL_MS=10000          # OTel export cadence (default 60000)
# -----------------------------------------------------------------------------

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
env_file="$repo_root/.env.maverick.local"
[ -f "$env_file" ] && set -a && . "$env_file" && set +a

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "ANTHROPIC_API_KEY is not set." >&2
  echo "Put it in $env_file (gitignored):" >&2
  echo "  echo 'ANTHROPIC_API_KEY=sk-ant-...' > $env_file" >&2
  exit 1
fi

# ---- preflight ---------------------------------------------------------------
fail=0
curl -sf "$PROXY_URL/up" >/dev/null || { echo "proxy DOWN at $PROXY_URL (start the maverick-proxy binary)"; fail=1; }
curl -sf "$ENGINE_URL/up" | grep -q '"ok"' || { echo "engine DOWN or unavailable at $ENGINE_URL (see docs/onboarding.md)"; fail=1; }
curl -sf -o /dev/null "$GRAFANA_URL/login" || echo "note: grafana not reachable at $GRAFANA_URL — routing still works, OTel just has nowhere to land"
[ "$fail" -eq 1 ] && exit 1
echo "maverick routing: $PROXY_URL  model=$MAIN_MODEL  fast=$FAST_MODEL"
echo "otel: $OTLP_ENDPOINT  dashboards: $GRAFANA_URL (admin/admin)"

# ---- claude code env ----------------------------------------------------------
export ANTHROPIC_BASE_URL="$PROXY_URL"
export ANTHROPIC_MODEL="$MAIN_MODEL"
export ANTHROPIC_SMALL_FAST_MODEL="$FAST_MODEL"

export CLAUDE_CODE_ENABLE_TELEMETRY=1
export OTEL_METRICS_EXPORTER=otlp
export OTEL_LOGS_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT="$OTLP_ENDPOINT"
# Prometheus drops delta-temporality metrics silently — cumulative is required
export OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=cumulative
export OTEL_METRIC_EXPORT_INTERVAL="$METRIC_INTERVAL_MS"
export OTEL_LOGS_EXPORT_INTERVAL=5000

exec claude "$@"
