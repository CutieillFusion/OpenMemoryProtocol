# 01 — On-disk layout

OMP mirrors Git's on-disk layout as closely as practical. If you know where Git puts things, OMP puts them in the analogous place.

## Repository structure

```
<repo>/
  .omp/                         # OMP's private state — never in any tree, never versioned
    HEAD                        # text: "ref: refs/heads/main" or a commit hash (detached)
    refs/
      heads/main                # text file: <commit hash>
      heads/experimental        # text file: <commit hash>
    objects/
      ab/cdef0123456789...      # zlib-compressed object; filename = last 62 chars of hash
    local.toml                  # machine-local config; server bind, default author; NOT versioned

  omp.toml                      # versioned repo config; ignore patterns, schema policy

  schemas/                      # versioned schema definitions
    text.schema                 # TOML — schema for text/markdown files
    pdf.schema                  # TOML — schema for PDFs
    audio.schema                # TOML — schema for audio files (iter 2)
    image.schema                # TOML — schema for images (iter 2)

  probes/                       # versioned WASM extractors (see 05-probes.md)
    file/
      size.wasm                 # compiled WASM module
      size.probe.toml           # probe manifest (name, return type, limits)
      mime.{wasm,probe.toml}
      sha256.{wasm,probe.toml}
      name.{wasm,probe.toml}
    text/
      line_count.{wasm,probe.toml}
      first_lines.{wasm,probe.toml}
      frontmatter.{wasm,probe.toml}
    pdf/
      page_count.{wasm,probe.toml}
      toc.{wasm,probe.toml}
      title.{wasm,probe.toml}
      text_excerpt.{wasm,probe.toml}

  media-example1/               # a user collection — hierarchical grouping
    intro.mp3
    video.mp4
    transcripts/
      intro.md
      video.md

  media-example2/               # another user collection
    presentation.pdf
    slides/
      slide-01.png
      slide-02.png

  reports/                      # another grouping
    earnings-q3.pdf
    2026-03-kickoff.md
```

## The two halves

Every OMP repo has two distinct halves:

### 1. The **working tree** (everything outside `.omp/`)

What the user sees and edits. Includes:
- User content files (PDFs, audio, markdown, images, anything)
- `schemas/*.schema` — TOML files declaring per-file-type manifest shapes
- `probes/**/*.wasm` and `probes/**/*.probe.toml` — WASM extractors and their manifests
- `omp.toml` — versioned repo config

The working tree is what gets walked and committed. All four categories above are tracked and versioned identically.

### 2. `.omp/` — OMP's private state

Mirrors Git's `.git/` directory. Contents:
- **`HEAD`** — current branch reference, plain text.
- **`refs/`** — named pointers to commits, each a plain text file containing a commit hash.
- **`objects/`** — content-addressed object store. Each object is zlib-compressed bytes of `<type> <size>\0<content>`. File path is `<first 2 hex chars>/<remaining 62 hex chars>` of its SHA-256.
- **`local.toml`** — machine-local, un-versioned config: server bind address, default author identity. Kept out of the tree so checking out an old commit doesn't change how *this* machine's server runs.

`.omp/` is always ignored by the tree walker and never appears in any `tree` object.

## Hierarchical grouping

Users organize files into directories freely. OMP's tree model is nested (see [`03-hierarchical-trees.md`](./03-hierarchical-trees.md)), so `media-example1/transcripts/intro.md` is a real path, not a filename with slashes. Each directory along that path corresponds to its own tree object in storage.

Rationale: LLM-focused workflows collect related media into bundles — a podcast episode has its audio, its transcript, and its show notes in one folder. Keeping them physically together makes browsing, review, and bulk operations natural.

## What a tree walk sees

When OMP walks the working tree to build a commit:

- `.omp/` is always skipped.
- Patterns in `omp.toml`'s `[workdir.ignore]` list are skipped. **Ignore affects untracked files only** — a path already recorded in HEAD's tree is still walked and either updated or preserved even if its path now matches an ignore pattern, identical to Git. To stop tracking something, you must first `DELETE /files/{path}` (or `omp rm`), then add the pattern. This keeps ignore edits from silently dropping committed content.
- Symlinks encountered during the walk are handled per `[workdir] follow_symlinks` in `omp.toml` (default `false`). When `false`, symlinks are skipped silently — they do not appear as blobs or manifests and are not errors. When `true`, the walker follows the target and treats it as if the file were at the symlink's location; loops are detected by path and broken with a walk-time error.
- Every remaining file produces either a **blob entry** (schemas, probes, `omp.toml`) or a **manifest entry** (user files). Path-based convention: anything under `schemas/` or `probes/`, or named `omp.toml` at the repo root, becomes a blob; everything else becomes a manifest.
- Every directory produces a **tree entry** pointing at its own tree object.

## Why `omp.toml` isn't under a dot-file

Git's `.gitignore`, `.gitattributes`, etc. are dotfiles. OMP's equivalent is `omp.toml` — no leading dot. Reasons:

- It's a structured file with multiple sections (ignore, schema policy, future settings). Dotfiles traditionally tend to be flat, single-purpose.
- It's discoverable: a user glancing at the repo sees `omp.toml` next to `pyproject.toml`, `package.json`, `Cargo.toml` — recognizable as "the config file for this kind of project."
- It's parallel to the modern ecosystem (pyproject.toml, Cargo.toml, tsconfig.json), not the legacy Unix one.

## Comparison with Git's layout

| Concept           | Git                    | OMP                          |
|-------------------|------------------------|------------------------------|
| Private state dir | `.git/`                | `.omp/`                      |
| Objects dir       | `.git/objects/`        | `.omp/objects/`              |
| Refs dir          | `.git/refs/`           | `.omp/refs/`                 |
| HEAD              | `.git/HEAD`            | `.omp/HEAD`                  |
| Versioned config  | (none — config is machine-local) | `omp.toml`         |
| Machine config    | `.git/config`          | `.omp/local.toml`            |
| Ignore file       | `.gitignore`           | `[workdir.ignore]` in `omp.toml` |
| Attributes file   | `.gitattributes`       | (not in v1; schemas cover this use case) |
| File extensions used in objects | zlib'd binary | zlib'd binary — same |

OMP intentionally diverges on versioned config (Git has none) because schema decisions are semantically part of the repo — you want to time-travel them.
