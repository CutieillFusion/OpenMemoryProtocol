# 04 — Schemas

Schemas are the core of OMP's extensibility. A schema defines what a manifest for a given file type should look like — which fields it has, how each field is computed, and what type each field must be. Schemas are **plain TOML files in the repo's tree**, at `schemas/<file_type>.schema`. Adding a new file type to OMP means committing a new schema file. No code change, no restart, no endpoint beyond `POST /files`.

## Where schemas live

A schema lives at `schemas/<file_type>.schema` at the root of the repo's working tree. Example: `schemas/pdf.schema`, `schemas/audio.schema`, `schemas/my-custom-type.schema`.

Schemas are committed like any other file — they show up in the root tree with mode `blob`, are versioned, and can be time-traveled. A repo at commit X has exactly the set of schemas defined in its tree at commit X; older commits had older schemas; branch B may have different schemas than branch A.

## Schema TOML structure

```toml
file_type = "pdf"                      # required: matches filename before .schema
mime_patterns = ["application/pdf"]    # required: list of MIME type globs; used for auto-detection

# Optional: declare whether this schema is strict about unknown fields in ingest requests
allow_extra_fields = false

# Each [fields.<name>] block declares one manifest field.

[fields.title]
source = "user_provided"
type = "string"
required = false
  [fields.title.fallback]
  source = "probe"
  probe = "file.name"

[fields.page_count]
source = "probe"
probe = "pdf.page_count"
type = "int"

[fields.tags]
source = "user_provided"
type = "list[string]"
required = false

[fields.summary]
source = "user_provided"
type = "string"
required = false
description = "Short paragraph describing the document's content."

[fields.computed_slug]
source = "field"
from = "title"
transform = "slugify"
type = "string"
```

## Top-level fields

- **`file_type`** (required, string) — must match the basename of the schema file (i.e., `schemas/pdf.schema` must declare `file_type = "pdf"`).
- **`mime_patterns`** (required, list of strings) — MIME type patterns for auto-detection. When a user uploads a file without specifying `--type`, OMP inspects the file's MIME type and picks the first schema whose `mime_patterns` matches.
- **`allow_extra_fields`** (optional, bool, default `false`) — if true, caller-provided fields not declared in the schema are stored verbatim under `[fields]`. If false, unknown fields are rejected.
- **`[render]`** (optional table) — UI render hint. Describes how the web frontend should display the file's bytes when a manifest of this file_type is opened. Schema-level (not per-manifest) so the choice travels with the file_type and time-travels with the schema. See "[Render hints](#render-hints)" below.

## Render hints

Web clients need to know whether to fetch and render a file's bytes inline (text, image, markdown) or to surface them as a download (binary). The schema is the right place to declare that, since the file_type already drives every other UI choice and the answer doesn't vary per-manifest.

```toml
[render]
kind = "markdown"           # text | hex | image | markdown | binary | none
max_inline_bytes = 65536    # optional cap; default 64 KiB
```

Recognized kinds:

- **`text`** — UTF-8 decode and render as monospace with line numbers.
- **`hex`** — render as a hex+ASCII dump.
- **`image`** — fetch as a blob and render via `<img>`. Auth-aware (the UI uses a `blob:` URL so the bearer token is attached to the byte fetch).
- **`markdown`** — UTF-8 decode, render with a sanitized markdown pipeline.
- **`binary`** — no inline preview; show a download button. Default for unknown file types.
- **`none`** — no preview at all (use for opaque/encrypted blobs where displaying anything is misleading).

When `[render]` is omitted, OMP infers a reasonable kind from `mime_patterns[0]`: `image/*` → `image`, `text/markdown` → `markdown`, other `text/*` and `application/json|toml|yaml|xml` → `text`, everything else → `binary`. Files with no schema (the `_minimal` schema, or the `allow_blob_fallback` path) resolve to `binary`.

The render hint is surfaced on `GET /files/{path}` responses as a top-level `render` field. Time-travel works as expected — `?at=<old_commit>` returns the render hint from the schema as it existed at that commit, not from HEAD.

Forward compatibility: an unknown `kind` value (one written by a newer OMP than the reader knows about) resolves to `binary` rather than failing the schema parse, so older clients keep working.

## Field declarations

Each `[fields.<field_name>]` block declares one manifest field. Required subkeys vary by `source`, but every field has:

- **`source`** (required) — one of `constant`, `probe`, `user_provided`, `field` (detailed below). `fallback` is *not* a valid `source` value; it is a nested table that wraps one of the four sources — see "The fallback wrapper" below.
- **`type`** (required) — the expected value type (detailed below).
- **`required`** (optional, default `false`) — if `true`, the field must be populated; otherwise ingest fails.
- **`description`** (optional) — human-readable docs for the field, useful for LLMs deciding what to supply.
- **`fallback`** (optional) — a nested table with its own `source` and args, used if the primary source returns null or errors.

## Field sources (closed set of four, plus a fallback wrapper)

Every field is computed by exactly one source, optionally wrapped in a `fallback` for graceful degradation. New sources in v2 would be a breaking change — but v1's four are expressive enough that we don't expect to need more.

### 1. `constant`

A literal value embedded in the schema.

```toml
[fields.app_version]
source = "constant"
value = "0.1.0"
type = "string"
```

Useful for stamping manifests with fixed metadata.

### 2. `probe`

Call a named probe — a WebAssembly module committed to the tree under `probes/<namespace>/<name>.wasm`. Probes are sandboxed, pure, and deterministic by construction (no imports, no syscalls). See [`05-probes.md`](./05-probes.md) for the ABI, sandbox configuration, and the starter-pack probe list.

```toml
[fields.byte_size]
source = "probe"
probe = "file.size"
type = "int"

[fields.text_preview]
source = "probe"
probe = "text.first_lines"
args = { n = 5 }
type = "string"
```

`args` is an optional table of keyword arguments passed to the probe.

### 3. `user_provided`

A value supplied by the caller in the ingest request. OMP validates the value against `type` and stores it. If the field is `required = true` and no value is provided (and no `fallback` yields one), ingest fails.

```toml
[fields.summary]
source = "user_provided"
type = "string"
required = false
```

This is how OMP handles LLM-generated content: the LLM (running elsewhere) computes a summary and submits it. OMP stores it.

### 4. `field`

Reference another field in the same schema. Useful for derived fields. v1 ships three transforms: `identity`, `slugify`, and `lower`.

```toml
[fields.slug]
source = "field"
from = "title"
transform = "slugify"
type = "string"
```

Resolution order is topological — OMP sorts fields so referenced fields resolve before dependent ones. Cycles are detected at schema load time and rejected.

## The fallback wrapper

Not a source on its own — a *wrapper* attached to any of the four sources above. If the primary source returns null (e.g., `user_provided` with nothing provided, or a probe that returned null), the fallback's source is used.

```toml
[fields.title]
source = "user_provided"
type = "string"
  [fields.title.fallback]
  source = "probe"
  probe = "file.name"
```

Fallbacks can be nested — a fallback can itself have a fallback. Not a common pattern, but supported.

## Supported `type` values

Types are string literals. The closed set:

- `string`
- `int`
- `float`
- `bool`
- `datetime` (ISO 8601 string)
- `list[string]`, `list[int]`, `list[float]`, `list[bool]`, `list[datetime]`
- `object` (arbitrary TOML-compatible nested structure; OMP validates it's a table, no deeper type check)

Tightly bounded. Adding a new type in a future version is safe — old schemas don't break because they don't mention the new type.

## The ingest pipeline

When OMP ingests a file:

1. Determine file type: explicit `--type` argument wins; otherwise match the file's MIME type against every loaded schema's `mime_patterns`. Patterns are fnmatch-style globs over the MIME string, case-insensitive. Schemas are consulted alphabetically by `file_type` and the first match wins; ties are therefore deterministic. If no schema matches, consult `omp.toml`'s `[ingest]` settings: `default_schema_policy = "reject"` fails the ingest with a clear error; `default_schema_policy = "minimal"` falls through to the built-in `_minimal` schema (file_type = `"_minimal"`, schema_hash = the hash of a fixed canonical TOML shipped with OMP) which declares only the `file.*` probes; `allow_blob_fallback = true` additionally permits storing the file as a plain blob with no manifest.
2. Load the matching schema from the current tree (`schemas/<file_type>.schema`).
3. Topologically sort the schema's fields by dependency.
4. Resolve each field in order:
   - `constant` → literal value.
   - `probe` → call the probe with the file's bytes and args.
   - `user_provided` → look up in the caller's request.
   - `field` → look up already-resolved fields, apply transform.
   - If the source returns null and a `fallback` is declared, switch to the fallback's source and try again.
5. Validate each resolved value against its declared `type`.
6. Verify every `required = true` field has a non-null value; if any fail, raise an error with the list of missing fields.
7. Build the manifest: `source_hash`, `file_type`, `schema_hash`, `ingested_at`, `ingester_version`, the `[probe_hashes]` table (framed-object hash of every probe whose `probe_run` was invoked during step 4 — including probes reached only via `fallback`), plus the `[fields]` table.
8. Store the blob, store the manifest, return both hashes.

The manifest's `schema_hash` records exactly which schema object was used — so if that schema is edited later, the old manifest still points at the old schema for interpretability.

## Validation at write time

When a caller `POST`s a schema file (just another `POST /files` to a path under `schemas/`), OMP validates it *before* staging:

- Top-level `file_type` matches filename.
- Every field's `source` is one of the four allowed values (`constant`, `probe`, `user_provided`, `field`); `fallback`, if present, is a nested table whose own `source` is likewise one of the four.
- Every `probe` reference names a probe present at `probes/<namespace>/<name>.wasm` + `probes/<namespace>/<name>.probe.toml` in the current tree. Probe resolution time-travels automatically — at `--at <commit>`, the probes are whatever was in `probes/` at that commit.
- Every `field` reference names another field in the same schema.
- No dependency cycles.
- Every `type` is in the supported set.
- `mime_patterns` is a valid list of MIME glob patterns.

Invalid schemas are rejected with a specific error pointing at the first problem. This is what makes LLM-authored schemas safe — a syntactically correct but logically broken schema doesn't silently corrupt the repo.

## Editing a schema safely

The intended workflow for an LLM modifying a schema:

1. Fetch the current schema: `GET /files/schemas/pdf.schema`.
2. Produce a modified version.
3. **Dry-run**: `POST /test/ingest` with the proposed new schema and a sample file. OMP validates the schema, runs the engine, and returns the manifest that *would* be produced — without staging anything.
4. If the result looks right, `POST /files path=schemas/pdf.schema file=@new.schema` to stage the schema change.
5. `POST /commit` — the schema change AND rebuilt manifests for every existing file of that file_type land in **one atomic commit**. See [`21-schema-reprobe.md`](./21-schema-reprobe.md) for the auto-reprobe semantics: field-level reuse copies unchanged fields verbatim from the old manifest, only fields whose `Source` actually changed get re-derived. The commit response includes a `reprobed` summary listing per-file_type counts and any per-file failures (one bad file does not block the commit).

The dry-run step is the safety valve. It means an LLM can iterate on schema designs freely without ever leaving the repo in a broken state.

### A note on schema formatting and `schema_hash`

Schema files are stored as **blobs** — their hash is over the exact bytes the caller uploaded, not over a canonical TOML form. Reformatting a schema (reordering keys, changing quote style, adjusting whitespace) produces a new blob with a new `schema_hash`, even if the schema is logically identical. Every manifest ingested after that point will reference the new hash. This is acceptable — time-travel is by hash, and both hashes remain in the object store — but LLMs editing schemas should preserve formatting when no logical change is intended, to avoid churning `schema_hash` across every downstream manifest. The same applies to `probes/**/*.probe.toml` and `omp.toml`.

## Why this shape is the right one

**Separation of concerns.** A schema answers "what should a manifest look like?" without encoding "how to call an LLM." The four sources are computational primitives; none of them require OMP to know anything about the outside world except file bytes and the probe modules committed under `probes/` in the same tree.

**Data-not-code.** Everything about a schema is TOML. An LLM reading and writing TOML is a solved problem. An LLM modifying source code in any host language is not.

**Bounded extensibility.** Probes are WASM modules committed to the tree alongside schemas. If a schema can't be expressed via the four sources and the available probes, the gap is a new probe committed to `probes/`, not a rewrite of OMP.

**Time-travel is free.** Because schemas are in the tree, they're versioned. Because every manifest records its `schema_hash`, old manifests are always interpretable. The "what did this LLM believe, and under what schema" question is one lookup.
