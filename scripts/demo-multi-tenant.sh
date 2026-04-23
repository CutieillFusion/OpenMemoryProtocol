#!/usr/bin/env bash
# Multi-tenant demo. Boots omp-server in multi-tenant mode against a tempdir,
# creates three tenants, and exercises:
#   1. 401 on missing/invalid Bearer tokens.
#   2. Cross-tenant isolation (alice can't see bob's files, and vice versa).
#   3. 429 quota_exceeded when a tight-quota tenant overflows.
#
# Hermetic: no credentials, no network, all state under a tempdir.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OMP="$ROOT/target/release/omp"
SERVER="$ROOT/target/release/omp-server"
PORT="${OMP_DEMO_PORT:-18765}"
BASE="http://127.0.0.1:$PORT"

WORK="$(mktemp -d)"
TENANTS_BASE="$WORK/tenants"
mkdir -p "$TENANTS_BASE"

SERVER_PID=""
cleanup() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

if [ ! -x "$OMP" ] || [ ! -x "$SERVER" ]; then
  echo "building release binaries..." >&2
  ( cd "$ROOT" && cargo build --release --bin omp --bin omp-server )
fi

need() { command -v "$1" >/dev/null || { echo "this demo needs $1 on PATH" >&2; exit 1; }; }
need curl

status_of() {
  # Print the HTTP status of a request. Usage: status_of <curl-args...>
  curl -s -o /dev/null -w "%{http_code}" "$@"
}

extract_token() {
  # Stdin: `omp admin tenant create` output. Stdout: the token line's value.
  awk '/^token:[[:space:]]/ {print $2}'
}

echo "== 1. seed the registry (pre-start; server reads it at boot) ==" >&2
ALICE_TOKEN=$(
  "$OMP" admin tenant create alice --tenants-base "$TENANTS_BASE" | extract_token
)
BOB_TOKEN=$(
  "$OMP" admin tenant create bob --tenants-base "$TENANTS_BASE" | extract_token
)
# Tight cap: any non-trivial upload will blow past 4 KB in on-disk objects.
TINY_TOKEN=$(
  "$OMP" admin tenant create tiny --tenants-base "$TENANTS_BASE" --bytes 4096 \
    | extract_token
)

"$OMP" admin tenant list --tenants-base "$TENANTS_BASE" >&2

echo >&2
echo "== 2. start the server in multi-tenant mode ==" >&2
"$SERVER" --tenants-base "$TENANTS_BASE" --bind "127.0.0.1:$PORT" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

# Wait for the server to accept connections.
for _ in $(seq 1 50); do
  if [ "$(status_of "$BASE/healthz")" = "200" ]; then
    break
  fi
  sleep 0.1
done
if [ "$(status_of "$BASE/healthz")" != "200" ]; then
  echo "server did not come up on $BASE" >&2
  cat "$WORK/server.log" >&2
  exit 1
fi
echo "server listening at $BASE (pid $SERVER_PID)" >&2

echo >&2
echo "== 3. auth: missing/invalid tokens return 401 ==" >&2
printf '  no token        -> %s\n' "$(status_of "$BASE/status")" >&2
printf '  bad  token      -> %s\n' "$(status_of -H 'Authorization: Bearer nope' "$BASE/status")" >&2
printf '  alice token     -> %s\n' "$(status_of -H "Authorization: Bearer $ALICE_TOKEN" "$BASE/status")" >&2

echo >&2
echo "== 4. alice uploads a file; bob uploads a different one ==" >&2
cat > "$WORK/alice.md" <<'EOF'
# alice's private notes
Only alice should see this.
EOF
cat > "$WORK/bob.md" <<'EOF'
# bob's private notes
Only bob should see this.
EOF

alice_curl() { curl -sf -H "Authorization: Bearer $ALICE_TOKEN" "$@"; }
bob_curl()   { curl -sf -H "Authorization: Bearer $BOB_TOKEN"   "$@"; }

alice_curl \
  -F "path=docs/private.md" \
  -F "file=@$WORK/alice.md" \
  -F "file_type=text" \
  -F "fields[title]=Alice-only" \
  "$BASE/files" >/dev/null

bob_curl \
  -F "path=docs/private.md" \
  -F "file=@$WORK/bob.md" \
  -F "file_type=text" \
  -F "fields[title]=Bob-only" \
  "$BASE/files" >/dev/null

alice_curl -X POST -H "Content-Type: application/json" \
  -d '{"message":"alice init","author":{"name":"alice","email":"a@demo","timestamp":"2026-04-22T00:00:00Z"}}' \
  "$BASE/commit" >/dev/null
bob_curl -X POST -H "Content-Type: application/json" \
  -d '{"message":"bob init","author":{"name":"bob","email":"b@demo","timestamp":"2026-04-22T00:00:00Z"}}' \
  "$BASE/commit" >/dev/null

echo "-- alice GET /files --" >&2
alice_curl "$BASE/files" >&2
echo >&2
echo "-- bob   GET /files --" >&2
bob_curl "$BASE/files" >&2
echo >&2
echo "-- alice GET /files/docs/private.md (title field) --" >&2
alice_curl "$BASE/files/docs/private.md" | grep -E '"title"' || true
echo "-- bob   GET /files/docs/private.md (title field) --" >&2
bob_curl "$BASE/files/docs/private.md" | grep -E '"title"' || true

echo >&2
echo "-- on-disk layout --" >&2
find "$TENANTS_BASE" -maxdepth 3 -type d | sort >&2

echo >&2
echo "== 5. quota: tiny (4 KB cap) trips 429 quota_exceeded ==" >&2
tiny_curl() { curl -s -H "Authorization: Bearer $TINY_TOKEN" "$@"; }
# Repeated uploads until the quota fires, or after 20 attempts give up.
SAW_429=0
for i in $(seq 1 20); do
  body="$(printf 'x%.0s' $(seq 1 1024))"  # ~1 KB of text
  printf '%s' "$body" > "$WORK/chunk.md"
  code=$(curl -s -o "$WORK/last.json" -w "%{http_code}" \
    -H "Authorization: Bearer $TINY_TOKEN" \
    -F "path=chunk-$i.md" \
    -F "file=@$WORK/chunk.md" \
    -F "file_type=text" \
    "$BASE/files")
  if [ "$code" = "429" ]; then
    echo "  upload #$i -> 429" >&2
    cat "$WORK/last.json" >&2
    echo >&2
    SAW_429=1
    break
  fi
done
if [ $SAW_429 -eq 0 ]; then
  echo "expected 429 within 20 uploads (raise quota or inspect)" >&2
  exit 1
fi

echo >&2
echo "== demo succeeded ==" >&2
