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

log() { printf '[demo] %s\n' "$*"; }

cleanup() {
  log "stopping background services"
  for pid in $(jobs -p); do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

log "building binaries"
cargo build --release \
  -p omp-store -p omp-server -p omp-gateway -p omp-cli >/dev/null

rm -rf "$DEMO_ROOT"
mkdir -p "$DEMO_ROOT/store-repo" "$DEMO_ROOT/shard-a" "$DEMO_ROOT/shard-b"

OMP_BIN=./target/release/omp
STORE_BIN=./target/release/omp-store
SERVER_BIN=./target/release/omp-server
GATEWAY_BIN=./target/release/omp-gateway

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

cat >"$DEMO_ROOT/gateway.toml" <<EOF
shards = [
  "http://127.0.0.1:$SHARD_A_PORT",
  "http://127.0.0.1:$SHARD_B_PORT",
]
allow_dev_tokens = true
EOF

log "starting gateway on :$GATEWAY_PORT"
"$GATEWAY_BIN" \
  --bind "127.0.0.1:$GATEWAY_PORT" \
  --config "$DEMO_ROOT/gateway.toml" \
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
curl -fs -o /dev/null -w "status=%{http_code}\n" "$GATE/status" \
  -H "Authorization: Bearer wrong-token" || true

log "done — services still running. Press Ctrl-C to tear down."
log "  store log:    $DEMO_ROOT/store.log"
log "  shard A log:  $DEMO_ROOT/shard-a.log"
log "  shard B log:  $DEMO_ROOT/shard-b.log"
log "  gateway log:  $DEMO_ROOT/gateway.log"

# Keep services alive until Ctrl-C.
wait
