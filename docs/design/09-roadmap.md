# 09 — Roadmap

This doc lays out what ships when. Three explicit tiers: **v1** (the first demo), **iteration 2** (what we add on top of v1 without restructuring anything), and **deferred** (intentionally not being designed for yet).

Implementation language is **Rust**. The host binary and the starter-pack probes live in one `cargo` workspace. Rust was chosen over Python after the target expanded from single-agent laptop use to multi-tenant Git-style hosting; see the language rationale in [`08-deployability.md`](./08-deployability.md) and the multi-tenant design in [`11-multi-tenancy.md`](./11-multi-tenancy.md).

## v1 — first demo, 2–3 weeks of focused work

### Goal

A working end-to-end OMP that a grader or collaborator can clone, `cargo build`, and use to ingest a PDF, edit a schema, and time-travel through historical manifests. Runs on a laptop with no external services, no API keys. **Single tenant** — the multi-tenant layer is iteration 2. v1's job is to prove the Rust port of the design works end-to-end before adding the tenancy dimension.

### In scope

**Core plumbing:**
- Four object types (blob, tree, manifest, commit) with Git-style framing and zlib compression.
- SHA-256 content addressing on canonical bytes.
- `ObjectStore` trait with the disk backend implemented (`crates/omp-core/src/store/disk.rs`).
- Refs and HEAD as text files, branches, DAG walks.
- Path resolution through nested trees (`crates/omp-core/src/paths.rs`).
- Working tree walker with ignore-pattern support.
- Canonical TOML serializer (`crates/omp-core/src/toml_canonical.rs`) — sorted keys, fixed float format, LF line endings, property-tested for byte-identical round-trip. Non-trivial in Rust; worth calling out as a v1 deliverable.

**Ingest:**
- Schema loader + TOML validator.
- Starter schemas for `text` and `pdf` shipped in the `omp init` template.
- Ingest engine resolving all four field sources (`constant`, `probe`, `user_provided`, `field`) plus the `fallback` wrapper.
- WASM runtime (wasmtime Rust crate) + probe loader with per-module fuel, memory, and wall-clock caps.
- Starter probe pack dropped into the tree by `omp init`: `file.*` (4), `text.*` (3), `pdf.*` (4), each as a compiled WASM module (built by the same cargo workspace) with a sibling `.probe.toml`. See [`05-probes.md`](./05-probes.md).

**API layer:**
- `omp_core::api` with every documented operation (`init`, `add`, `commit`, `log`, `show`, `diff`, `branch`, `checkout`, `test_ingest`, `patch_fields`, `ls`, `remove`).
- `axum` HTTP server exposing the same operations.
- `clap`-based CLI wrapping the same operations (direct call by default, `--remote` for HTTP).

**Quality bar:**
- `cargo test` suite covering hash stability, wire-format round-trip, store round-trip, schema validation, engine field resolution, path resolution, commits, branches, time-travel, server smoke, CLI smoke. Property tests on canonical TOML.
- Fully deterministic — no network, no LLM stub, no timestamps-that-vary (tests inject fixed timestamps).
- README with a reproducible end-to-end demo that runs cold without external credentials.

### Explicit non-goals for v1

Things we're not building in v1, with reasons:

- **Multi-tenancy / auth**: the headline feature of iteration 2 and the main scalability story. Left out of v1 so the language port and core plumbing land first without the tenancy dimension confounding bugs. See [`11-multi-tenancy.md`](./11-multi-tenancy.md).
- **Merge / conflict resolution**: structural union works for the single-writer case; merge is only interesting once multi-agent flows exist. Deferred. (The commit object format already permits multiple `parent` lines, so adding merge later does not require a format change — v1 simply never writes a multi-parent commit.)
- **Image and audio probes**: the architecture supports them (new WASM modules + default schemas), but writing image/audio decoders in Rust-to-WASM is real work, and the starter pack stays focused. Deferred to iteration 2.
- **Embedding-based search**: requires a model, embeddings storage, index. Deferred indefinitely.
- **Microservice split**: designed for later; the codebase is organized to make it cheap when the time comes.
- **Event streaming (Kafka/NATS)**: only interesting once services exist.
- **Web UI**: CLI + curl is enough for the demo.
- **Remote protocol (push/pull)**: no collaboration use case in v1.
- **Garbage collection / packfiles**: loose objects are fine at the scales v1 targets (thousands of files, not millions). Explicit consequence: `DELETE /files/{path}` (and ref / branch deletion) only unlinks objects from the reachable graph; the `.omp/objects/` files remain on disk until an `omp admin gc` command exists. v1 repos can therefore grow monotonically even through heavy delete-and-re-add churn. Iteration 2 ships `omp admin verify` (reachability + integrity check) as the precursor to a real `gc`; actual reclamation is in the "deferred" tier below.

## Iteration 2 — natural extensions

After v1 is solid, the next tier of features fits without restructuring anything. Rough cost estimates assume v1's design is correct.

### Multi-tenancy (headline iteration-2 feature)

This is where the scale/maintainability story lands — see [`11-multi-tenancy.md`](./11-multi-tenancy.md) for the full design. Cost: ~1–2 weeks.

- Introduce a `Tenant` namespace in the `ObjectStore` trait (object and ref keys are prefixed by tenant id; the disk backend maps tenants to subdirectories, the S3 backend to key prefixes).
- Add per-tenant auth middleware in `axum` (Bearer token checked against a tenant registry).
- Add per-tenant quotas (object count, bytes, probe fuel per request).
- Every HTTP route scopes to the calling tenant; cross-tenant access is a compile-time impossibility because the `Repo` handle carries the tenant in its type.
- Single-tenant `omp serve --no-auth` remains for local dev.

### Storage backend alternatives

- S3-backed `ObjectStore` (for serverless and horizontal-scale hosting).
- Postgres-backed `ObjectStore` (for shared-service deployments with transactional ref updates).
- Each is ~200–400 lines of Rust, independently testable against the same conformance suite.

### Image and audio ingestion

- Add `image.*` and `audio.*` WASM probes to the starter pack (dimensions/EXIF/OCR for images; duration/channels/transcript-excerpt for audio). Sources live as additional crates in the cargo workspace; the build embeds them.
- Ship starter `schemas/image.schema` and `schemas/audio.schema` in the `omp init` template.
- No OMP ABI change — only the starter pack grows.
- Cost: ~3–4 days per file type, most of it on probe implementation and fixtures.

### Basic merge (same-path same-mode)

Implement three-way merge for trees. Fast-forward is trivial. Real merges:

- Disjoint changes (different paths on both branches): union.
- Same path, same mode, identical content on both branches: trivial.
- Same path, manifest conflict: two strategies — `keep-both` (stores both manifests under `<name>` and `<name>.conflict.<branch>`) and `last-write-wins` (picks one by commit timestamp). Default `keep-both`.
- Blob conflicts (schema files): fall back to the same two strategies.

No semantic merge of TOML content in v2. Cost: ~1 week.

### Runtime performance

- Tree caching in-process (avoid re-parsing on every path walk).
- Lazy fetch for remote backends.
- WASM module pooling across ingest requests so hot probes don't pay instantiation cost each call.

### `omp admin` CLI group

- `omp admin export` / `omp admin import` — dump and restore a repo.
- `omp admin verify` — walk all reachable objects, re-hash each, report integrity issues.
- `omp admin backend switch <type>` — migrate a repo between backends.

## Deferred — not being designed for yet

These are not roadmap items; they're things someone will eventually ask for, and we should not bake current-v1 decisions around them until someone actually asks.

- **Embedding-based retrieval.** Adding vector search over manifests. Requires embedding models, a vector index, query language. Potentially a full subsystem.
- **Remote protocol (push/pull).** OMP clones talking to each other over the network. Significant surface — Git spent years on its own protocol.
- **Garbage collection / pack files.** For million-object repos, loose objects are inefficient. Solve when it matters.
- **Multi-writer with operational transforms or CRDT merge.** Serious distributed systems problem.
- **Per-branch write ACLs.** Iteration 2 introduces tenant-level auth; finer-grained per-branch permissions within a tenant are a separate concern deferred until someone asks for them.
- **Web UI.** Browser-based repo browser. Nice demo, but lots of code; better built on top of a settled API.
- **Rich diff viewers / inspectors.** Side-by-side manifest diffs in a UI, provenance visualizations, etc.
- **Cross-repo schema reuse.** A way to import schemas (and the probes they depend on) from another repo.
- **`omp probes refresh`** to pull newer starter-pack probes into an existing repo without hand-copying. Not a new capability — starter-pack probes are already editable blobs in the tree — just ergonomics.

## Course-project framing (optional)

If OMP is submitted as a microservices-course project, the natural decomposition is:

- **Gateway service** — terminates HTTP, routes to backends.
- **Object store service** — wraps `ObjectStore` over gRPC or HTTP. One instance per backend type.
- **Ingest service** — runs probes and the engine. Scales independently.
- **Refs / commit service** — holds refs, serializes commits, enforces consistency.
- **Query service** — exposes listing, search, time-travel queries.
- **CLI + web UI** — client.

This split happens *after* v1 is solid. The splitting itself is mostly mechanical because `omp_core::api` is already the only place with real logic, and the `ObjectStore` trait already exists.

Do not split early. Do not design v1 around the split. Ship the monolith first; the seams it reveals are the right ones to split along.
