# 08 — Deployability

OMP is designed to be deployable in the places Git servers are deployable, and to leave room for deployment to places Git servers can't go.

## The core primitive — `ObjectStore`

All of OMP's storage logic talks to one narrow trait:

```rust
pub trait ObjectStore: Send + Sync {
    fn put(&self, type_: &str, content: &[u8]) -> Result<Hash>;        // store, return hash
    fn get(&self, hash: &Hash) -> Result<Option<(String, Vec<u8>)>>;   // -> (type, content)
    fn has(&self, hash: &Hash) -> Result<bool>;
    fn iter_refs(&self) -> Result<Box<dyn Iterator<Item = (String, Hash)> + '_>>;
    fn read_ref(&self, name: &str) -> Result<Option<Hash>>;
    fn write_ref(&self, name: &str, commit: &Hash) -> Result<()>;
    fn delete_ref(&self, name: &str) -> Result<()>;
    fn read_head(&self) -> Result<String>;                              // "ref: refs/..." or raw hash
    fn write_head(&self, value: &str) -> Result<()>;
}
```

Nine methods. Everything OMP does reduces to these. `Send + Sync` lets the trait object be shared across `tokio` tasks so multiple concurrent requests hit the same backend without per-request locking.

Because this interface is narrow and doesn't leak implementation details (no file paths, no SQL, no keys), swapping the backend is a focused change. The ingest engine, schema validator, commit logic, and HTTP server have no idea whether the backend is disk, S3, Postgres, or a hashmap.

## The v1 backend — disk

`crates/omp-core/src/store/disk.rs` implements `ObjectStore` against the local filesystem using `flate2` for zlib and the `fs2` crate for advisory file locks:

- Objects: zlib-compressed framed bytes written to `.omp/objects/<h[:2]>/<h[2:]>`.
- Refs: plain text files under `.omp/refs/`.
- HEAD: plain text at `.omp/HEAD`.
- Uses `fs2::FileExt::lock_exclusive` (POSIX `flock` / Windows `LockFileEx`) during ref updates and commit staging for process-level exclusion.

Concurrency characteristics: single-writer safe per tenant; multi-reader safe. Hosted deployments scale by distributing tenants across replicas, not by parallelizing writes within a tenant — see [`11-multi-tenancy.md`](./11-multi-tenancy.md).

## Deployment targets

### Local development

```
cargo install --path crates/omp-cli
omp init
omp add ...
omp serve
```

Works. This is the primary dev loop. During development, `cargo run --bin omp -- serve` is enough.

### Docker with a volume

```dockerfile
FROM rust:1.85 AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --bin omp

FROM debian:stable-slim
COPY --from=builder /src/target/release/omp /usr/local/bin/omp
VOLUME /repo
WORKDIR /repo
CMD ["omp", "serve", "--bind", "0.0.0.0:8000"]
```

Final image is ~20 MB (`debian:stable-slim` base; smaller with `distroless` or `FROM scratch` + `musl` target). Mount `/repo` to a persistent volume, expose port 8000. Works.

### Kubernetes

- **StatefulSet** for v1 disk-backed single-tenant deployments (state matters, single replica per tenant).
- **PersistentVolumeClaim** for `/repo` in the disk-backed path.
- **Horizontal scale** in iteration 2: deploy as a `Deployment` with N replicas, all pointing at a shared S3 or Postgres `ObjectStore` backend. Tenant routing (consistent hash on tenant id, or stateless dispatch with optimistic ref CAS) keeps per-tenant single-writer invariants even across replicas. See [`11-multi-tenancy.md`](./11-multi-tenancy.md).
- **Service** exposing the server port internally.
- **Liveness probe**: `GET /status`.

Zero application changes to go from single-replica-disk to multi-replica-shared-backend — only the backend swap and a `--tenant-routing` flag.

### VM / bare metal

Directory on disk + systemd unit running `omp serve`. Works. No special infrastructure.

### Serverless (Vercel Functions, AWS Lambda)

**Does not work with the v1 disk backend.** The disk backend needs durable local storage, and serverless functions don't have it.

Mitigation paths (not in v1, but unblocked by the design):

1. **S3-backed store** — implement `ObjectStore` against S3 (or R2, or any S3-compatible storage). Works for serverless. Eventual consistency on S3 is acceptable because objects are content-addressed; ref writes use conditional puts for atomicity.
2. **Postgres-backed store** — a single table `objects(hash PRIMARY KEY, type, content BYTEA)` plus a `refs` table. Works for serverless if the Postgres instance is reachable.
3. **Vercel Blob or equivalent** — same shape as S3 backend.

Each of these is ~200–400 lines of Rust implementing the nine-method trait. None of them require touching anything else in OMP.

The explicit decision for v1 is: ship the disk backend only. Don't ship a serverless-compatible backend *and* the disk backend *and* migration tools all at once. Keep the surface small, prove the design works, then add backends as needed.

## What the disk backend depends on

- POSIX-ish filesystem (works on Linux, macOS, Windows with minor path handling via `std::path`).
- `flate2` crate for zlib.
- `fs2` crate for cross-platform advisory file locks.
- Durable writes: `File::sync_all()` after writing objects to avoid losing unreferenced-but-committed blobs on crash.

Nothing else. No database, no network, no external service.

## What OMP itself depends on (beyond the backend)

- `wasmtime` (Rust crate) — the WASM runtime that executes probes. Compiled into the binary. See [`05-probes.md`](./05-probes.md).
- `ciborium` — CBOR encoding for the probe ABI payload.
- `axum` + `tokio` + `hyper` — the async HTTP layer.
- `clap` — the CLI layer.
- `toml` / `toml_edit` + a tiny in-tree canonicalizer (sorted keys, fixed float format, LF line endings) — the canonicalizer is necessary because Rust's TOML crates don't guarantee byte-identical round-trips, which the manifest-hash contract requires.
- `sha2` — SHA-256 for content addressing.
- `flate2` — zlib for object framing.
- `serde` + `serde_json` — HTTP payload serialization.

All are pure-Rust crates; the final artifact is a single static(-ish) binary from `cargo build --release`.

## Resource footprint

For v1 target use cases (single-tenant) and hosted iteration-2 deployments (many tenants, commodity node):

- **Storage**: proportional to content. Each object compressed ~2×. Small repos are kilobytes; a repo of 1000 PDFs + manifests is a few hundred megabytes.
- **Memory**: the server is mostly I/O-bound. Idle `axum` server + wasmtime ~15 MB RAM. Active ingestion of a 40-page PDF (with per-probe WASM sandboxes capped at 64 MB each) spikes to ~90 MB per concurrent ingest. Many concurrent reads add negligible memory because they are async and don't each hold a sandbox.
- **CPU**: dominated by probes. PDF text extraction is the heaviest v1 probe; ~150ms for a 40-page document inside the WASM sandbox. Trivial probes (`file.size`, `file.sha256`) cost sub-millisecond per invocation including sandbox setup when modules are pooled. Native wasmtime integration means these costs do not funnel through a Python-FFI boundary or a GIL.
- **Disk writes per ingest**: ~5 objects (blob, manifest, tree-updates-along-the-path, commit). Small — milliseconds on SSD.
- **Throughput target (hosted)**: one commodity node handles hundreds of concurrent read clients and tens of concurrent ingest clients across tenants before needing a second replica.

## Backup / migration

- **Backup**: back up `.omp/objects/` and `.omp/refs/`. That's the whole state. `.omp/HEAD` and `.omp/local.toml` are also there but regenerable.
- **Migration between backends**: an `omp admin export` / `omp admin import` pair (post-v1) that streams all objects and refs through the `ObjectStore` interface. Because objects are content-addressed, migration is idempotent and verifiable: every migrated object has the same hash on the destination.

## What this buys us

The `ObjectStore` abstraction is the single reason OMP can claim "serverless compatibility is a backend swap, not a rewrite." Without it, we'd have disk paths leaking into ingest, commit, schema validation, etc., and adding S3 support would be a three-month refactor instead of a weekend.

It's also what makes the "microservice decomposition" direction (post-v1 extra credit) sane: if the object store is a service, the ingest engine is a service, and the API gateway is a service, they all communicate through the same interface that the in-process code uses today. No logic moves, only transports change.

One caveat: the Rust trait as written (`Send + Sync`, `Result<_, OmpError>`, `Box<dyn Iterator>`) doesn't serialize directly over a wire. When the store is split out, `ObjectStore` becomes the *conceptual* contract and a sibling `proto/store.proto` defines the actual wire types (`PutRequest`, `GetResponse`, a server-streaming `IterRefs`). The in-process trait stays as the canonical shape; the gRPC definitions are a mechanical projection of it. That projection is modest work, but it's work — not free.
