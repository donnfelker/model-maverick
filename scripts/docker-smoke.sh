#!/usr/bin/env bash
# The local container smoke for the deploy images (bead 9.4; docker/README.md).
#
# It builds both images from docker/Dockerfile. Then it runs Postgres 16, the engine
# (`maverick serve`), the hosted proxy, and a fake OpenAI upstream on one private Docker
# network, and checks:
#   1. `maverick migrate` in the image migrates an empty database;
#   2. GET /up on the public listener, and GET /health/detail on the internal listener
#      with an internal token;
#   3. one proxy request: the tenant key in X-Maverick-API-Key, the caller's own OpenAI key,
#      the engine's recommendation, the call to the fake upstream, and the relayed reply;
#   4. scripts/backup.sh and scripts/restore-test.sh against that database, in
#      postgres:16-alpine (the client tools match the server).
#
# No real provider is called: every proxy upstream is the fake or a closed port, and the
# engine has no provider key. Every published port binds 127.0.0.1 only. Docker-published
# ports bypass the host firewall, so a bare `-p 8080:8080` would be public.
#
# On failure it prints every container's logs. The containers, the network, and the work
# dir are removed on exit.
#
#   SMOKE_SKIP_BUILD=1 scripts/docker-smoke.sh   reuses maverick:smoke and maverick-proxy:smoke
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PREFIX="maverick-smoke-$$"
NET="$PREFIX"
WORK="$(mktemp -d)"
ENGINE_IMAGE="maverick:smoke"
PROXY_IMAGE="maverick-proxy:smoke"
CONTAINERS=()

MASTER_KEY="c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4"
INTERNAL_TOKEN="ops:smoke-internal-token"
DB_URL="postgres://maverick:maverick@${PREFIX}-pg:5432/maverick"
CLOSED="http://127.0.0.1:1"

step=0
log() {
    step=$((step + 1))
    printf '==> [%d] %s\n' "$step" "$*"
}

dump_logs() {
    for name in "${CONTAINERS[@]}"; do
        echo "---- logs: $name"
        docker logs "$name" 2>&1 | tail -n 60 || true
    done
}

cleanup() {
    local status=$?
    if [[ $status -ne 0 ]]; then
        echo "SMOKE FAILED (exit $status) at step $step"
        dump_logs
    fi
    for name in "${CONTAINERS[@]}"; do
        docker rm -f "$name" >/dev/null 2>&1 || true
    done
    docker network rm "$NET" >/dev/null 2>&1 || true
    rm -rf "$WORK"
    exit "$status"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# run_bg <name> <docker run args...>
run_bg() {
    local name="$1"
    shift
    CONTAINERS+=("$name")
    docker run -d --name "$name" --network "$NET" "$@" >/dev/null
}

# The host port that 127.0.0.1 publishes for a container port.
host_port() {
    docker port "$1" "$2/tcp" | grep '^127\.0\.0\.1:' | head -n 1 | sed 's/.*://'
}

# wait_http <url> [curl args...]: up to 60 s for a 200.
wait_http() {
    local url="$1"
    shift
    for _ in $(seq 1 60); do
        if [[ "$(curl -s -o /dev/null -w '%{http_code}' "$@" "$url" || true)" == 200 ]]; then
            return 0
        fi
        sleep 1
    done
    fail "no 200 from $url"
}

if [[ "${SMOKE_SKIP_BUILD:-}" != 1 ]]; then
    log "build $ENGINE_IMAGE and $PROXY_IMAGE"
    docker build -f docker/Dockerfile --target engine -t "$ENGINE_IMAGE" .
    docker build -f docker/Dockerfile --target proxy -t "$PROXY_IMAGE" .
fi
for image in "$ENGINE_IMAGE" "$PROXY_IMAGE"; do
    # distroless/static: no shell, and the nonroot user.
    user="$(docker image inspect -f '{{.Config.User}}' "$image")"
    [[ "$user" == 65532 || "$user" == nonroot* ]] || fail "$image runs as '$user', not nonroot"
done

log "network and Postgres 16"
docker network create "$NET" >/dev/null
run_bg "${PREFIX}-pg" -e POSTGRES_USER=maverick -e POSTGRES_PASSWORD=maverick \
    -e POSTGRES_DB=maverick postgres:16-alpine
for _ in $(seq 1 60); do
    docker exec "${PREFIX}-pg" pg_isready -U maverick -h 127.0.0.1 >/dev/null 2>&1 && break
    sleep 1
done
docker exec "${PREFIX}-pg" pg_isready -U maverick -h 127.0.0.1 >/dev/null || fail "Postgres is not ready"

ENGINE_ENV=(
    -e "MAVERICK_DATABASE_URL=$DB_URL"
    -e "MAVERICK_INTERNAL_TOKENS=$INTERNAL_TOKEN"
    -e "MAVERICK_CAPTURE_MASTER_KEY=$MASTER_KEY"
)

log "maverick migrate (the image) on the empty database"
docker run --rm --network "$NET" "${ENGINE_ENV[@]}" "$ENGINE_IMAGE" migrate

log "seed a tenant, its key, a column, its pool, the corpus version, and aggregates"
KEY="mk_live_$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 43 || true)"
KEY_HASH="$(printf '%s' "$KEY" | sha256sum | cut -d' ' -f1)"
docker exec -i "${PREFIX}-pg" psql -X -q -v ON_ERROR_STOP=1 -U maverick -d maverick <<SQL
insert into corpus_versions (version, description, task_count, active)
values ('2026-07-01', 'smoke', 0, true);
insert into model_columns (id, provider, model_id, endpoint, settings, status, pricing)
values ('col_smoke_gpt', 'openai', 'gpt-5', 'direct', '{}', 'active',
        '{"input_per_mtok": 1.25, "output_per_mtok": 10.0, "currency": "USD", "as_of": "2026-07-01", "source": "list"}');
insert into tenants (id, name, prompt_hmac_key)
values ('ten_smoke', 'Smoke', decode(md5('smoke-a') || md5('smoke-b'), 'hex'));
insert into api_keys (id, tenant_id, key_hash, last4)
values ('key_smoke', 'ten_smoke', decode('$KEY_HASH', 'hex'), '${KEY: -4}');
insert into pools (id, tenant_id, name, model_column_ids, is_default)
values ('pool_smoke', 'ten_smoke', 'default', '{col_smoke_gpt}', true);
insert into aggregates (model_column_id, capability, difficulty_tier, dimension, ewma,
                        variance, n, measured, matrix_version, as_of)
select 'col_smoke_gpt', capability, tier, 'pass_rate', 0.95, 0, 20, true, '2026-07-01', current_date
from unnest(array['structured_extraction', 'multi_step_reasoning']) as capability,
     generate_series(1, 5) as tier;
insert into aggregates (model_column_id, capability, difficulty_tier, dimension, ewma,
                        variance, n, measured, matrix_version, as_of)
select 'col_smoke_gpt', capability, 3, dimension, value, 0, 20, true, '2026-07-01', current_date
from unnest(array['structured_extraction', 'multi_step_reasoning']) as capability,
     (values ('quality', 0.9), ('cost_per_run_usd', 0.01), ('latency_p95_ms', 900.0))
       as cell(dimension, value);
SQL

log "the engine: maverick serve"
run_bg "${PREFIX}-engine" "${ENGINE_ENV[@]}" -p 127.0.0.1::8080 -p 127.0.0.1::8081 \
    "$ENGINE_IMAGE" serve
ENGINE_PORT="$(host_port "${PREFIX}-engine" 8080)"
INTERNAL_PORT="$(host_port "${PREFIX}-engine" 8081)"
wait_http "http://127.0.0.1:${ENGINE_PORT}/up"
[[ "$(curl -s "http://127.0.0.1:${ENGINE_PORT}/up")" == '{"status":"ok"}' ]] || fail "/up body"
wait_http "http://127.0.0.1:${INTERNAL_PORT}/health/detail" \
    -H "Authorization: Bearer $INTERNAL_TOKEN"
echo "health/detail: $(curl -s -H "Authorization: Bearer $INTERNAL_TOKEN" \
    "http://127.0.0.1:${INTERNAL_PORT}/health/detail")"
[[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${INTERNAL_PORT}/health/detail")" == 401 ]] \
    || fail "/health/detail without a token must be 401"

log "the fake OpenAI upstream and the hosted proxy"
run_bg "${PREFIX}-upstream" python:3.12-alpine python -u -c '
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

class Upstream(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", "0"))) or b"{}")
        print(json.dumps({"path": self.path, "model": body.get("model"),
                          "authorization": self.headers.get("authorization"),
                          "maverick_key": self.headers.get("x-maverick-api-key")}), flush=True)
        reply = json.dumps({"id": "chatcmpl-smoke", "object": "chat.completion",
                            "model": body.get("model"),
                            "choices": [{"index": 0, "finish_reason": "stop",
                                         "message": {"role": "assistant", "content": "smoke reply"}}],
                            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def log_message(self, *args):
        pass

HTTPServer(("0.0.0.0", 8000), Upstream).serve_forever()
'
run_bg "${PREFIX}-proxy" -p 127.0.0.1::8090 \
    -e MAVERICK_PROXY_MODE=hosted \
    -e MAVERICK_PROXY_ADDR=:8090 \
    -e "MAVERICK_ENGINE_URL=http://${PREFIX}-engine:8080" \
    -e "MAVERICK_UPSTREAM_OPENAI=http://${PREFIX}-upstream:8000" \
    -e "MAVERICK_UPSTREAM_ANTHROPIC=$CLOSED" \
    -e "MAVERICK_UPSTREAM_GOOGLE=$CLOSED" \
    -e "MAVERICK_UPSTREAM_XAI=$CLOSED" \
    -e "MAVERICK_UPSTREAM_OPENROUTER=$CLOSED" \
    "$PROXY_IMAGE"
PROXY_PORT="$(host_port "${PREFIX}-proxy" 8090)"
wait_http "http://127.0.0.1:${PROXY_PORT}/up"

log "one proxy request through the engine to the fake upstream"
reply="$(curl -s -w '\n%{http_code}' "http://127.0.0.1:${PROXY_PORT}/v1/chat/completions" \
    -H "X-Maverick-API-Key: $KEY" \
    -H "Authorization: Bearer sk-smoke-caller" \
    -H "Content-Type: application/json" \
    -d '{"model": "maverick/auto", "messages": [{"role": "user", "content": "Say hello."}]}')"
status="${reply##*$'\n'}"
body="${reply%$'\n'*}"
echo "proxy: $status $body"
[[ "$status" == 200 ]] || fail "the proxy answered $status"
[[ "$body" == *'"smoke reply"'* ]] || fail "the proxy did not relay the upstream reply"
seen="$(docker logs "${PREFIX}-upstream" 2>&1 | grep '"path"' || true)"
echo "upstream saw: $seen"
[[ "$(printf '%s\n' "$seen" | grep -c '"path"')" == 1 ]] || fail "the upstream saw $(printf '%s\n' "$seen" | grep -c '"path"') requests"
[[ "$seen" == *'"model": "gpt-5"'* ]] || fail "the upstream model is not the recommended gpt-5"
[[ "$seen" == *'"authorization": "Bearer sk-smoke-caller"'* ]] || fail "the caller key did not reach the upstream"
[[ "$seen" == *'"maverick_key": null'* ]] || fail "X-Maverick-API-Key reached the upstream (D30)"

log "scripts/backup.sh and scripts/restore-test.sh in postgres:16-alpine"
mkdir -p "$WORK/backups"
CONTAINERS+=("${PREFIX}-bin")
docker create --name "${PREFIX}-bin" "$ENGINE_IMAGE" >/dev/null
docker cp "${PREFIX}-bin:/usr/local/bin/maverick" "$WORK/maverick"
docker run --rm --network "$NET" --user "$(id -u):$(id -g)" \
    -v "$ROOT/scripts:/scripts:ro" -v "$WORK/backups:/backups" \
    -e "MAVERICK_DATABASE_URL=$DB_URL" -e MAVERICK_BACKUP_DIR=/backups \
    postgres:16-alpine sh /scripts/backup.sh | tee "$WORK/backup.log"
grep -q '"backup target unset' "$WORK/backup.log" || fail "no warning for the unset backup target"
ls "$WORK"/backups/maverick-*.dump >/dev/null || fail "no dump written"
grep -q '^tenants 1$' "$WORK"/backups/maverick-*.counts || fail "the counts miss tenants 1"
docker run --rm --network "$NET" --user "$(id -u):$(id -g)" \
    -v "$ROOT/scripts:/scripts:ro" -v "$WORK/backups:/backups" \
    -v "$WORK/maverick:/usr/local/bin/maverick:ro" \
    "${ENGINE_ENV[@]}" -e MAVERICK_BACKUP_DIR=/backups \
    postgres:16-alpine sh /scripts/restore-test.sh | tee "$WORK/restore.log"
grep -q '"restore test passed"' "$WORK/restore.log" || fail "the restore test did not pass"

log "backup retention: MAVERICK_BACKUP_KEEP=2 keeps the newest two"
for _ in 1 2; do
    sleep 1 # the dump names have one-second resolution
    docker run --rm --network "$NET" --user "$(id -u):$(id -g)" \
        -v "$ROOT/scripts:/scripts:ro" -v "$WORK/backups:/backups" \
        -e "MAVERICK_DATABASE_URL=$DB_URL" -e MAVERICK_BACKUP_DIR=/backups \
        -e MAVERICK_BACKUP_KEEP=2 postgres:16-alpine sh /scripts/backup.sh >/dev/null
done
dumps="$(find "$WORK/backups" -name 'maverick-*.dump' | wc -l)"
counts="$(find "$WORK/backups" -name 'maverick-*.counts' | wc -l)"
[[ "$dumps" == 2 && "$counts" == 2 ]] || fail "retention kept $dumps dumps and $counts counts files"

echo "SMOKE PASSED"
