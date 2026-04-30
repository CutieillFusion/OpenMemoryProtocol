#!/usr/bin/env bash
# Hermetic end-to-end demo. No credentials, no network.
# Exercises the demo moment from 00-overview.md: time-traveling through an
# LLM agent's manifest revisions.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OMP="$ROOT/target/release/omp"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [ ! -x "$OMP" ]; then
  echo "building release binaries..." >&2
  ( cd "$ROOT" && cargo build --release --bin omp --bin omp-server )
fi

cd "$WORK"
echo "== init ==" >&2
"$OMP" init

echo "== stage the starter schemas + probes + omp.toml ==" >&2
# The repo needs schemas/probes in HEAD for time-travel; stage everything
# dropped by init.
shopt -s globstar
for f in omp.toml schemas/*/schema.toml probes/**/probe.wasm probes/**/probe.toml; do
  [ -f "$f" ] || continue
  "$OMP" add "$f" --from "$f" >/dev/null
done

echo "== add a text file under 'docs/hello.md' ==" >&2
cat > hello.md <<'EOF'
# Hello, world

This is an intro document used in the OMP demo.
EOF

"$OMP" add docs/hello.md --from hello.md --type text \
  --field title="Hello World (v1)" \
  --field "summary=First pass at an intro blurb." \
  --field tags="[intro,demo]" >/dev/null

"$OMP" commit -m "v1: add intro doc" >/dev/null
echo "HEAD after v1:" >&2
"$OMP" log --max 2

echo "== patch the manifest — new title, new summary ==" >&2
"$OMP" patch-fields docs/hello.md \
  --field title="Hello World (v2, tightened)" \
  --field "summary=Cleaner second pass; sharper opener." >/dev/null

"$OMP" commit -m "v2: tighten intro" >/dev/null
echo "HEAD after v2:" >&2
"$OMP" log --max 2

echo "== time-travel: what did the agent think on v1 vs v2? ==" >&2
printf '\n-- at HEAD~1 --\n'
"$OMP" show docs/hello.md --at HEAD~1 | grep -E '"(title|summary)"' || true
printf '\n-- at HEAD --\n'
"$OMP" show docs/hello.md --at HEAD | grep -E '"(title|summary)"' || true

echo "== probe coverage: inspect what the probes actually extracted ==" >&2

# Assert a manifest JSON (on stdin) contains a given top-level field under
# `[fields]`. Exits nonzero if missing, so a probe regression fails the demo.
assert_field_has() {
  local field="$1" expect_substring="${2:-}"
  local json
  json="$(cat)"
  if ! printf '%s' "$json" | grep -q "\"$field\""; then
    echo "FAIL: manifest missing field \"$field\"" >&2
    echo "$json" >&2
    exit 1
  fi
  if [ -n "$expect_substring" ] && ! printf '%s' "$json" | grep -q "$expect_substring"; then
    echo "FAIL: expected substring $expect_substring in manifest" >&2
    echo "$json" >&2
    exit 1
  fi
}

echo "-- hello.md's manifest (default view: compact, no provenance hashes) --" >&2
"$OMP" show docs/hello.md --at HEAD

echo "-- same manifest with --verbose (shows source_hash + probe_hashes) --" >&2
"$OMP" --verbose show docs/hello.md --at HEAD | head -25

# Every starter probe must have fired and its framed hash must have landed
# in `probe_hashes`. We use --verbose so the provenance table is in the JSON
# we can grep. The starter pack is tiny — 3 universal `file.*` probes.
manifest_json="$("$OMP" --verbose show docs/hello.md --at HEAD)"
for probe in file.size file.mime file.sha256; do
  printf '%s' "$manifest_json" | assert_field_has "$probe"
done
printf '%s' "$manifest_json" | assert_field_has "byte_size"
printf '%s' "$manifest_json" | assert_field_has "sha256"
printf '%s' "$manifest_json" | assert_field_has "mime" "text/"
echo "  ok: all 3 starter probes present in probe_hashes (via --verbose)" >&2

echo "== dry-run: test a schema change without staging ==" >&2
cat > text-new.schema <<'EOF'
file_type = "text"
mime_patterns = ["text/*"]

[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"

[fields.sha256]
source = "probe"
probe = "file.sha256"
type = "string"

[fields.mime]
source = "probe"
probe = "file.mime"
type = "string"

[fields.title]
source = "user_provided"
type = "string"
  [fields.title.fallback]
  source = "constant"
  value = "untitled"

[fields.app_version]
source = "constant"
value = "demo-0.1"
type = "string"
EOF

"$OMP" test-ingest docs/hello.md --from hello.md --proposed-schema text-new.schema \
  --field title="Dry-run title" \
  | grep -E '"(title|app_version)"' || true

echo "== done ==" >&2
echo "demo succeeded" >&2

echo "== status ==" >&2
"$OMP" status

echo "== ls ==" >&2
"$OMP" ls

# Strip the `<mode> <hash>` columns from `omp ls` so the probe listings read
# as plain filenames.
names_only() { awk '{print $NF}'; }

echo "== probes ==" >&2
"$OMP" ls probes | names_only

echo "== file probes ==" >&2
"$OMP" ls probes/file | names_only

echo "== schemas ==" >&2
"$OMP" ls schemas | names_only

