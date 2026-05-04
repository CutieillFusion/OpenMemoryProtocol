# 26 — Default schemas + probes for common LLM input file types

Today, `init_tenant` seeds a single starter schema (`text`) and three universal probes (`file.size`, `file.mime`, `file.sha256`). Anyone uploading a PDF, image, audio file, video, or even a markdown file gets the catch-all path: a manifest with the three universal fields, no per-type structural metadata, and no per-type render hint. [`09-roadmap.md`](./09-roadmap.md) flagged "image / audio probes" as iteration-2 work but never spelled out the *schemas* those probes feed into. This doc fills that gap.

The deliverable is two things, both landing as code:

1. **Eight default per-format schemas** in `crates/omp-core/starter-schemas/`: `text`, `markdown`, `png`, `jpeg`, `mp3`, `wav`, `mp4`, `pdf`. Each schema declares the three universal probe fields, its format-specific structural fields where a probe exists for them, and a `[render]` hint matched to the format. **No `user_provided` fields** — the defaults describe what each file *is*, not what an agent says about it. A tenant that wants `title`/`summary`/`tags` slots can fork a schema; `text` and `markdown` set `allow_extra_fields = true` so extras pass through without forcing every schema to declare them.
2. **Six new structural probes** as compiled WASM in the starter pack: `text.line_count`, `image.dimensions`, `audio.duration_seconds`, `video.duration_seconds`, `video.dimensions`, `pdf.page_count`. Each is a small pure-Rust crate under `probes-src/`, cross-compiled to `wasm32-unknown-unknown` by `scripts/build-probes.sh`, and seeded into `probes/<ns>/<basename>/{probe.wasm,probe.toml}` at `init_tenant` time alongside the existing `file.*` probes.

Format-specific probes that need heavier decoders (image color-mode, audio sample-rate / channels, video codec, PDF text extraction, markdown outline) are explicitly out of scope here — they need a separate doc to handle the WASM-build, decoder-vendoring, and limit-tuning story.

## What does not change

- The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md). The new probes use the existing WASM ABI; the new schemas use the existing four field sources and type set.
- The schema TOML spec from [`04-schemas.md`](./04-schemas.md). Including the write-time `validate_probe_refs` check (`crates/omp-core/src/schema.rs::398`) — schemas can only reference probes that are present in the tree, which is why the four new probes ship *with* the schemas that consume them in this same iteration.
- The per-schema folder layout from [`25-schema-marketplace.md`](./25-schema-marketplace.md): `schemas/<file_type>/schema.toml`.
- The probe ABI from [`05-probes.md`](./05-probes.md). All four new probes export `alloc` / `free` / `probe_run`, take CBOR `{bytes, kwargs}` input, return a CBOR value or null. Same sandbox limits.
- The dry-run + auto-reprobe flow from [`21-schema-reprobe.md`](./21-schema-reprobe.md). When a schema is recommitted to add fields, every existing manifest of that file_type is rebuilt atomically.

## The eight default schemas

| `file_type` | `mime_patterns`                          | Format-specific structural fields              | `[render].kind` | `allow_extra_fields` |
|-------------|------------------------------------------|------------------------------------------------|-----------------|----------------------|
| `text`      | `text/*`                                 | `line_count` (`text.line_count`)               | `text`          | true                 |
| `markdown`  | `text/markdown`, `text/x-markdown`       | `line_count` (`text.line_count`)               | `markdown`      | true                 |
| `png`       | `image/png`                              | `dimensions` (`image.dimensions`)              | `image`         | false                |
| `jpeg`      | `image/jpeg`, `image/jpg`                | `dimensions` (`image.dimensions`)              | `image`         | false                |
| `mp3`       | `audio/mpeg`, `audio/mp3`                | `duration_seconds` (`audio.duration_seconds`)  | `binary`        | false                |
| `wav`       | `audio/wav`, `audio/x-wav`, `audio/wave` | `duration_seconds` (`audio.duration_seconds`)  | `binary`        | false                |
| `mp4`       | `video/mp4`                              | `duration_seconds` (`video.duration_seconds`), `dimensions` (`video.dimensions`) | `binary`        | false                |
| `pdf`       | `application/pdf`                        | `page_count` (`pdf.page_count`)                | `binary`        | false                |

Every schema includes `byte_size` (`file.size`), `sha256` (`file.sha256`), and `mime` (`file.mime`). Those rows aren't repeated in the table.

`text` and `markdown` set `allow_extra_fields = true` because text and markdown documents are the most likely vehicle for LLM-supplied annotation (title, summary, callouts, frontmatter-derived data). The other six schemas stay strict; their formats don't have a culture of attached free-form metadata that's missing today, and a tenant that wants slots can fork the schema explicitly.

`mp4` reads `moov.mvhd` for duration and the video-track `tkhd` for dimensions via a small inline ISO-BMFF box parser shared between the two video probes (see below).

### Representative TOML — `pdf`

```toml
file_type = "pdf"
mime_patterns = ["application/pdf"]

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

[fields.page_count]
source = "probe"
probe = "pdf.page_count"
type = "int"

[render]
kind = "binary"
```

The other seven follow the same shape with `mime_patterns`, the format-specific field rows, and `[render].kind` swapped. Full TOML lives in `crates/omp-core/starter-schemas/<file_type>/schema.toml`.

## The six new probes

Each is a small `cdylib` crate under `probes-src/`, depending on `probe-common` for the ABI shims and on a focused decoder library where one is needed. Built into the `release` profile of `probes-src/`'s workspace, then staged at `crates/omp-core/build/wasm/<name>.wasm` and `include_bytes!`-baked into `omp-core` at compile time. Same flow as the existing `file.*` probes.

### `text.line_count`

```toml
name = "text.line_count"
returns = "int"
accepts_kwargs = []
description = "Number of lines in a text or markdown document. Counts `\\n` separators; a non-newline-terminated final line still counts."

[limits]
max_input_bytes = 33554432   # 32 MiB
```

Implementation: 12 lines of Rust, no dependencies beyond `probe-common`. Counts `\n` bytes and adds 1 if the input doesn't end with one.

### `image.dimensions`

```toml
name = "image.dimensions"
returns = "object"
accepts_kwargs = []
description = "Pixel dimensions of a raster image: {width: int, height: int}. Header sniff via `imagesize`; supports PNG, JPEG, and other common formats. Returns null if the bytes can't be parsed."

[limits]
max_input_bytes = 67108864   # 64 MiB
```

Implementation: delegates to the `imagesize` crate (pure Rust, no_std-friendly, reads only the header). Both PNG and JPEG variants of the schema reference this single probe.

### `audio.duration_seconds`

```toml
name = "audio.duration_seconds"
returns = "float"
accepts_kwargs = []
description = "Duration in seconds for WAV (RIFF/fmt+data chunks) and MP3 (frame walk with Xing/Info VBR awareness). Returns null for other formats or unparseable input."

[limits]
max_input_bytes = 268435456   # 256 MiB
```

Implementation:
- **WAV** — inline RIFF parser. Walks chunks looking for `fmt ` (extracts byte rate from offset 8) and `data` (extracts payload size). Duration = `data_size / byte_rate`.
- **MP3** — skips an ID3v2 header if present (synchsafe size at bytes 6..10), then walks frame headers from the first sync word. Each frame's duration is `samples_per_frame / sample_rate`, looked up from MPEG-version-and-layer tables. If the first frame contains a Xing/Info VBR header with a `frames` count, that's used directly; otherwise per-frame durations are summed.
- Returns null if neither parser succeeds, so the probe is safe to call against any input.

About 220 lines of Rust, zero non-`probe-common` dependencies (the MP3 frame-header tables are small enough to inline).

### `video.duration_seconds`

```toml
name = "video.duration_seconds"
returns = "float"
accepts_kwargs = []
description = "Movie duration in seconds for MP4 (ISO BMFF). Reads `moov.mvhd` and divides duration by timescale. Returns null for non-MP4 input or if `moov`/`mvhd` is missing."

[limits]
max_input_bytes = 1073741824
```

### `video.dimensions`

```toml
name = "video.dimensions"
returns = "object"
accepts_kwargs = []
description = "Pixel dimensions of the first video track in an MP4 (ISO BMFF) file: {width: int, height: int}. Reads the video track's `tkhd` (16.16 fixed-point); selects the track whose `mdia.hdlr` handler_type is `vide`."

[limits]
max_input_bytes = 1073741824
```

Implementation: both share a small ISO-BMFF box reader vendored in `probes-src/probe-mp4/` (an `rlib` workspace member used by both `cdylib` probe crates). The reader walks top-level boxes by `[size: u32 BE][type: 4 ASCII]` headers, descends into `moov`, reads `mvhd` for the timescale + duration, and iterates `trak` boxes selecting the first whose `mdia.hdlr` reports `handler_type = "vide"` for `tkhd` width/height. Both 32-bit (mvhd v0) and 64-bit (mvhd v1) variants are handled; box size encoding 0 (to-end-of-file) and 1 (extended 64-bit size) are both honored.

### `pdf.page_count`

```toml
name = "pdf.page_count"
returns = "int"
accepts_kwargs = []
description = "Best-effort page count via byte scan for `/Type /Page` markers. Handles uncompressed cross-reference tables; PDFs that store all page nodes inside compressed object streams return null."

[limits]
max_input_bytes = 268435456
```

Implementation: byte scan that counts `/Type /Page` occurrences (with `/Pages`, `/PageLayout`, etc. excluded by checking the byte after `/Page` is a PDF-name terminator). Returns null if the file doesn't start with `%PDF-` or no markers are found, so a confidently-wrong "0" is impossible. This is deliberately a "good enough on the common case" probe — a real PDF parser (e.g., `lopdf`) would handle compressed object streams but pulls in a much larger dependency. Upgrading to a structural parser is a follow-up if `null` rates in production warrant it.

## Why per-format and not per-category

It's tempting to define one `image` schema with `mime_patterns = ["image/*"]` and one `audio` schema with `mime_patterns = ["audio/*"]`. Per-format wins on three points:

- **Probe coverage isn't uniform across categories.** `audio.duration_seconds` works for WAV and MP3 today; FLAC, OGG, AAC return null. A flat `audio` schema would have to either advertise a field that's silently absent for half its inputs or omit the field entirely. Per-format schemas honestly say "WAV gets duration, MP3 gets duration" without making promises about formats we don't yet handle.
- **Future format-specific fields differ.** PNG has color-mode (RGB / RGBA / palette / grayscale); JPEG has chroma subsampling. MP3 has bitrate (constant or variable) and ID3 tags; WAV has neither. A per-category schema either accumulates conditional fields or papers over the differences.
- **MIME globs over `image/*` silently match `image/svg+xml` (XML) and `image/heic` (no probe support).** Tight `mime_patterns` keep the auto-detection path predictable.

The cost is duplicating the universal fields across schemas. Schemas are tiny TOML files, so the cost is small.

## Field philosophy: structural only

Every default schema describes **what the file is** — its bytes, its content type, its dimensions or duration. There are no `title`, `summary`, or `tags` user_provided slots in the defaults.

Two reasons:

1. **Honesty.** A schema field with `source = "user_provided"` advertises that someone *should* fill it in. Default schemas can't make that promise on a tenant's behalf — different tenants have different curation models, and a slot that's empty on every manifest is noise.
2. **Forks are cheap.** A tenant that wants those slots commits a one-line edit to the schema. The visual editor from doc 25 makes this a Save action. The default doesn't have to anticipate every workflow.

`text` and `markdown` are the exception: they set `allow_extra_fields = true` so a caller can attach annotations (title, alt, frontmatter-derived keys) without an explicit fork. Plain text and markdown are the formats most likely to need this latitude; the binary formats keep the strict contract.

## Build / deployment / migration

- **`init_tenant` seeds the schemas and probes.** `crates/omp-core/src/probes/starter.rs` declares `STARTER_PROBES` (now nine entries: three `file.*`, plus `text.line_count`, `image.dimensions`, `audio.duration_seconds`, `video.duration_seconds`, `video.dimensions`, `pdf.page_count`) and `starter_schemas()` returns eight `(path, bytes)` pairs. The existing `init_tenant` seeding loop writes any path that doesn't already exist; `init` remains rerunnable and won't clobber tenant edits.
- **`scripts/build-probes.sh`** builds all nine probe `cdylib` crates (plus the `probe-mp4` `rlib` shared helper) from `probes-src/` to `wasm32-unknown-unknown` and stages the `.wasm` blobs at `crates/omp-core/build/wasm/`, where `starter.rs` `include_bytes!`-bakes them. Adding a probe to the starter pack is a workspace-member entry plus two `MAP` / `NAMES` lines in the script.
- **Existing tenants.** Tenants initialized before this change keep their existing tree until they re-init or pull the schemas/probes through the marketplace (doc 25 / doc 23) or via `POST /files`. The "if !p.exists()" guard in `init_tenant` means a re-init *fills in* the missing schemas and probes without touching anything that was already there.
- **Reprobe on commit.** When the new schemas land in a tenant's tree (via `init`, marketplace install, or `POST /files`), the reprobe pass from doc 21 fires automatically over every existing manifest of the affected file_type. PDFs ingested before the new schema landed will get `page_count` populated in the same atomic commit.
- **No service changes.** Pure data + WASM. No gateway, marketplace, or store change.

## Risks & deferrals

- **PDF `page_count` is best-effort.** Returns `null` rather than a wrong number when it can't see uncompressed page objects. Replacing the byte scan with a structural parser (`lopdf` or similar) is a follow-up — the probe contract (`returns = "int"`, null when unknown) doesn't change.
- **MP3 duration on heavily VBR or malformed streams.** The frame-walk handles standard CBR + Xing/Info VBR. Files with non-standard frame layouts (mid-stream sample-rate changes, embedded second files) may report a slightly skewed duration. Acceptable for v1.
- **No image color-mode, audio sample-rate / channels, video codec, markdown outline, or PDF text.** Each requires a real decoder (e.g., `image`, `symphonia`, an h264/h265 SPS parser, `pulldown-cmark`, a PDF text extractor) compiled to WASM with limit tuning. Deferred to a follow-up doc that handles them as a group.
- **MP4 covers ISO BMFF only.** WebM/Matroska, MOV (close to ISO BMFF but with quirks), and fragmented MP4 with no `moov` are out of scope. The probes return `null` rather than reaching for incorrect numbers.
- **Format coverage.** Eight formats covered; common LLM inputs not in scope yet — WebP, GIF, HEIC, FLAC, OGG, MOV, WebM, DOCX, EPUB. The starter pack stays small; the marketplace from doc 25 is the path for the long tail.
- **MIME glob ambiguity.** `text/*` (the `text` schema) overlaps with `text/markdown` (the `markdown` schema). The walker's "first match" rule plus the explicit `--type` override are sufficient for the demo path; a "most specific glob wins" tie-break is a deferred refinement.
- **Probe-output type for `image.dimensions`.** Returns a CBOR map `{width, height}` typed as `object` in the schema. The schema spec's `object` type is opaque-by-default, which is fine for now — a structured "dimensions" type would need an extension to the schema TOML spec, deferred.
- **WASM blob sizes.** Each new probe is ~90–120 KiB of release-mode WASM. The starter pack now totals ~700 KiB of embedded WASM, all `include_bytes!` into `omp-core`. Acceptable for the binary; if it grows further, lazy-loading from disk is a follow-up.

## Relationship to other docs

- [`04-schemas.md`](./04-schemas.md) — schema spec; reaffirmed. `validate_probe_refs` is what forces probes and the schemas that consume them to land together.
- [`05-probes.md`](./05-probes.md) — probe ABI; reaffirmed. New probes use the documented contract.
- [`09-roadmap.md`](./09-roadmap.md) — this doc is the long-form expansion of the "image / audio probes" line item, scoped down to the structural metadata the simple decoders can produce.
- [`12-large-files.md`](./12-large-files.md) — `audio.duration_seconds` and `pdf.page_count` have `max_input_bytes` of 256 MiB, well within the chunked-blob ceiling; nothing in this doc weakens chunked ingest.
- [`19-web-frontend.md`](./19-web-frontend.md) — the new `[render]` hints plug directly into the existing render-kind dispatcher.
- [`21-schema-reprobe.md`](./21-schema-reprobe.md) — backfills existing manifests when these schemas land in an existing tenant.
- [`23-probe-marketplace.md`](./23-probe-marketplace.md) — the new probes follow the per-folder layout from doc 23 and would be marketplace-publishable if a tenant wanted to fork them.
- [`25-schema-marketplace.md`](./25-schema-marketplace.md) — folder layout reused exactly; visual editor reads the new schemas like any other.

## Implementation status

- ✅ Seven new starter schemas committed at `crates/omp-core/starter-schemas/<file_type>/schema.toml` for `markdown`, `png`, `jpeg`, `mp3`, `wav`, `mp4`, `pdf`. Universal probe fields + format-specific probe fields where probe exists + `[render]` hint. No `user_provided` fields. `text` (extended) and `markdown` set `allow_extra_fields = true`.
- ✅ Six new probe `cdylib` crates under `probes-src/`: `text-line-count`, `image-dimensions`, `audio-duration`, `video-duration`, `video-dimensions`, `pdf-page-count`. Plus `probe-mp4` (rlib) shared by the two video probes.
- ✅ Probe manifests at `crates/omp-core/starter-probes/{text.line_count,image.dimensions,audio.duration_seconds,video.duration_seconds,video.dimensions,pdf.page_count}.probe.toml`.
- ✅ `scripts/build-probes.sh` extended to compile and stage all six new WASM blobs.
- ✅ `crates/omp-core/src/probes/starter.rs::STARTER_PROBES` now includes all nine probes; `starter_schemas()` returns all eight schemas.
- ✅ `crates/omp-core/tests/end_to_end.rs::init_drops_starter_pack` updated to assert the full starter pack lands.
- ✅ `docs/design/README.md` index entry added.
- ⏸ Image color-mode, audio sample-rate / channels, video codec, PDF text extraction, markdown outline — deferred to a follow-up doc that handles WASM-build of real decoders.
- ⏸ Per-schema `README.md` and `examples/` companions — deferred. The marketplace publish flow is the natural place for those.
- ⏸ Format coverage beyond the eight defaults (WebP, GIF, HEIC, FLAC, OGG, MOV, WebM, DOCX, EPUB) — deferred to the marketplace.
- ⏸ Inline audio/video/PDF preview render kinds — frontend work, deferred to a future doc.
