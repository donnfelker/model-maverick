#!/bin/sh
# An SSH tunnel to the engine's internal listener on the deploy host (bead 9.13). POSIX sh.
#
#   scripts/internal-tunnel.sh <user@host> [local port]
#
# The internal listener (:8081: /internal/v1/*, /health/detail) has no kamal-proxy route, and
# no container publishes it on the host. Kamal starts a new web container before it stops the
# old one, and `kamal app exec` copies the role's options, so a fixed host port would clash on
# every deploy. This script asks the host for the running web container's address on the
# `kamal` Docker network (Kamal labels it service=<service>, role=web), then forwards
# 127.0.0.1:<local port> (default 8081) on this machine to <address>:8081:
#
#   scripts/internal-tunnel.sh deploy@<HOST>
#   curl -s http://127.0.0.1:8081/health/detail -H "Authorization: Bearer <label>:<secret>"
#
# A deploy replaces the container and its address: run the script again after each deploy.
# The SSH user must be able to run docker on the host, as Kamal's user can.
#
# Environment:
#   MAVERICK_SERVICE   default maverick (the `service:` in config/deploy.yml)
set -eu

usage() {
    echo "usage: scripts/internal-tunnel.sh <user@host> [local port]" >&2
    exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
target="$1"
port="${2:-8081}"
service="${MAVERICK_SERVICE:-maverick}"
case "$target" in '' | -*) usage ;; esac
case "$port" in '' | *[!0-9]*) usage ;; esac
case "$service" in
    '' | *[!a-z0-9_-]*)
        echo "MAVERICK_SERVICE must match [a-z0-9_-]+" >&2
        exit 2
        ;;
esac

# Run by the host's shell: the newest running web container's address on the kamal network.
remote="id=\$(docker ps --quiet --filter label=service=$service --filter label=role=web --filter status=running | head -n 1)"
remote="$remote && [ -n \"\$id\" ] && docker inspect --format '{{with index .NetworkSettings.Networks \"kamal\"}}{{.IPAddress}}{{end}}' \"\$id\""

# shellcheck disable=SC2029 # $service expands here on purpose (validated above); $id is escaped.
if ! address=$(ssh "$target" "$remote"); then
    echo "no running $service web container on $target" >&2
    exit 1
fi
case "$address" in
    '' | *[!0-9.]*)
        echo "the $service web container on $target has no address on the kamal network" >&2
        exit 1
        ;;
esac

echo "127.0.0.1:$port -> $service web $address:8081 on $target (Ctrl-C closes the tunnel)" >&2
exec ssh -N -L "127.0.0.1:$port:$address:8081" "$target"
