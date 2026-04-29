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

cleanup() {
  log "stopping background services"
  for pid in $(jobs -p); do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

log "building binaries"
cargo build --release \
  -p omp-store -p omp-server -p omp-gateway -p omp-cli -p omp-builder >/dev/null

rm -rf "$DEMO_ROOT"
mkdir -p "$DEMO_ROOT/store-repo" "$DEMO_ROOT/shard-a" "$DEMO_ROOT/shard-b" "$DEMO_ROOT/builds"

OMP_BIN=./target/release/omp
STORE_BIN=./target/release/omp-store
SERVER_BIN=./target/release/omp-server
GATEWAY_BIN=./target/release/omp-gateway
BUILDER_BIN=./target/release/omp-builder

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

cat >"$DEMO_ROOT/gateway.toml" <<EOF
shards = [
  "http://127.0.0.1:$SHARD_A_PORT",
  "http://127.0.0.1:$SHARD_B_PORT",
]
allow_dev_tokens = true
builder = "http://127.0.0.1:$BUILDER_PORT"
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

log "done — services still running. Press Ctrl-C to tear down."
log "  store log:    $DEMO_ROOT/store.log"
log "  shard A log:  $DEMO_ROOT/shard-a.log"
log "  shard B log:  $DEMO_ROOT/shard-b.log"
log "  gateway log:  $DEMO_ROOT/gateway.log"
log "  builder log:  $DEMO_ROOT/builder.log"
log "  Build a probe via UI: $GATE/ui/probes/build (requires dev-* token)"

# Keep services alive until Ctrl-C.
wait
