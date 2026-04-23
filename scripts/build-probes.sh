#!/usr/bin/env bash
# Build the starter-pack probe crates to wasm32-unknown-unknown and stage
# the compiled blobs under crates/omp-core/build/wasm/, where omp-core's
# starter.rs will include_bytes! them at compile time.
#
# The starter pack is intentionally tiny — 3 universal `file.*` probes.
# Adding a new filetype is a tree commit under `probes/`, not a change here.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROBES_SRC="$ROOT/probes-src"
STAGE="$ROOT/crates/omp-core/build/wasm"

if ! rustup target list --installed 2>/dev/null | grep -q '^wasm32-unknown-unknown$'; then
  echo "error: rustup target 'wasm32-unknown-unknown' is not installed." >&2
  echo "install with: rustup target add wasm32-unknown-unknown" >&2
  exit 1
fi

mkdir -p "$STAGE"

( cd "$PROBES_SRC" && cargo build --release )

# Source crate dir -> compiled wasm basename (matches each crate's lib name).
declare -A MAP=(
  [file-size]=file_size.wasm
  [file-mime]=file_mime.wasm
  [file-sha256]=file_sha256.wasm
)

# Source crate dir -> dotted wasm name we stage under `build/wasm/`.
declare -A NAMES=(
  [file-size]=file.size.wasm
  [file-mime]=file.mime.wasm
  [file-sha256]=file.sha256.wasm
)

WASM_OUT="$PROBES_SRC/target/wasm32-unknown-unknown/release"

for dir in "${!MAP[@]}"; do
  src="$WASM_OUT/${MAP[$dir]}"
  dst="$STAGE/${NAMES[$dir]}"
  if [ ! -f "$src" ]; then
    echo "error: compiled probe not found: $src" >&2
    exit 1
  fi
  cp "$src" "$dst"
done

echo "probes staged at: $STAGE"
ls -lh "$STAGE"
