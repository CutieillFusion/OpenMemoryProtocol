#!/usr/bin/env bash
# Boot the local dev stack: omp-server (API) + Vite dev frontend (hot reload).
#
# Defaults:
#   - server: 127.0.0.1:8000, tenants-base /tmp/omp-dev, --no-auth
#   - vite:   127.0.0.1:5173 (proxies API → :8000 per frontend/vite.config.ts)
#
# Override via env: OMP_DATA, OMP_API_PORT, OMP_UI_PORT, OMP_BUILD_MODE.
#
# Quits cleanly on Ctrl-C; both children get SIGTERM via the trap. Existing
# listeners on either port abort the script — kill them first or pick new
# ports.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OMP_DATA="${OMP_DATA:-/tmp/omp-dev}"
OMP_API_PORT="${OMP_API_PORT:-8000}"
OMP_UI_PORT="${OMP_UI_PORT:-5173}"
OMP_BUILD_MODE="${OMP_BUILD_MODE:-debug}"  # debug | release

API_BIND="127.0.0.1:${OMP_API_PORT}"
UI_BIND="127.0.0.1:${OMP_UI_PORT}"
LOG_DIR="${OMP_DATA}/logs"

log() { printf '[dev] %s\n' "$*"; }
die() { log "error: $*" >&2; exit 1; }

port_in_use() {
  ss -tln 2>/dev/null | awk '{print $4}' | grep -q ":$1\$"
}

API_PID=""
UI_PID=""
cleanup() {
  trap - EXIT INT TERM
  log "shutting down…"
  for pid in "$UI_PID" "$API_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  log "bye"
}
trap cleanup EXIT INT TERM

# --- preflight ---------------------------------------------------------------

command -v cargo >/dev/null || die "cargo not on PATH"
command -v npm >/dev/null   || die "npm not on PATH"

if port_in_use "$OMP_API_PORT"; then
  die "port $OMP_API_PORT already in use — kill the listener (e.g. 'pkill omp-server') and retry"
fi
if port_in_use "$OMP_UI_PORT"; then
  die "port $OMP_UI_PORT already in use — kill the listener and retry"
fi

mkdir -p "$OMP_DATA" "$LOG_DIR"

# --- build -------------------------------------------------------------------

log "building omp-server (${OMP_BUILD_MODE})"
if [ "$OMP_BUILD_MODE" = "release" ]; then
  cargo build --release -p omp-server >/dev/null
  SERVER_BIN="$ROOT/target/release/omp-server"
else
  cargo build -p omp-server >/dev/null
  SERVER_BIN="$ROOT/target/debug/omp-server"
fi

if [ ! -d "$ROOT/frontend/node_modules" ]; then
  log "installing frontend deps"
  (cd "$ROOT/frontend" && npm install --silent)
fi

# --- launch ------------------------------------------------------------------

log "starting omp-server on $API_BIND  (data: $OMP_DATA, no-auth)"
RUST_LOG="${RUST_LOG:-info}" "$SERVER_BIN" \
  --tenants-base "$OMP_DATA" --no-auth --bind "$API_BIND" \
  >"$LOG_DIR/server.log" 2>&1 &
API_PID=$!

log "starting Vite dev server on $UI_BIND"
( cd "$ROOT/frontend" && \
    OMP_DEV_BACKEND="http://$API_BIND" \
    npm run dev -- --host 127.0.0.1 --port "$OMP_UI_PORT" \
    >"$LOG_DIR/vite.log" 2>&1 ) &
UI_PID=$!

# Wait for both to actually listen — gives a clean failure if a child crashes.
wait_for_port() {
  local port="$1" name="$2" tries=120
  while ! port_in_use "$port"; do
    tries=$((tries - 1))
    [ "$tries" -le 0 ] && die "$name did not start (see $LOG_DIR/*.log)"
    sleep 0.5
    kill -0 "$3" 2>/dev/null || die "$name exited (see $LOG_DIR/*.log)"
  done
}

log "waiting for ports…"
wait_for_port "$OMP_API_PORT" "omp-server" "$API_PID"
wait_for_port "$OMP_UI_PORT"  "vite"       "$UI_PID"

cat <<EOF

[dev] ready

  UI       http://$UI_BIND/ui/
  API      http://$API_BIND/
  data     $OMP_DATA/_local
  logs     $LOG_DIR/{server,vite}.log

  pids     server=$API_PID  vite=$UI_PID

  Ctrl-C to stop both.

EOF

# Stream both logs interleaved so the user sees output without leaving the foreground.
tail -F "$LOG_DIR/server.log" "$LOG_DIR/vite.log" &
TAIL_PID=$!

# Block until any child exits, then trigger cleanup.
wait -n "$API_PID" "$UI_PID" || true
kill "$TAIL_PID" 2>/dev/null || true
