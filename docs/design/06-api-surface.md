# 06 — API surface

OMP exposes two interfaces: an **HTTP API** for programmatic clients (the primary audience: LLM agents) and a **CLI** for humans. Both are thin wrappers around a single Rust module, `omp_core::api`, which holds all the real logic. If an operation is valid on one interface, it's valid on the other with the same semantics.

## Design principles

1. **Small surface.** Fewer endpoints beats more endpoints. `POST /files` covers file upload, schema upload, config update — anything in the working tree. No `POST /schemas` or `POST /prompts` or `POST /config`.
2. **Staging then commit.** Operations that change the repo state *stage* a change. Nothing is committed until `POST /commit`. Mirrors Git's index-then-commit flow; makes multi-file commits trivial.
3. **Time-travel is a query parameter.** Every read endpoint accepts `?at=<commit>` (or `?at=<branch>` or `?at=<ref-expr>`). Default is HEAD.
4. **Dry-runs are first-class.** `POST /test/ingest` exists from day one so LLMs can propose schema changes without committing.

## The contract — `omp_core::api`

All the real operations live as free functions / methods on the `Repo` handle in `crates/omp-core/src/api.rs`. Signatures (Rust pseudocode, eliding error types — every call returns `Result<_, OmpError>`):

```rust
// Repo lifecycle
pub fn init(path: &Path) -> Result<()>
pub fn status(&self) -> Result<RepoStatus>

// Files & manifests
// Returns AddResult::Manifest for user-file paths (anything not under schemas/ and not omp.toml at the repo root).
// Returns AddResult::Blob for schema uploads (schemas/*.schema) and omp.toml, since those are stored as blobs
// and have no manifest. Callers match on the enum.
pub fn add(&self, path: &str, bytes: &[u8], user_fields: Option<Fields>, file_type: Option<&str>) -> Result<AddResult>
pub fn patch_fields(&self, path: &str, updates: Fields) -> Result<Manifest>
pub fn remove(&self, path: &str) -> Result<()>
pub fn show(&self, path: &str, at: Option<&str>) -> Result<ShowResult>  // Manifest | Blob | Tree
pub fn bytes_of(&self, path: &str, at: Option<&str>) -> Result<Bytes>
pub fn ls(&self, path: &str, at: Option<&str>, recursive: bool) -> Result<Vec<TreeEntry>>

// Commits & history
pub fn commit(&self, message: &str, author: Option<&str>) -> Result<CommitHash>
pub fn log(&self, path: Option<&str>, max: usize) -> Result<Vec<Commit>>
pub fn diff(&self, from: &str, to: &str, path: Option<&str>) -> Result<Diff>

// Branches
pub fn branch(&self, name: &str, start: Option<&str>) -> Result<()>
pub fn checkout(&self, r#ref: &str) -> Result<()>
pub fn list_branches(&self) -> Result<Vec<BranchInfo>>  // filters iter_refs() to `refs/heads/*`; BranchInfo { name, head: Hash, is_current: bool }

// Dry-run for schema iteration
// Dry-run of an ingest against a *user file*. When `proposed_schema` is Some,
// that schema body is used in place of the committed one for the duration of
// the call — this is how an LLM iterates on a schema edit before staging it.
// Nothing is staged and nothing is committed. To validate a schema in isolation
// (without ingesting a file), POST the schema to `/files` — the staging-time
// schema validator runs there and rejects bad schemas before they enter the
// working tree.
pub fn test_ingest(&self, path: &str, bytes: &[u8], user_fields: Option<Fields>, proposed_schema: Option<&[u8]>) -> Result<Manifest>
```

Integration tests target `omp_core::api` directly. HTTP and CLI layers are smoke-tested but not heavily unit-tested — they have no logic worth duplicating.

## HTTP API

An `axum` app in `crates/omp-server/src/main.rs` mounts these routes. Each route is ~3 lines: parse request → call `omp_core::api` → serialize response. `tokio` handles per-request concurrency; multiple clients can hit read-only endpoints in parallel without contention. In v1, a single process-scoped writer lock serializes all writes (see "Concurrency" below). In iteration 2, that lock becomes tenant-scoped (see [`11-multi-tenancy.md`](./11-multi-tenancy.md)).

### Repo & status

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/init` | Initialize a repo in the current working directory (or at a configured path). Mostly useful for tests; humans will use the CLI. |
| `GET`  | `/status` | Staged changes, current branch, HEAD commit. |

### Files

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/files` | Upload a file. Body is multipart: `path`, `file`, and optional `fields[...]` for user-provided manifest fields, optional `file_type` override. Stages the change. Returns the produced manifest for user-file paths, or a blob reference (`{hash, size}`) for schema and `omp.toml` uploads. |
| `GET`  | `/files` | **Flat** recursive listing of every user-file (manifest-mode) path in the tree — one entry per leaf, no directory structure. Each entry is `{path, manifest_hash, source_hash, file_type}`. Supports `?at=<ref>` and `?prefix=<path>` to filter by path prefix. Use `/tree` for hierarchical directory browsing; use `/files` when you want "give me the files." Schemas, probes, and `omp.toml` are blobs, not files, and are *not* returned by this endpoint. |
| `GET`  | `/files/{path:path}` | Fetch the manifest for a single file. Accepts `?at=<ref>`. |
| `GET`  | `/files/{path:path}/bytes` | Fetch the raw file bytes. Accepts `?at=<ref>`. |
| `PATCH`| `/files/{path:path}/fields` | Update one or more user-provided fields on an existing file. Body is a JSON object. Stages a new manifest version. Errors with `invalid_path` if the target path resolves to a blob (schemas, `omp.toml`) rather than a manifest. |
| `DELETE`| `/files/{path:path}` | Stage a removal. |

### Tree (directory listing)

| Method | Path | Description |
|--------|------|-------------|
| `GET`  | `/tree` | List the root tree's immediate entries (one level only). Accepts `?at=<ref>`. |
| `GET`  | `/tree/{path:path}` | List a subdirectory's immediate entries. Returns entries with `{mode, hash, name}` where `mode ∈ {blob, manifest, tree}`. Accepts `?at=<ref>` and `?recursive=true` for a full subtree walk. |

`omp_core::api::show` returns a `ShowResult` enum (`Manifest | Blob | Tree`); the HTTP surface splits that across `/files/{path}` (manifest), `/files/{path}/bytes` (blob content), and `/tree/{path}` (directory). Callers that don't already know the mode can issue a `GET /tree/{parent_path}` and read the target entry's `mode` field — the extra round-trip is the cost of keeping each HTTP endpoint's response shape homogeneous (rather than a polymorphic `/show` that returns three different bodies). The `--remote` CLI does this round-trip transparently. Direct (in-process) CLI calls hit `omp_core::api` and match on the enum directly.

### Commits & history

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/commit` | Commit staged changes. Body: `{message, author?}`. Returns `{hash, reprobed?}`. The optional `reprobed` array is present when the commit included a `schemas/<X>.schema` change that auto-rebuilt existing manifests of that file_type — see [`21-schema-reprobe.md`](./21-schema-reprobe.md). Each entry has `{file_type, count, skipped: [{path, reason}]}`; per-file failures don't block the commit. |
| `GET`  | `/log` | Commit history. Supports `?path=<p>&max=<n>`. |
| `GET`  | `/diff` | Structured diff between two refs. `?from=<a>&to=<b>&path=<p>`. |

### Branches

| Method | Path | Description |
|--------|------|-------------|
| `GET`  | `/branches` | List branches. |
| `POST` | `/branches` | Create a branch. Body: `{name, start?}`. |
| `POST` | `/checkout` | Switch HEAD to a ref. Body: `{ref}`. |

### Dry-run

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/test/ingest` | Dry-run an ingest. Multipart: `path`, `file`, optional `fields[...]`, optional `proposed_schema` (schema file contents to use in place of the committed one). Returns the manifest that *would* be produced. Stages nothing. |

## CLI

A `clap`-based CLI in `crates/omp-cli/src/main.rs`. Every command wraps one `omp_core::api` function. By default, commands call `omp_core::api` directly (no running server needed). A `--remote <url>` flag makes the CLI a thin HTTP client instead.

```
omp init
omp status

omp add <path> [--field k=v ...] [--type <file_type>]   # CLI reads bytes from <path> on the local filesystem, then calls api.add(path, bytes_, ...)
omp show <path> [--at <ref>]
omp cat <path> [--at <ref>]                  # alias for bytes
omp ls [<path>] [--at <ref>] [--recursive]
omp patch-fields <path> --field k=v [...]
omp rm <path>

omp commit -m <message>
omp log [<path>] [--max <n>]
omp diff <ref1>..<ref2> [<path>]

omp branch [<name> [<start>]]
omp checkout <ref>

omp test-ingest <path> [--field k=v ...] [--proposed-schema <path-to-schema-file>]

omp serve [--bind <addr>]                    # runs the HTTP server; bind address defaults to [server].bind from .omp/local.toml (or OMP_SERVER_BIND), see 07-config.md
```

### CLI niceties

- **`omp show` output** for a manifest renders the TOML pretty-printed with key fields highlighted. For a blob, it prints the raw content (or a hex dump if binary). For a tree, it runs `ls`.
- **`omp log`** defaults to the last 50 commits, compact single-line format (hash abbrev, author, time, first line of message).
- **`omp diff`** for a manifest shows field-by-field changes. For a blob, a unified text diff (or a byte-diff summary if binary).

## Error shape

All HTTP errors return JSON:

```json
{
  "error": {
    "code": "schema_validation_failed",
    "message": "Field 'title' must be string, got int",
    "details": {"path": "schemas/pdf.schema", "field": "title"}
  }
}
```

Stable error codes (v1 starter set, extensible):

- `not_found` — path, commit, or ref doesn't exist
- `schema_validation_failed` — uploaded schema is syntactically wrong
- `ingest_validation_failed` — file + fields don't satisfy the schema (missing required, wrong type)
- `probe_failed` — a probe raised or returned an unusable value
- `conflict` — attempted a change that collides with staged state
- `invalid_path` — path contains `/` in a component, or is empty, etc.

Iteration 2 adds `unauthorized` (HTTP 401, introduced with the tenant layer) and `quota_exceeded` (HTTP 429). See [`11-multi-tenancy.md`](./11-multi-tenancy.md). These are additive — v1 clients never observe them because v1 has no auth or quota layer.

CLI errors get the same structure plus a nonzero exit code.

## Authentication and tenancy

None in v1. v1 runs as a single-tenant local service, matching Git on a laptop. Auth and multi-tenancy are the main job of iteration 2 — that's the scale/maintainability story the hosted version rests on. See [`11-multi-tenancy.md`](./11-multi-tenancy.md) for the design of the tenant boundary, the `Authorization` header shape, and the per-tenant namespace layered over `ObjectStore`.

## Concurrency

v1 is **single-tenant, single-writer**. All write requests (`POST /files`, `PATCH`, `DELETE`, `POST /commit`, `POST /branches`, `POST /checkout`) are serialized by a single in-process `tokio::sync::Mutex`, plus a cross-process `fs2::FileExt::lock_exclusive` held on `.omp/refs.lock` for the duration of each commit to keep a second `omp` process on the same repo from racing. Reads are lock-free once an object is on disk and can proceed in parallel.

Iteration 2 promotes the in-process mutex to a **per-tenant** mutex (one `tokio::sync::Mutex` per `TenantId`), so different tenants' commits don't contend. See [`11-multi-tenancy.md`](./11-multi-tenancy.md). Multi-writer support within a single tenant (with proper merge resolution) remains a future concern; see [`09-roadmap.md`](./09-roadmap.md).

## Why one server + one CLI + one `omp_core::api`

The triplication (same ops on HTTP, CLI, Rust API) exists because the three audiences are different:

- **LLM agents** invoke over HTTP. They want JSON in, JSON out, no weird shell semantics.
- **Humans** use the CLI. They want terse commands, readable output, shell composition.
- **Tests** call `omp_core::api` directly. They want no network, no subprocess, full control over state.

Keeping logic only in `omp_core::api` means all three paths are equivalent and none can drift. It also means swapping to a different transport later (gRPC, MCP) is adding one more thin adapter.
