#!/bin/sh
# Build (if needed) and start the GUNBATTE ROYALE ladder server.
#
# Usage:
#   ./start-server.sh                    # start on port 8321
#   PORT=9000 ./start-server.sh          # custom port
#   ./start-server.sh --house-bots 0     # extra args go to `gunbatte-server serve`
#
# Re-running restarts: the build runs first (a failed build leaves the old
# server up), then any previous instance listening on the port is stopped
# (SIGTERM, then SIGKILL after 10s), then the new one starts — so a stale
# server no longer makes the new one die with "address already in use".
#
# The VPS runs the server under systemd; deploys go through the shared
# `vps-deploy gunbatte` helper rather than this script.
#
# Endpoints once up:
#   http://127.0.0.1:$PORT/          viewer (watch replays / PLAY LIVE)
#   http://127.0.0.1:$PORT/ladder    standings + match history
set -eu
cd "$(dirname "$0")"

PORT="${PORT:-8321}"
# An explicit --port in "$@" wins over $PORT: gunbatte-server reads the flag, not the
# environment, and the stop step has to target the socket we actually bind.
port_in_args=""
prev=""
for arg in "$@"; do
    if [ "$prev" = "--port" ]; then
        PORT="$arg"
        port_in_args=1
    fi
    case "$arg" in
        --port=*)
            PORT="${arg#--port=}"
            port_in_args=1
            ;;
    esac
    prev="$arg"
done

# PIDs listening on TCP port $1; empty when the port is free.
port_listeners() {
    if command -v lsof >/dev/null 2>&1; then
        lsof -t -i "tcp:$1" -sTCP:LISTEN 2>/dev/null || true
    elif command -v ss >/dev/null 2>&1; then
        ss -ltnpH 2>/dev/null | awk -v want=":$1" '
            $4 ~ want "$" {
                if (match($0, /pid=[0-9]+/)) print substr($0, RSTART + 4, RLENGTH - 4)
            }
        ' | sort -u
    fi
}

# Stop whatever is listening on $1, waiting for the socket to be released.
stop_port() {
    port="$1"
    pids="$(port_listeners "$port")"
    [ -n "$pids" ] || return 0

    for pid in $pids; do
        cmd="$(ps -o args= -p "$pid" 2>/dev/null || true)"
        case "$cmd" in
            *gunbatte-server*) ;;
            *)
                echo "!! port $port is held by something else (pid $pid: $cmd)" >&2
                echo "   refusing to kill it; free the port or pick another one" >&2
                exit 1
                ;;
        esac
    done

    echo "==> stopping previous server on port $port (pid $(echo $pids | tr ' ' ','))"
    kill $pids 2>/dev/null || true
    n=0
    while [ "$n" -lt 50 ]; do
        pids="$(port_listeners "$port")"
        [ -n "$pids" ] || return 0
        # Fractional sleep is a GNU/coreutils extension; fall back on a strict
        # POSIX sleep so `set -e` can't abort the restart mid-wait.
        sleep 0.2 2>/dev/null || sleep 1
        n=$((n + 1))
    done

    echo "==> still holding the port after 10s, SIGKILL (pid $(echo $pids | tr ' ' ','))"
    kill -9 $pids 2>/dev/null || true
    sleep 0.5
}

# The server binary: cargo is a no-op when sources are unchanged.
echo "==> cargo build --release -p gunbatte-server"
cargo build --release -p gunbatte-server

# viewer/dist is a gitignored build artifact (wasm-pack + vite); the server
# runs fine without it, but the web viewer needs it. Rebuild by hand with
# `make viewer` after changing viewer/ sources, or delete dist to trigger this.
if [ ! -f viewer/dist/index.html ]; then
    echo "==> viewer/dist missing, building viewer (wasm-pack + vite)..."
    make viewer
fi

stop_port "$PORT"

echo "==> starting server on http://127.0.0.1:${PORT}/"
if [ -n "$port_in_args" ]; then
    exec ./target/release/gunbatte-server serve "$@"
else
    exec ./target/release/gunbatte-server serve --port "$PORT" "$@"
fi
