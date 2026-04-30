#!/usr/bin/env bash
# Local end-to-end demo of the microservices decomposition.
#
# Spins up:
#   - omp-store (gRPC) on :9001 backed by a temp DiskStore
#   - omp-server shard A on :8001 (single-tenant repo at /tmp/omp-demo/shard-a)
#   - omp-server shard B on :8002 (single-tenant repo at /tmp/omp-demo/shard-b)
#   - omp-gateway on :8080 routing alice/bob to the two shards
#
# Then runs a few `curl` commands through the gateway.

set -euo pipefail

DEMO_ROOT=${DEMO_ROOT:-/tmp/omp-demo}
GATEWAY_PORT=${GATEWAY_PORT:-8080}
STORE_PORT=${STORE_PORT:-9001}
SHARD_A_PORT=${SHARD_A_PORT:-8001}
SHARD_B_PORT=${SHARD_B_PORT:-8002}
BUILDER_PORT=${BUILDER_PORT:-9100}
MARKETPLACE_PORT=${MARKETPLACE_PORT:-9200}

# Source .env at the repo root if it exists. Lets the operator put
# WORKOS_CLIENT_ID / WORKOS_CLIENT_SECRET (and optionally
# WORKOS_ISSUER_URL / WORKOS_REDIRECT_URI) there once and have the demo
# pick them up automatically. Without these, the gateway runs in token
# mode and the web UI shows "Browser sign-in not configured".
ENV_FILE="$(cd "$(dirname "$0")/.." && pwd)/.env"
if [ -f "$ENV_FILE" ]; then
  set -a
  # shellcheck disable=SC1090
  . "$ENV_FILE"
  set +a
fi
WORKOS_ISSUER_URL=${WORKOS_ISSUER_URL:-https://api.workos.com}
WORKOS_REDIRECT_URI=${WORKOS_REDIRECT_URI:-http://localhost:$GATEWAY_PORT/auth/callback}

log() { printf '[demo] %s\n' "$*"; }

# Pattern used by both the preflight kill and the cleanup trap. Anchored
# to the demo's exact release-build paths so we never clobber an unrelated
# `omp-*` process the user happens to be running.
DEMO_PROC_PATTERN='./target/release/omp-(store|server|gateway|builder|marketplace)'

kill_demo_procs() {
  pkill -u "$USER" -f "$DEMO_PROC_PATTERN" 2>/dev/null || true
}

cleanup() {
  log "stopping background services"
  for pid in $(jobs -p); do kill "$pid" 2>/dev/null || true; done
  # Backstop: any child that detached or that we lost track of (e.g.
  # because a prior trap was interrupted) still gets cleaned up via
  # binary-pattern match.
  kill_demo_procs
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Preflight: clear leftovers from a prior, possibly-aborted run. Without
# this, a stale gateway can hold a port; new binaries fail to bind
# silently (background jobs); smoke checks then target the stale gateway
# and look like a code regression. Real bug observed during the
# marketplace integration.
if pgrep -u "$USER" -f "$DEMO_PROC_PATTERN" >/dev/null 2>&1; then
  log "killing leftover demo processes from a prior run"
  kill_demo_procs
  # Give the kernel a moment to release the bound ports before we relaunch.
  sleep 1
fi

log "rebuilding embedded frontend"
# Force a fresh frontend build so the gateway picks up the latest UI on every
# demo run. The gateway's build.rs short-circuits when `frontend/build/` already
# exists, which means a cached build can mask UI source changes — clear it first.
# Prefer bun (matches frontend/bun.lock) and fall back to npm. Vite logs CSS
# and a11y warnings to stderr on every build; we redirect both streams to a
# log file so they don't drown the demo output, but keep them on disk for
# debugging when the build actually fails.
FRONTEND_DIR="$(cd "$(dirname "$0")/.." && pwd)/frontend"
FRONTEND_LOG="$DEMO_ROOT/frontend-build.log"
mkdir -p "$DEMO_ROOT"
rm -rf "$FRONTEND_DIR/build"
if command -v bun >/dev/null 2>&1; then
  ( cd "$FRONTEND_DIR" && bun install --frozen-lockfile && bun run build ) \
    >"$FRONTEND_LOG" 2>&1 || {
    log "frontend build failed — see $FRONTEND_LOG"
    tail -40 "$FRONTEND_LOG" >&2
    exit 1
  }
else
  ( cd "$FRONTEND_DIR" && npm ci && npm run build ) \
    >"$FRONTEND_LOG" 2>&1 || {
    log "frontend build failed — see $FRONTEND_LOG"
    tail -40 "$FRONTEND_LOG" >&2
    exit 1
  }
fi

log "building binaries"
# OMP_SKIP_UI_BUILD tells the gateway's build.rs to trust the frontend/build/
# directory we just produced rather than invoking `npm` itself. Avoids a
# second (npm-based) build inside cargo on a bun-tooled repo.
OMP_SKIP_UI_BUILD=1 cargo build --release \
  -p omp-store -p omp-server -p omp-gateway -p omp-cli -p omp-builder -p omp-marketplace >/dev/null

rm -rf "$DEMO_ROOT"
mkdir -p "$DEMO_ROOT/store-repo" "$DEMO_ROOT/shard-a" "$DEMO_ROOT/shard-b" "$DEMO_ROOT/builds" "$DEMO_ROOT/marketplace"

OMP_BIN=./target/release/omp
STORE_BIN=./target/release/omp-store
SERVER_BIN=./target/release/omp-server
GATEWAY_BIN=./target/release/omp-gateway
BUILDER_BIN=./target/release/omp-builder
MARKETPLACE_BIN=./target/release/omp-marketplace

log "initializing repos"
"$OMP_BIN" init --repo "$DEMO_ROOT/shard-a" >/dev/null
"$OMP_BIN" init --repo "$DEMO_ROOT/shard-b" >/dev/null

# Even though the gRPC store is wired, the demo gateway routes to the
# omp-server shards directly (each with its own DiskStore). The gRPC store
# binary still launches to demonstrate it's running and accepts traffic;
# `crates/omp-store-client/tests/grpc_round_trip.rs` exercises it for real.
log "starting omp-store on :$STORE_PORT (gRPC)"
"$STORE_BIN" --bind "127.0.0.1:$STORE_PORT" --repo "$DEMO_ROOT/store-repo" \
  >"$DEMO_ROOT/store.log" 2>&1 &

log "starting shard A on :$SHARD_A_PORT"
"$SERVER_BIN" --bind "127.0.0.1:$SHARD_A_PORT" --repo "$DEMO_ROOT/shard-a" \
  >"$DEMO_ROOT/shard-a.log" 2>&1 &

log "starting shard B on :$SHARD_B_PORT"
"$SERVER_BIN" --bind "127.0.0.1:$SHARD_B_PORT" --repo "$DEMO_ROOT/shard-b" \
  >"$DEMO_ROOT/shard-b.log" 2>&1 &

log "starting omp-builder on :$BUILDER_PORT"
"$BUILDER_BIN" \
  --bind "127.0.0.1:$BUILDER_PORT" \
  --scratch-root "$DEMO_ROOT/builds" \
  --probe-common-path "$(pwd)/probes-src/probe-common" \
  >"$DEMO_ROOT/builder.log" 2>&1 &

# omp-marketplace runs without --verifying-key in the demo so authed
# endpoints (publish/yank) accept any TenantContext. The binary logs a
# loud warning; production deployments must pass the gateway's verifying
# key. See `docs/design/23-probe-marketplace.md`.
log "starting omp-marketplace on :$MARKETPLACE_PORT"
"$MARKETPLACE_BIN" \
  --bind "127.0.0.1:$MARKETPLACE_PORT" \
  --data-root "$DEMO_ROOT/marketplace" \
  >"$DEMO_ROOT/marketplace.log" 2>&1 &

cat >"$DEMO_ROOT/gateway.toml" <<EOF
shards = [
  "http://127.0.0.1:$SHARD_A_PORT",
  "http://127.0.0.1:$SHARD_B_PORT",
]
allow_dev_tokens = true
builder = "http://127.0.0.1:$BUILDER_PORT"
marketplace = "http://127.0.0.1:$MARKETPLACE_PORT"
EOF

# When WORKOS_CLIENT_ID is set (e.g., via .env), enable browser sign-in.
# Without it the gateway runs in pure token mode and the web UI surfaces
# the "Browser sign-in not configured" page (which is the right behavior;
# the CLI keeps using `Authorization: Bearer dev-*`).
if [ -n "${WORKOS_CLIENT_ID:-}" ]; then
  log "WorkOS configured — enabling browser sign-in for the web UI"
  cat >>"$DEMO_ROOT/gateway.toml" <<EOF

[workos]
client_id = "$WORKOS_CLIENT_ID"
issuer_url = "$WORKOS_ISSUER_URL"
redirect_uri = "$WORKOS_REDIRECT_URI"
EOF
fi

log "starting gateway on :$GATEWAY_PORT"
# Persist the signing key across restarts so a developer doesn't get
# logged out every time they `Ctrl-C` the demo and start it again.
SIGNING_KEY="$DEMO_ROOT/gateway-signing.key"
if [ ! -f "$SIGNING_KEY" ]; then
  python3 -c "import secrets,sys; sys.stdout.buffer.write(secrets.token_bytes(32))" >"$SIGNING_KEY"
fi
WORKOS_CLIENT_SECRET="${WORKOS_CLIENT_SECRET:-}" \
"$GATEWAY_BIN" \
  --bind "127.0.0.1:$GATEWAY_PORT" \
  --config "$DEMO_ROOT/gateway.toml" \
  --signing-key "$SIGNING_KEY" \
  >"$DEMO_ROOT/gateway.log" 2>&1 &

# Give services a moment to start.
sleep 1

GATE="http://127.0.0.1:$GATEWAY_PORT"

log "[demo] gateway healthz:"
curl -fsS "$GATE/healthz"
echo

log "[demo] alice via gateway: POST a file"
TMPFILE=$(mktemp)
echo "hello from alice" >"$TMPFILE"
curl -fsS -X POST "$GATE/files" \
  -H "Authorization: Bearer dev-alice" \
  -F "path=alice.txt" \
  -F "file=@$TMPFILE" >/dev/null
rm "$TMPFILE"

curl -fsS -X POST "$GATE/commit" \
  -H "Authorization: Bearer dev-alice" \
  -H "Content-Type: application/json" \
  -d '{"message":"alice add"}'
echo

log "[demo] bob via gateway: POST a file"
TMPFILE=$(mktemp)
echo "hello from bob" >"$TMPFILE"
curl -fsS -X POST "$GATE/files" \
  -H "Authorization: Bearer dev-bob" \
  -F "path=bob.txt" \
  -F "file=@$TMPFILE" >/dev/null
rm "$TMPFILE"

curl -fsS -X POST "$GATE/commit" \
  -H "Authorization: Bearer dev-bob" \
  -H "Content-Type: application/json" \
  -d '{"message":"bob add"}'
echo

log "[demo] /files on shard A directly:"
curl -fsS "http://127.0.0.1:$SHARD_A_PORT/files" | head -c 400
echo
log "[demo] /files on shard B directly:"
curl -fsS "http://127.0.0.1:$SHARD_B_PORT/files" | head -c 400
echo

log "[demo] unauthorized request rejected at gateway:"
# /status is intentionally unauth-readable in WorkOS mode (per doc 22 the
# frontend probes it to learn `auth_mode`). Hit /files instead — that
# always requires a resolved tenant.
curl -fs -o /dev/null -w "status=%{http_code}\n" "$GATE/files" \
  -H "Authorization: Bearer wrong-token" || true

log "[demo] embedded UI smoke check:"
if curl -fsS -o /dev/null -w "/ui/ status=%{http_code}\n" "$GATE/ui/"; then
  log "  Web UI:     $GATE/ui/"
  log "  paste a dev token like 'dev-alice' when the modal appears."
else
  log "  /ui/ not reachable — gateway built without --features embed-ui?"
fi

log "[demo] omp-builder healthz (direct):"
curl -fs -o /dev/null -w "status=%{http_code}\n" "http://127.0.0.1:$BUILDER_PORT/healthz" || true

log "[demo] omp-marketplace healthz (direct):"
curl -fs -o /dev/null -w "status=%{http_code}\n" "http://127.0.0.1:$MARKETPLACE_PORT/healthz" || true

log "[demo] probe marketplace listing via gateway:"
curl -fs -o /dev/null -w "status=%{http_code}\n" \
  -H "Authorization: Bearer dev-alice" \
  "$GATE/marketplace/probes" || true

log "[demo] schema marketplace listing via gateway:"
curl -fs -o /dev/null -w "status=%{http_code}\n" \
  -H "Authorization: Bearer dev-alice" \
  "$GATE/marketplace/schemas" || true

log "done — services still running. Press Ctrl-C to tear down."
log "  store log:        $DEMO_ROOT/store.log"
log "  shard A log:      $DEMO_ROOT/shard-a.log"
log "  shard B log:      $DEMO_ROOT/shard-b.log"
log "  gateway log:      $DEMO_ROOT/gateway.log"
log "  builder log:      $DEMO_ROOT/builder.log"
log "  marketplace log:  $DEMO_ROOT/marketplace.log"
log "  frontend build:   $DEMO_ROOT/frontend-build.log"
log "  Build a probe via UI: $GATE/ui/probes/build (requires dev-* token)"

# Keep services alive until Ctrl-C.
wait
