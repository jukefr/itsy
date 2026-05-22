#!/usr/bin/env bash
# itsy-bench-net — bring up rootless docker + a socat host-loopback hop
# so harbor task containers can reach the host's llama-server at
# `10.0.2.2:8000`. Idempotent; safe to invoke before every harbor run.
#
# Background:
#   In rootless docker on this sandbox the daemon ships with
#   `DOCKERD_ROOTLESS_ROOTLESSKIT_DISABLE_HOST_LOOPBACK=true`. That
#   makes `10.0.2.2` unreachable to bridge-attached containers
#   (slirp4netns refuses to forward to the host's loopback).
#
#   Fix: restart the daemon with
#   `DOCKERD_ROOTLESS_ROOTLESSKIT_DISABLE_HOST_LOOPBACK=false`, then run
#   a local socat that listens on this container's 0.0.0.0:LLM_PORT
#   and forwards to LLM_HOST_FROM_SANDBOX:LLM_PORT.
#
# Used by the `terminal-bench-2` skill.

set -uo pipefail

LLM_PORT="${LLM_PORT:-8000}"
LLM_HOST_FROM_SANDBOX="${LLM_HOST_FROM_SANDBOX:-10.0.2.2}"
LOGDIR="${LOGDIR:-/home/node/logs}"
DOCKERD_LOG="$LOGDIR/dockerd-bench-net.log"
SOCAT_LOG="${SOCAT_LOG:-/tmp/itsy-bench-socat.log}"

say() { printf '[itsy-bench-net] %s\n' "$*"; }
die() { say "FAILED — $*" >&2; exit 1; }

# True if a child container can fetch /v1/models through the chain.
end_to_end_ok() {
    [ -S /run/user/1000/docker.sock ] || return 1
    docker version >/dev/null 2>&1   || return 1
    local out
    out=$(timeout 10 docker run --rm alpine sh -c \
        "wget -qO- --timeout=4 http://${LLM_HOST_FROM_SANDBOX}:${LLM_PORT}/v1/models 2>/dev/null | head -c 30" \
        2>/dev/null || true)
    [[ "$out" == *'"data"'* ]]
}

restart_dockerd_with_host_loopback() {
    say "restarting rootless docker daemon with host loopback enabled"
    pkill -9 -f dockerd       2>/dev/null || true
    pkill -9 -f rootlesskit   2>/dev/null || true
    pkill -9 -f slirp4netns   2>/dev/null || true
    sleep 2
    rm -f /run/user/1000/docker.sock 2>/dev/null || true

    mkdir -p "$LOGDIR"
    # Ensure log file exists and is writable by `node`.
    touch "$DOCKERD_LOG" && chown node:node "$DOCKERD_LOG" 2>/dev/null || true
    : > "$DOCKERD_LOG"

    # Spawn daemon detached from our shell so this script can return.
    sudo -u node -i bash <<EOF &
export XDG_RUNTIME_DIR=/run/user/1000
export DOCKERD_ROOTLESS_ROOTLESSKIT_DISABLE_HOST_LOOPBACK=false
nohup dockerd-rootless.sh --storage-driver=fuse-overlayfs \
    >>$DOCKERD_LOG 2>&1 </dev/null &
disown
EOF
    wait

    # Wait for the socket + responsive daemon.
    for _ in $(seq 1 30); do
        if [ -S /run/user/1000/docker.sock ] && docker version >/dev/null 2>&1; then
            say "daemon is up"
            return 0
        fi
        sleep 0.5
    done
    say "daemon never became responsive. Tail of $DOCKERD_LOG:"
    tail -20 "$DOCKERD_LOG" >&2
    return 1
}

ensure_socat() {
    if pgrep -f "socat TCP-LISTEN:${LLM_PORT}," >/dev/null 2>&1; then
        say "socat already listening on :${LLM_PORT}"
        return 0
    fi
    command -v socat >/dev/null 2>&1 || apt-get install -y socat >/dev/null 2>&1 \
        || die "socat install"
    say "starting socat: 0.0.0.0:${LLM_PORT} → ${LLM_HOST_FROM_SANDBOX}:${LLM_PORT}"
    nohup socat "TCP-LISTEN:${LLM_PORT},fork,reuseaddr" \
                "TCP:${LLM_HOST_FROM_SANDBOX}:${LLM_PORT}" \
                >>"$SOCAT_LOG" 2>&1 &
    disown
    sleep 1
    ss -tln 2>/dev/null | grep -q ":${LLM_PORT} " || die "socat didn't bind :${LLM_PORT}"
}

# ── Fast path: everything already works ─────────────────────────────────
if end_to_end_ok; then
    say "already healthy — daemon up, socat up, child can reach LLM"
    exit 0
fi

# ── Slow path: bring it up ─────────────────────────────────────────────
restart_dockerd_with_host_loopback || exit 1
ensure_socat                       || exit 1
if end_to_end_ok; then
    say "ready"
    exit 0
fi
die "still couldn't reach LLM through a child container after restart"
