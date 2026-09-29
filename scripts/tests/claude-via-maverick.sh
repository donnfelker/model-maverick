#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
launcher="$repo_root/scripts/claude-via-maverick.sh"

contains() {
  text="$1"
  needle="$2"
  if ! grep -Fq "$needle" <<<"$text"; then
    printf 'missing expected text: %s\n' "$needle" >&2
    printf '%s\n' "$text" >&2
    exit 1
  fi
}

omits() {
  text="$1"
  needle="$2"
  if grep -Fq "$needle" <<<"$text"; then
    printf 'unexpected secret text: %s\n' "$needle" >&2
    printf '%s\n' "$text" >&2
    exit 1
  fi
}

hosted_tenant="mk_live_test_secret_123"
hosted_provider="sk-ant-test-secret-456"
hosted="$(
  MAVERICK_PROXY_MODE=hosted \
    MAVERICK_PROXY_URL=http://proxy.test \
    MAVERICK_TENANT_KEY="$hosted_tenant" \
    ANTHROPIC_API_KEY="$hosted_provider" \
    "$launcher" --dry-run -p "hidden prompt" 2>&1
)"

contains "$hosted" "mode=hosted"
contains "$hosted" "proxy_url=http://proxy.test"
contains "$hosted" "tenant_header=present"
contains "$hosted" "anthropic_provider_key=present"
contains "$hosted" "env ANTHROPIC_CUSTOM_HEADERS=<redacted X-Maverick-API-Key>"
contains "$hosted" "command=claude <2 arg(s)>"
omits "$hosted" "$hosted_tenant"
omits "$hosted" "$hosted_provider"
omits "$hosted" "hidden prompt"

self_hosted="$(
  MAVERICK_PROXY_MODE=self-hosted \
    MAVERICK_PROXY_URL=http://localhost:8090 \
    MAVERICK_ENGINE_URL=http://localhost:8080 \
    MAVERICK_CLAUDE_DRY_RUN=1 \
    "$launcher" 2>&1
)"

contains "$self_hosted" "mode=self-hosted"
contains "$self_hosted" "tenant_header=not-used"
contains "$self_hosted" "anthropic_provider_key=absent"
contains "$self_hosted" "command=claude <0 arg(s)>"

if MAVERICK_PROXY_MODE=hosted "$launcher" --dry-run >/tmp/maverick-launcher.out 2>&1; then
  printf 'hosted mode without MAVERICK_TENANT_KEY unexpectedly succeeded\n' >&2
  cat /tmp/maverick-launcher.out >&2
  exit 1
fi
contains "$(cat /tmp/maverick-launcher.out)" "MAVERICK_TENANT_KEY is required in hosted mode"
rm -f /tmp/maverick-launcher.out

if MAVERICK_PROXY_MODE=hosted \
  MAVERICK_TENANT_KEY="$hosted_tenant" \
  ANTHROPIC_API_KEY=sk-or-v1-test-secret \
  "$launcher" --dry-run >/tmp/maverick-launcher.out 2>&1; then
  printf 'hosted Claude OpenRouter mode unexpectedly succeeded\n' >&2
  cat /tmp/maverick-launcher.out >&2
  exit 1
fi
contains "$(cat /tmp/maverick-launcher.out)" "OpenRouter opt-in is for the chat-completions surface"
omits "$(cat /tmp/maverick-launcher.out)" "sk-or-v1-test-secret"
rm -f /tmp/maverick-launcher.out

printf 'claude-via-maverick dry-run tests passed\n'
