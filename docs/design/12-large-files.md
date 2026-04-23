# 12 — Large files (up to 200 GB per file)

v1 assumes every file fits in RAM. `store.put(type, &[u8])` takes a slice, `frame()` concatenates header + content in memory, zlib compresses the full buffer, and probes receive the whole file as one CBOR payload inside a WASM sandbox capped at 64 MB. That's fine for PDFs and markdown. It fails for video archives, dataset dumps, model weights, and any caller who wants OMP to be the durable home for their LLM corpus — the kinds of inputs that pushed Git to invent LFS.

This doc adds an on-ramp for per-file sizes up to **200 GB** without moving any of the five fixed points in [`10-why-no-v2.md`](./10-why-no-v2.md). The approach is a chunked-Merkle object, introduced additively, with streaming everywhere on the write path.

## The shape of the change

A new content-addressable object type, `chunks`, sits alongside the existing four. A large file is stored as a `chunks` object pointing at an ordered list of ordinary `blob` objects. Small files keep the v1 behavior — one `blob`, stored as before. A per-file threshold decides which path an ingest takes.

```
manifest.source_hash  →  chunks object  →  [blob#0, blob#1, ..., blob#N]
                                           each blob ≤ chunk_size_bytes
```

Nothing else in the object model moves. Manifests still carry one `source_hash`; readers dispatch on that hash's object type. Trees, commits, schemas, probes, refs: all unchanged.

## The `chunks` object type

Canonical content is plain text, UTF-8, LF-terminated, one entry per line:

```
<chunk_hash> <length>
```

Where `<chunk_hash>` is the framed-object hash of a `blob` holding that chunk's bytes, and `<length>` is the decimal byte length of that chunk. Entries appear in **file order** (not sorted — order is the content).

Example (abbreviated hashes):

```
a1b2c3...  16777216
d4e5f6...  16777216
7a8b9c...  4215603
```

Framing and storage are identical to every other object type:

```
zlib_compress("chunks <size>\0<content>")
```

Hash is `sha256("chunks <size>\0" + content_bytes)` — pre-compression, exactly as in [`02-object-model.md`](./02-object-model.md). Stored at `.omp/objects/<first 2 hex>/<remaining 62>` like any object.

**Fixed points #1 and #2 preserved.** `chunks` reuses the SHA-256 + Git-style framing scheme verbatim.

### Why plain text, ordered by file offset

Consistent with `tree` being plain text and LF-terminated. The one asymmetry — `tree` sorts by name for canonicalization, `chunks` preserves file order — is inherent: a tree is a set, a chunk list is a sequence. Canonicalization comes from the write order being fixed by the file itself.

### Why a new type instead of overloading `blob`

A `blob` is raw bytes, opaque. A `chunks` object is structure. Overloading `blob` with a "it's actually a pointer list if you squint" mode blurs what readers can assume. Keeping `chunks` distinct means `store.get(hash)` returns a typed object, the tree's entry modes (`blob`, `manifest`, `tree`) don't need to grow a new mode, and backward compatibility is free — old readers that don't know `chunks` refuse loudly rather than mis-parsing.

## The threshold

`omp.toml` gains a `[storage]` section:

```toml
[storage]
chunk_size_bytes = 16777216   # 16 MiB; default when section omitted
```

- Files with `len < chunk_size_bytes` are stored as a single `blob`, unchanged from v1.
- Files with `len >= chunk_size_bytes` are stored as a `chunks` object referencing N blobs of exactly `chunk_size_bytes` each (the last chunk may be shorter).

16 MiB is chosen because it fits comfortably inside the 64 MB WASM memory cap (leaving room for CBOR framing and probe working memory), keeps the object count for a 200 GB file to ~12,500 — manageable for the loose-object store — and is small enough that a network retry during upload costs a few seconds, not hours.

Changing `chunk_size_bytes` after the fact does not rewrite existing objects. Old `chunks` objects remain valid; only newly-ingested files use the new size.

## Streaming ingest

v1's `ObjectStore::put(type, &[u8])` is retained unchanged. Large-file support is an additive trait method:

```rust
pub trait ObjectStore: Send + Sync {
    // ... existing nine methods ...

    /// Stream-in an object from a reader. The backend is responsible for
    /// computing the framed hash while writing compressed bytes to temp
    /// storage, then atomically renaming into place. Returns the object hash.
    fn put_stream(&self, type_: ObjectType, reader: &mut dyn Read, known_size: u64) -> Result<Hash>;
}
```

`known_size` is required because the framing header `<type> <size>\0` must be hashed before content — we can't back-patch it. Callers that don't know the size up front (e.g., stdin) buffer to a temp file first and `fstat` it.

**Fixed point #3 preserved.** `put_stream` is additive; the nine existing methods keep their signatures. Iteration 2 backends (S3, Postgres) implement both.

### Disk backend implementation sketch

The disk backend's `put_stream`:

1. Open `.omp/objects/tmp/<random>` for write.
2. Stream `"<type> <size>\0"` into a SHA-256 hasher and a zlib encoder.
3. Loop: read up to 64 KiB from `reader` → update hasher → feed zlib encoder → write compressed output.
4. On EOF, finalize hasher → hash. Close zlib. Close temp file.
5. Atomically rename temp file to `ab/cdef...`. If a file already exists there (dedup), delete the temp instead.

Peak memory is one read buffer (64 KiB) plus zlib state (~256 KiB). A 200 GB chunk — if you ever wrote one — would not load into RAM.

### Chunked-file ingest

For a file above the threshold, the ingest path in `omp_core::api::add` doesn't hand the whole file to `put_stream`. It chunks first:

1. Open the source (file handle for CLI, assembled upload session for HTTP).
2. Loop: read exactly `chunk_size_bytes` (or remaining) → `store.put(ObjectType::Blob, chunk_bytes)` → record `(chunk_hash, chunk_length)`. Update a running SHA-256 over the raw file content for `file.sha256`.
3. Assemble the `chunks` object body from the recorded pairs.
4. `store.put(ObjectType::Chunks, chunks_body)` → this is the hash written into `manifest.source_hash`.

Each chunk body is small (≤16 MiB) and uses the plain `put`. The `chunks` object itself is tiny (~90 bytes per chunk, so ~1 MB for a 200 GB file) and also uses plain `put`. `put_stream` is there for the backend's benefit (e.g., streaming an individual oversized blob into an S3 backend) but the common ingest path doesn't need it.

## Probes on large files

Probes as specified in [`05-probes.md`](./05-probes.md) cannot accept 200 GB input. The 64 MB default WASM memory cap and the CBOR `{bytes, kwargs}` convention both assume whole-file input. Two additive changes accommodate large files without touching the probe ABI itself.

### 1. `max_input_bytes` in `probe.toml`

Every probe gains one optional field:

```toml
[limits]
memory_mb = 64
fuel = 1_000_000_000
wall_clock_s = 10
max_input_bytes = 33554432   # 32 MiB; default = memory_mb in bytes
```

The ingest engine skips any probe whose declared `max_input_bytes` is smaller than the blob's content length. A skipped probe is observationally identical to a probe that returned null: its schema field resolves via `fallback` if one is declared, or is marked unresolved if not. The manifest's `probe_hashes` simply omits the entry (the probe did not fire).

This keeps the WASM ABI unchanged — the probe itself never sees oversized input, because the engine never calls it. **Fixed point #5 preserved.**

### 2. Streaming built-ins for size and content hash

`file.size` and `file.sha256` are universal, cheap, and don't benefit from running inside WASM on 200 GB of bytes — the sandbox would recompute what the ingest pipeline already knows. Formalize a small **streaming built-ins** table in the engine:

- `file.size` — the content length known at ingest time.
- `file.sha256` — the running SHA-256 maintained during chunked ingest.

When an engine encounters a schema field with `probe = "file.sha256"` (or `file.size`) on a blob larger than the probe's `max_input_bytes`, it uses the ingest-time value instead of invoking the `.wasm`. The result is byte-identical to what the WASM probe would have returned on the same input — the streaming built-ins exist to avoid the sandbox's size limit, not to compute different values.

The probes' `.wasm` blobs still ship in the starter pack. For small files they run normally, and test suites can assert the built-in and the WASM path agree on fixture data. For large files the engine shortcut fires. This is a pure engine change — nothing in the probe ABI, nothing in the `probe.toml` grammar beyond the one field above.

### Probes that don't fit either bucket

A `pdf.page_count` probe that can't ingest a 200 GB PDF just doesn't run. The schema field fallbacks to `null` (or whatever the schema declares). This is the right default: the probe was the price of admission for a capability; the capability doesn't exist at that size and we don't pretend otherwise. Callers that genuinely need per-page metadata on 200 GB PDFs supply it via `user_provided` fields, as [`04-schemas.md`](./04-schemas.md) already specifies for any value OMP cannot derive from bytes.

## HTTP / CLI ingest surface

Large files cannot be a single HTTP request body. Two additions to [`06-api-surface.md`](./06-api-surface.md):

### Resumable upload sessions (HTTP)

```
POST   /uploads                       → {upload_id, chunk_size_bytes}
PATCH  /uploads/{id}?offset=<bytes>   body: the next slice of bytes
POST   /uploads/{id}/commit           body: {path, fields?, file_type?}
DELETE /uploads/{id}                  explicit abort
```

- Session state lives under `.omp/uploads/<upload_id>/` (a content-buffer directory, a small JSON state file, and a running SHA-256 intermediate). Parallel to `.omp/index.json`: un-versioned, ignored by the tree walker, local to this machine.
- `PATCH` is idempotent per offset (client may retry a failed chunk without corrupting the stream).
- `commit` is where the `manifest` + `chunks` object + staging entry are written. It takes the tenant write lock only for its duration (see below).
- The existing `POST /files` stays for small files — single-shot upload remains the common case.

### CLI

```
omp add <path> [--field k=v ...] [--type <file_type>]
```

Unchanged in surface. The CLI reads directly from the local filesystem, streams chunks via the in-process `omp_core::api`, and never goes through the upload-session endpoints. `omp add` on a 200 GB file behaves the same as on a 200 KB file; the only difference is that it's slower and the resulting manifest's `source_hash` points at a `chunks` object.

For CLIs talking to a remote server (`--remote <url>`), the client transparently uses the upload-session endpoints. Users don't see the difference.

## Multi-tenancy interactions

Everything in [`11-multi-tenancy.md`](./11-multi-tenancy.md) still holds. Two adjustments the tenant layer needs in the iteration that ships large files:

- **Quota is checked up front, not per chunk.** `PATCH /uploads/{id}` already knows the session's declared total size (set at `POST /uploads`); the quota check fires once, on session open. A 200 GB upload that would blow the tenant cap fails at byte zero, not at chunk 12,000.
- **The per-tenant write lock is held only during `commit`, not during `PATCH`.** A 200 GB upload can take hours; holding the tenant's single-writer lock for the whole transfer would block every other commit from that tenant. Chunk `PATCH`es write only to `.omp/uploads/<id>/` — a session-local scratch area, not the object store — so they don't need the lock. The `commit` step (which does write to `.omp/objects/` and update refs) takes the lock as usual. Concurrency invariant: at most one in-flight *commit* per tenant, not at most one in-flight *upload*.
- **Orphaned sessions get a TTL.** Sessions older than `[storage] upload_session_ttl_hours` (default 24) are reaped by `omp admin gc`. Prevents abandoned uploads from pinning disk forever.

## Garbage collection becomes load-bearing

A 200 GB file produces ~12,500 loose chunk objects plus one `chunks` object plus one `manifest`. Deleting that file through `DELETE /files/{path}` only unlinks from the tree; the objects remain. For a store holding even a few hundred such files, that matters.

`omp admin gc` — already deferred per [`09-roadmap.md`](./09-roadmap.md) — effectively becomes a prerequisite for advertising large-file support in production. The reachability walk extends naturally:

- Walk commits → trees → manifests → `manifest.source_hash`.
- If the `source_hash` resolves to a `blob`, mark it live (v1 behavior).
- If it resolves to a `chunks` object, mark the `chunks` object live *and* every `<chunk_hash>` in its body.

No other GC change. Chunk objects participate in normal content-addressed dedup: the same 16 MiB chunk appearing in two different files is stored once, and GC reference-counts it correctly by reading each `chunks` list.

## What this doc explicitly does *not* propose

- **No Git-LFS-style pointer-to-external-storage.** Content stays inside the addressable `ObjectStore`. No secondary bucket, no side-channel download URL.
- **No content-defined chunking (FastCDC, rolling hash).** Fixed-size splitting is enough to make 200 GB work. CDC would improve dedup across edits to the same large file and is noted as a future enhancement — it fits inside the same `chunks` object type without any wire-format change (the chunk boundaries become content-dependent, but the list shape is identical).
- **No change to `tree`, `manifest`, or `commit` formats.** Only a new object type is introduced.
- **No change to the probe WASM ABI.** The ABI is untouched; only the engine's decision to invoke a probe changes, plus one optional field in `probe.toml`.
- **No pack files.** Orthogonal to chunking — packs are about loose-object inefficiency, which shows up at millions of objects, not thousands of 16 MiB chunks. Packs remain anticipated future work per [`10-why-no-v2.md`](./10-why-no-v2.md).
- **No partial-read streaming on the read side.** `bytes_of` still returns full file bytes. A streaming read endpoint (`GET /files/{path}/bytes?range=...`) is an easy follow-up but out of scope here.

## Fixed-points audit

Against [`10-why-no-v2.md`](./10-why-no-v2.md):

1. **SHA-256 of canonical wire bytes.** `chunks` objects hash exactly like every other object: `sha256("chunks <size>\0" + content)`. Chunk blobs hash as v1 blobs. Unchanged.
2. **Git-style object framing.** `chunks` uses `<type> <size>\0<content>` with zlib compression. Unchanged.
3. **`ObjectStore` as the single storage contract.** `put_stream` is additive. The existing nine methods keep their signatures; all existing callers keep working. Backends that can't stream (in-memory test backend) can implement `put_stream` as `read_to_end` + `put` without regression.
4. **Four field sources + fallback wrapper.** Unchanged. Skipped probes behave exactly like probes returning null, which `fallback` already handles.
5. **WASM probe ABI.** Unchanged. Three exports, zero imports, CBOR in/out. The engine's decision about whether to *call* a probe is out-of-band — the probe module itself is identical.

Adding large-file support is additive on every axis. Removing the feature reverts to pure v1 behavior: drop the `chunks` object type, the `put_stream` method, and the `max_input_bytes` field, and nothing else moves.

## Deferred

- **Content-defined chunking** (FastCDC) to improve dedup across edits to the same large file.
- **Partial reads** — `GET /files/{path}/bytes?range=<a>-<b>`.
- **Streaming probe ABI** — a revised ABI where probes receive a handle and call back for byte ranges. Only worth it if we find a non-trivial probe whose work is genuinely incremental over chunk boundaries (e.g., line-counting a 200 GB log file). Not in this iteration.
- **Parallel chunk upload** — the `PATCH /uploads/{id}` surface allows it in principle; the v1-of-this-feature implementation serializes writes to keep the disk-backend code simple. Iteration 2.
