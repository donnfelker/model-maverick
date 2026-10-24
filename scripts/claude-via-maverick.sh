#!/usr/bin/env bash
# Launch Claude Code through the Model Maverick proxy.
# The default maverick/auto model requests ranking. Leaving Claude Code's own
# default model (without this launcher) sends that model directly, with no ranking.
#
# Put local secrets in .env.maverick.local next to the repo root. That file is
# gitignored. Example for hosted mode:
#
#   MAVERICK_PROXY_MODE=hosted
#   MAVERICK_PROXY_URL=https://proxy.example.test
#   MAVERICK_TENANT_KEY=mk_live_...
#   ANTHROPIC_API_KEY=sk-ant-...
#
# Check the resolved command without starting Claude Code:
#
#   ./scripts/claude-via-maverick.sh --dry-run -p "hello"
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
env_file="$repo_root/.env.maverick.local"

if [ -f "$env_file" ]; then
  set -a
  # shellcheck disable=SC1090
  . "$env_file"
  set +a
fi

dry_run="${MAVERICK_CLAUDE_DRY_RUN:-0}"
if [ "${1:-}" = "--dry-run" ]; then
  dry_run=1
  shift
fi

mode="${MAVERICK_PROXY_MODE:-self-hosted}"
proxy_url="${MAVERICK_PROXY_URL:-${PROXY_URL:-http://localhost:8090}}"
engine_url="${MAVERICK_ENGINE_URL:-${ENGINE_URL:-http://localhost:8080}}"
grafana_url="${MAVERICK_GRAFANA_URL:-${GRAFANA_URL:-http://localhost:3001}}"
otlp_endpoint="${MAVERICK_OTLP_ENDPOINT:-${OTLP_ENDPOINT:-http://127.0.0.1:4318}}"
main_model="${MAVERICK_CLAUDE_MODEL:-${MAIN_MODEL:-maverick/auto}}"
fast_model="${MAVERICK_CLAUDE_FAST_MODEL:-${FAST_MODEL:-maverick/auto:cost}}"
metric_interval_ms="${MAVERICK_OTEL_METRIC_INTERVAL_MS:-${METRIC_INTERVAL_MS:-10000}}"

log() {
  printf 'maverick claude: %s\n' "$*" >&2
}

die() {
  log "error: $*"
  exit 1
}

present() {
  if [ -n "${1:-}" ]; then
    printf 'present'
  else
    printf 'absent'
  fi
}

validate_alias() {
  case "$1" in
    maverick/*) ;;
    *) die "$2 must be a maverick/* alias, got \"$1\"" ;;
  esac
}

append_custom_header() {
  header="$1"
  if [ -n "${ANTHROPIC_CUSTOM_HEADERS:-}" ]; then
    ANTHROPIC_CUSTOM_HEADERS="${ANTHROPIC_CUSTOM_HEADERS}"$'\n'"${header}"
  else
    ANTHROPIC_CUSTOM_HEADERS="${header}"
  fi
  export ANTHROPIC_CUSTOM_HEADERS
}

case "$mode" in
  hosted|self-hosted) ;;
  *) die "MAVERICK_PROXY_MODE must be hosted or self-hosted, got \"$mode\"" ;;
esac
validate_alias "$main_model" "MAVERICK_CLAUDE_MODEL"
validate_alias "$fast_model" "MAVERICK_CLAUDE_FAST_MODEL"

if [ "$mode" = "hosted" ]; then
  [ -n "${MAVERICK_TENANT_KEY:-}" ] || die "MAVERICK_TENANT_KEY is required in hosted mode"
  case "$MAVERICK_TENANT_KEY" in
    mk_live_*) ;;
    *) die "MAVERICK_TENANT_KEY must start with mk_live_" ;;
  esac
  [ -n "${ANTHROPIC_API_KEY:-}" ] || die "ANTHROPIC_API_KEY is required in hosted mode for Claude Code's Messages requests"
  case "$ANTHROPIC_API_KEY" in
    sk-or-*) die "Claude Code uses the Messages surface. OpenRouter opt-in is for the chat-completions surface." ;;
  esac
  append_custom_header "X-Maverick-API-Key: ${MAVERICK_TENANT_KEY}"
fi

export ANTHROPIC_BASE_URL="$proxy_url"
export ANTHROPIC_MODEL="$main_model"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="$fast_model"
export ANTHROPIC_SMALL_FAST_MODEL="$fast_model"

export CLAUDE_CODE_ENABLE_TELEMETRY=1
export OTEL_METRICS_EXPORTER=otlp
export OTEL_LOGS_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT="$otlp_endpoint"
export OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=cumulative
export OTEL_METRIC_EXPORT_INTERVAL="$metric_interval_ms"
export OTEL_LOGS_EXPORT_INTERVAL=5000

log "mode=$mode"
log "proxy_url=$proxy_url"
log "model=$main_model fast_model=$fast_model"
log "tenant_header=$(if [ "$mode" = "hosted" ]; then printf 'present'; else printf 'not-used'; fi)"
log "anthropic_provider_key=$(present "${ANTHROPIC_API_KEY:-}")"
log "otel_endpoint=$otlp_endpoint"

if [ "$dry_run" = "1" ] || [ "$dry_run" = "true" ]; then
  log "dry_run=1"
  log "env ANTHROPIC_BASE_URL=$ANTHROPIC_BASE_URL"
  log "env ANTHROPIC_MODEL=$ANTHROPIC_MODEL"
  log "env ANTHROPIC_DEFAULT_HAIKU_MODEL=$ANTHROPIC_DEFAULT_HAIKU_MODEL"
  if [ "$mode" = "hosted" ]; then
    log "env ANTHROPIC_CUSTOM_HEADERS=<redacted X-Maverick-API-Key>"
  else
    log "env ANTHROPIC_CUSTOM_HEADERS=$(present "${ANTHROPIC_CUSTOM_HEADERS:-}")"
  fi
  log "command=claude <${#} arg(s)>"
  exit 0
fi

command -v claude >/dev/null || die "claude command not found"

fail=0
if ! curl -sf "$proxy_url/up" >/dev/null; then
  log "proxy DOWN at $proxy_url"
  fail=1
fi
if [ "$mode" = "self-hosted" ]; then
  if ! curl -sf "$engine_url/up" | grep -q '"ok"'; then
    log "engine DOWN or unavailable at $engine_url"
    fail=1
  fi
fi
if ! curl -sf -o /dev/null "$grafana_url/login"; then
  log "note: grafana not reachable at $grafana_url"
fi
[ "$fail" -eq 0 ] || exit 1

exec claude "$@"
