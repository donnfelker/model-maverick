#!/usr/bin/env bash
# bead 4.8: run the one opt-in, spend-capped live smoke test with real provider keys.
#
# It is the only test that calls real providers, is spend-capped at $1.00, and is skipped by
# default. Set the keys for the providers in use before running:
#   MAVERICK_ANTHROPIC_API_KEY, MAVERICK_OPENAI_API_KEY, MAVERICK_GOOGLE_API_KEY, MAVERICK_XAI_API_KEY
# Override the two columns with MAVERICK_LIVE_SMOKE_COLUMNS (two comma-separated
# provider:model_id:input_per_mtok:output_per_mtok entries).
set -euo pipefail
export MAVERICK_LIVE_SMOKE=1
exec cargo test -p maverick-runner --test live_smoke -- --ignored --exact live_smoke --nocapture "$@"
