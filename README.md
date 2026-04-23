# OpenMemoryProtocol

A Git-like content store for files an LLM agent works with.

- Every user file is stored as **blob + manifest**. Manifest fields are declared by a **schema** (TOML), populated by **probes** (WASM modules), and committed alongside the file as content-addressed objects.
- **Schemas and probes live in the tree.** Adding a file type = committing a schema; adding extraction logic = committing a probe. No OMP release required.
- **Time-travel is free.** Every manifest records the `schema_hash` and the `probe_hashes` that produced it. `omp show <path> --at HEAD~5` returns the exact historical manifest.

Full design docs live under [`docs/design/`](docs/design/).

## Build

```
rustup target add wasm32-unknown-unknown
scripts/build-probes.sh          # compiles the starter probe pack
cargo build --release            # builds omp, omp-server, omp-core
```

The release binaries land at `target/release/omp` and `target/release/omp-server`.

## Try it

```
./scripts/demo.sh
```

The demo initializes a fresh repo in a tempdir, adds a text file with a summary + tags, commits it, revises the summary and commits again, then prints the manifest as it stood at each commit — showing the time-travel story.

## CLI quick reference

```
omp init
omp add <path> --from <disk-file> [--type <file_type>] [--field k=v ...]
omp show <path> [--at <ref>]
omp cat <path> [--at <ref>]
omp ls [<path>] [--at <ref>] [--recursive]
omp patch-fields <path> [--field k=v ...]
omp rm <path>
omp commit -m <msg>
omp log [--max 50] [<path>]
omp diff <from> <to> [<path>]
omp branch [<name> [<start>]]
omp checkout <ref>
omp test-ingest <path> [--from <file>] [--field k=v ...] [--proposed-schema <file>]
```

The HTTP server (`omp-server`) exposes the same operations at the routes listed in [`docs/design/06-api-surface.md`](docs/design/06-api-surface.md).

## What's in v1

Per [`docs/design/09-roadmap.md`](docs/design/09-roadmap.md):

- Four object types (blob, tree, manifest, commit), Git-style framing, SHA-256.
- Disk `ObjectStore` (`.omp/objects/<h[:2]>/<h[2:]>`), refs, HEAD.
- Path resolution through nested trees; canonical TOML for manifests.
- Schema loader, closed set of four field sources + fallback wrapper.
- Wasmtime-based probe host: fuel, memory, wall-clock caps; zero host imports.
- Starter probe pack: `file.*` (4) + `text.*` (3) are real; `pdf.*` (4) are v1 stubs (WASM modules compiled and committed, but they return CBOR null pending a pure-Rust PDF parser). Every PDF field with a `fallback` degrades gracefully.
- `axum` HTTP server + `clap` CLI. CLI ships the in-process transport; `--remote` is deferred.
- `cargo test -p omp-core` exercises hash stability, canonical-TOML property tests, path resolution, schema validation, probe sandbox (fuel + host-import refusal), and end-to-end ingest/commit/time-travel.

## What v1 does *not* include

See [`docs/design/09-roadmap.md`](docs/design/09-roadmap.md#explicit-non-goals-for-v1):

- Multi-tenancy / auth (iteration 2 headline feature — see [`11-multi-tenancy.md`](docs/design/11-multi-tenancy.md)).
- Merge / conflict resolution.
- Image / audio probes.
- Embedding-based search.
- Alternative `ObjectStore` backends (S3, Postgres).
- Garbage collection / pack files.
- `omp serve` via the `omp` binary — run the sibling `omp-server` binary instead.

## Layout

```
crates/
  omp-core/          # library: objects, store, paths, schemas, engine, probes
  omp-cli/           # `omp` binary
  omp-server/        # `omp-server` binary
probes-src/          # sibling cargo workspace — compiles to wasm32-unknown-unknown
docs/design/         # authoritative design; start at README.md
scripts/
  build-probes.sh    # compiles the starter pack
  demo.sh            # end-to-end hermetic demo
```
