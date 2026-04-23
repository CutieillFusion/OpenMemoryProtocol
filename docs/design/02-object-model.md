# 02 — Object model

OMP has four content-addressable object types. Each is stored identically on disk: zlib-compressed bytes of `<type> <size>\0<content>`, hashed by SHA-256 of the canonical `<type> <size>\0<content>` bytes. This is exactly Git's object-framing convention with SHA-256 instead of SHA-1.

> A fifth type, `chunks`, is defined in [`12-large-files.md`](./12-large-files.md) to support per-file sizes up to 200 GB. It uses identical framing (`chunks <size>\0<content>`, zlib-compressed, SHA-256 over framed bytes) and does not change any of the four types below.

## Storage framing

Every object in `.omp/objects/` is:

```
zlib_compress("<type> <size>\0<content_bytes>")
```

- `<type>` is `blob`, `tree`, `manifest`, or `commit` (ASCII).
- `<size>` is the decimal length in bytes of `<content_bytes>`.
- `\0` is a single null byte.
- `<content_bytes>` is the type-specific payload — defined below.

The hash is `sha256(f"{type} {size}\0" + content_bytes)` (pre-compression). Filename in the object store is `<first 2 hex chars>/<remaining 62 hex chars>`.

## `blob`

Raw file bytes, opaque.

- **Content:** exactly the file's bytes.
- **Hash:** of the framed form. (The bytes' own sha256 is ALSO exposed separately in any manifest that points at this blob, under `source_hash` — see below.)
- **When used:** schema files (`schemas/*.schema`), `omp.toml`, and any file OMP treats as direct content without generating a manifest around it. Also, every manifest's `source_hash` field points at a blob.

## `tree`

A hierarchical map from names to objects. Plain text, UTF-8, LF-terminated. One entry per line:

```
<mode> <hash>\t<name>
```

Where `<mode>` ∈ {`blob`, `manifest`, `tree`} and `<name>` is a single path component (no slashes).

Example:

```
manifest  ab12cd34...  earnings-q3.pdf
blob      ef56ab78...  omp.toml
tree      1234abcd...  schemas
tree      5678efab...  media-example1
```

Sorted by `<name>` lexicographically so the canonical serialization is unique. Parsing is a simple split-per-line, split-on-tab.

Note: the example rows above render the tab as padding spaces for readability. On disk, the separator between `<hash>` and `<name>` is a single literal tab (`\t`); mode and hash are separated by one space.

**Why plain text?** Git uses a binary tree format for compactness; we don't. OMP optimizes for inspectability — you can `zlib -d` an OMP tree object and read it. Size cost is minor because trees aren't a bottleneck; individual entries are only ~80 bytes anyway.

### Tree entry modes

Three modes control how a tree entry is interpreted:

- **`blob`** — the entry's hash references raw file bytes. Used for schema files, probe modules (`probes/**/*.wasm`), probe manifests (`probes/**/*.probe.toml`), and `omp.toml`. OMP reads these itself, as configuration.
- **`manifest`** — the entry's hash references a `manifest` object, which in turn points at a `blob` via `source_hash`. Used for user files.
- **`tree`** — the entry's hash references another `tree` object (a subdirectory).

## `manifest`

TOML describing one user file. Every user file in a tree points (via a `manifest` entry) at a manifest; the manifest points at the raw bytes via `source_hash`.

```toml
source_hash = "9f3a2b1e..."         # hex sha256 of the blob's raw bytes (same value as the file.sha256 probe; distinct from the framed-object hash used in the tree)
file_type = "pdf"                   # which schema produced this manifest
schema_hash = "77c10e42..."         # hash of the schema object used (for time-travel interpretability)
ingested_at = "2026-04-21T10:14:00Z"
ingester_version = "0.1.0"    # OMP crate version that produced this manifest; informational only, not validated on read

[probe_hashes]
# Framed-object hash (the same hash the tree uses to reference the probe blob)
# of each probe that fired during ingest. Combined with schema_hash, this
# pins the entire extraction bit-for-bit: given a manifest, look up each
# probe's .wasm by this hash via ObjectStore.get and replay.
"pdf.page_count" = "a13b5f9c..."
"pdf.title"      = "d284e710..."
"file.size"      = "b5c01e33..."
"file.sha256"    = "6e77a8ba..."

[fields]
# Everything under [fields] is schema-defined — different file types have different shapes.
title = "Q3 Earnings Report"
page_count = 40
tags = ["finance", "q3", "earnings"]
summary = "Quarterly results covering revenue, costs, and outlook."
```

### Why manifests record `schema_hash` and `probe_hashes`

Schemas and probes are both tree-committed blobs that can change over time. A manifest produced today may not match what the current schema + current probes would produce a month from now. Recording both hashes on every manifest means:

- An old manifest is always interpretable — the schema and probe modules that produced it are reachable by hash.
- When time-traveling, `omp show foo.pdf --at HEAD~5` returns the historical manifest, `omp show schemas/pdf.schema --at HEAD~5` returns the schema that shaped it, and the entries under `[probe_hashes]` name the exact `.wasm` blobs that extracted each field. The extraction is fully reconstructable.

Every v1 manifest includes `probe_hashes` — it is populated by the ingest pipeline in the same step that writes `schema_hash` (see [`04-schemas.md`](./04-schemas.md)). A reader that encounters an older or externally-produced manifest missing the table treats it as empty (the extraction is no longer fully replayable, but the manifest is still interpretable against its `schema_hash`).

### Canonical TOML

Serialization uses an in-tree canonical TOML writer (`crates/omp-core/src/toml_canonical.rs`) built on top of the `toml_edit` crate. The canonicalizer enforces sorted keys, a fixed float format, and LF line endings; read-back-then-rewrite must produce byte-identical output — otherwise round-tripping changes the hash. Property tests on random manifest shapes pin this down.

## `commit`

Plain text in Git's header-then-body format.

```
tree 29ff16c9c14e2652b22f8b78bb08a5a07930c147...
parent 7a8b4e310aefbbb8e5d1e9c1f9a3a3c2d4b4c1f1...
author claude-code <claude@local> 2026-04-21T10:14:00Z

add Q3 earnings report
ingested under schema pdf@77c10e42
```

### Header lines

- **`tree <hash>`** — required. The root tree of this commit.
- **`parent <hash>`** — zero for the root commit, one for normal, two or more for merges. Multiple `parent` lines.
- **`author <name> <<email>> <timestamp>`** — required. Timestamp is ISO 8601 in UTC.

Message follows after a blank line, LF-terminated.

### Canonical serialization

- Headers are LF-terminated.
- Exactly one blank line separates headers from the message.
- Message body is preserved verbatim up to but not including a trailing LF (OMP adds exactly one trailing LF during serialization).
- Parent order is the order of the `parent` lines; callers are responsible for keeping merges' parent order stable.

## Summary table

| Type     | Content format | Used for                                    |
|----------|----------------|---------------------------------------------|
| blob     | raw bytes      | raw file bytes; referenced by manifests, schemas |
| tree     | plain text, `<mode> <hash>\t<name>` per line | hierarchical directory maps |
| manifest | TOML           | per-file LLM metadata                        |
| commit   | plain text, Git-style headers + body | snapshot of the repo state  |

All four are stored with the same `<type> <size>\0<content>` framing and zlib compression.
