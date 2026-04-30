# 25 — Schema marketplace + visual schema editor

[`23-probe-marketplace.md`](./23-probe-marketplace.md) shipped a marketplace for probes and explicitly deferred the schema equivalent: *"Schemas have no companions today; the question of whether they need a folder-ified layout and a marketplace is reopened only if real usage forces it."* This doc reopens it.

The reason it's a separate doc rather than a paragraph appended to 23 is the **UI**. A schema in [`04-schemas.md`](./04-schemas.md) is a TOML file — perfectly fine for an LLM author or a CLI user, but a dead-end for a human who just installed a schema from the marketplace and wants to tweak one field. Doc 19's "no in-app schema editor" deferral was livable when schemas were one-per-tenant artifacts authored alongside code; it stops being livable the moment installing someone else's schema is one click. So this doc adds three things alongside doc 23's pattern:

1. **A per-schema folder layout** mirroring doc 23's per-probe cutover, so a published schema can ship companions (README, example documents).
2. **Marketplace endpoints** for schemas, sharing the existing `omp-marketplace` service rather than spawning a second one.
3. **A visual schema editor** — the differentiator. Probe-source fields become a typeahead picker over installed/marketplace probes; every other knob becomes a dropdown or free-form text box. TOML stays as the on-disk source of truth; the editor is a view that round-trips through it.

The folder layout lands as code in this iteration. The marketplace endpoints and visual editor are design commitments with the API/UI contracts pinned, mirroring doc 23's posture.

## Why this fits inside `omp-marketplace` (not a new service)

Doc 23 argued for `omp-marketplace` as a separate service from the gateway because the access pattern, storage shape, and deploy cadence all differed from per-tenant traffic. None of those arguments push schemas into a *third* service:

- **Same access pattern.** Public read, authenticated write, publisher attribution by WorkOS `sub`. Identical to probes.
- **Same storage shape.** Catalog table + content-addressed blobs in a dedicated `omp-store` instance. The schema catalog is a sibling table in the same SQLite/Postgres database; blobs share the existing bucket.
- **Same deploy cadence.** Schemas and probes ship to the same audience (anyone with a WorkOS account who wants to publish OMP content). Splitting them across two pods would double the WorkOS plumbing, the Helm values, and the gateway routing rules for zero benefit at course-project scale.

So `omp-marketplace` gains a `schemas.rs` module beside the existing probe handlers, sharing `catalog.rs` and `blobs.rs`. A future split is a refactor, not a redesign — both modules are already isolated by entity.

## What does *not* change

- The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md). The marketplace and editor are strictly additive.
- The TOML schema spec from [`04-schemas.md`](./04-schemas.md). The four field sources, the fallback wrapper, the `[render]` block, the type set — all unchanged. The editor is a view over this spec, not a replacement for it.
- `schema_hash` semantics. Every manifest still records the framed-sha256 of the schema bytes used at ingest. Time-travel by hash works exactly as before.
- The dry-run + auto-reprobe flow from [`21-schema-reprobe.md`](./21-schema-reprobe.md). A schema commit (whether from the visual editor, the CLI, or the LLM-author path) still triggers atomic per-file_type rebuilds.
- The `TenantContext` envelope. The `sub` field added in doc 23 is reused; no new envelope fields.
- The HTTP API contract from [`06-api-surface.md`](./06-api-surface.md). New routes are additive under `/marketplace/schemas/*`.
- Doc 22's WorkOS auth flow. Publish endpoints reuse the existing gate.
- The CLI / LLM-author path. `POST /files schemas/<file_type>/schema.toml` still works; the editor is one of several authoring surfaces.

If a marketplace or editor requirement would force any of the above to change, the requirement is wrong, not the constraint.

## Per-schema folder layout (in this iteration, code)

[`04-schemas.md`](./04-schemas.md) and the existing implementation use a flat layout:

```
schemas/<file_type>.schema
```

This was right when a schema was a single artifact authored in-tree alongside the rest of the repo. A *published* schema wants companions — a README explaining the field semantics, sample documents the schema is designed for, possibly multiple variants for closely-related MIME types — for the same reasons doc 23 cited for probes.

The new layout is one directory per schema:

```
schemas/<file_type>/
├── schema.toml                # required: the schema (was: schemas/<file_type>.schema)
├── README.md                  # optional: prose for marketplace + editor sidebar
└── examples/                  # optional: sample documents
    └── sample.pdf
```

Two things to notice:

- `schema.toml`, not `<file_type>.schema`. The directory carries the file_type; repeating it in the filename is noise, and the `.schema` extension was only ever a marker for the walker — the walker now keys off the directory.
- The `file_type` key inside `schema.toml` still must match the directory basename (the analogue of doc 04's "filename stem matches `file_type`" rule).

**Hard cutover, no back-compat.** The schema walker stops accepting the flat layout and emits a warning if it encounters one (so a half-migrated repo surfaces visibly inert rather than silently broken). The starter pack in `crates/omp-core/starter-schemas/` is migrated; the demo `_local` tenant is regenerated by deleting `tenants/_local/` and letting `init_tenant` re-seed. This matches doc 23's posture exactly.

The change touches a small number of files:

- **`crates/omp-core/src/schema.rs`** — schema discovery functions walk `schemas/<file_type>/schema.toml` instead of `schemas/<file_type>.schema`.
- **`crates/omp-core/src/api.rs::current_schemas` / `current_schema_for_type`** — same walker change.
- **`crates/omp-core/src/api.rs::Repo::init_tenant`** — seeds the new directory layout for the starter schemas.
- **`crates/omp-core/starter-schemas/`** — `text.schema` and friends move to `text/schema.toml` etc.
- **Tests** — anywhere that hardcoded `schemas/<file_type>.schema` is updated.

`schema_hash` remains the framed-sha256 over the bytes of `schema.toml` exactly as before. Manifests ingested before this cutover still resolve their old `schema_hash` against the object store (blobs aren't moving); only the *path* in the working tree changes, and old paths simply no longer exist after the cutover commit. Schemas continue to validate at write-time per doc 04's "Validation at write time" section; the new validation is "directory basename matches `file_type` key."

The asymmetry doc 23 called out ("schemas are deliberately not folder-ified... if schemas later grow companions, they should follow the same pattern") is now resolved.

## Marketplace endpoints (implemented)

The `omp-marketplace` crate has a `schemas.rs` module beside the probe handlers. The schema catalog persists to a sibling JSON file (`catalog_schemas.json`); blob storage is the same content-addressed store as probe blobs. Endpoints below match what's wired in code.

### Endpoints

Public (no auth required):

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/marketplace/schemas` | List/search published schemas. Query params: `file_type`, `publisher_sub`, `q` (substring against description), `limit`, `cursor`. Returns paginated catalog rows. |
| `GET` | `/marketplace/schemas/<id>` | Single catalog entry plus a parsed schema preview: resolved field list (name, source, type, required, description) and the list of referenced probes with marketplace ids. |
| `GET` | `/marketplace/schemas/<id>/blobs/<hash>` | Download a specific blob (`schema.toml`, `README.md`, an example file) by content hash. |

Authenticated (gateway forwards a signed `TenantContext` carrying `sub`):

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/marketplace/schemas` | Publish a schema (publisher only). Multipart form: `version`, `description`, `schema` (the `schema.toml` body), `readme` (optional). The `file_type` is taken from the parsed TOML — there is no separate field. Server validates the TOML deserializes into the minimal shape (`file_type` non-empty, `mime_patterns` non-empty), stores blobs by sha256, writes a catalog row. Probe-reference resolution against the probe catalog is **deferred to install time** (see Doc 21 §reprobe and the install endpoint below) so that publishing a schema that references an in-flight probe upload doesn't fail spuriously. Examples bundles are deferred to a follow-up. |
| `PATCH` | `/marketplace/schemas/<id>` | Edit metadata (publisher only). JSON body with optional `description` and `readme`. Does not change the `schema_hash`. |
| `DELETE` | `/marketplace/schemas/<id>` | Yank a schema (publisher only). Catalog row is marked unpublished; **blobs are preserved forever** so historical replay against past `schema_hash` keeps working (per `02-object-model.md` §74-82). |

Install is a *gateway* endpoint, not a marketplace one — same shape as doc 23's probe install:

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/marketplace/install/schema/<id>` | Gateway pulls the schema folder from the marketplace, stages each blob into the caller's tenant under `schemas/<file_type>/{schema.toml,README.md,examples/*}` via the same ingest path as `POST /files`, **and stages any referenced probes that are not already present** by resolving each probe ref to its marketplace id and reusing the existing probe-install flow. The user clicks Commit on the existing Commit page to make it durable; schema and any newly-installed probes land in one atomic commit, which means the auto-reprobe pass from doc 21 fires once over a consistent state. |

Cross-tenant probe resolution at install time is the one piece of new logic. It works because doc 23's catalog already keys probes by `<namespace>.<name>` and the marketplace knows the latest published version per probe; the install endpoint picks the latest non-yanked version unless the schema's catalog row pinned a specific id.

### Catalog schema

```sql
CREATE TABLE schemas (
    id              TEXT PRIMARY KEY,         -- sha256(publisher_sub|file_type|version)
    publisher_sub   TEXT NOT NULL,            -- WorkOS user id
    file_type       TEXT NOT NULL,
    version         TEXT NOT NULL,            -- free-text; semver enforcement deferred (see doc 23)
    description     TEXT,
    schema_hash     TEXT NOT NULL,            -- framed sha256 of schema.toml
    readme_hash     TEXT,
    examples_hashes TEXT,                     -- JSON array: [{"filename":"sample.pdf","hash":"…"}]
    probe_refs      TEXT NOT NULL,            -- JSON array of "<ns>.<name>" used by this schema
    published_at    INTEGER NOT NULL,         -- unix seconds
    yanked_at       INTEGER,                  -- NULL if live
    downloads       INTEGER NOT NULL DEFAULT 0,
    UNIQUE(publisher_sub, file_type, version)
);
CREATE INDEX schemas_file_type ON schemas(file_type);
CREATE INDEX schemas_publisher ON schemas(publisher_sub);
```

`probe_refs` is denormalized intentionally:

- The install endpoint resolves probe dependencies without re-parsing the blob.
- The browse UI can show "uses N probes" and link directly to each without fetching `schema.toml`.
- The publish-time validator writes it once; readers never need to recompute it.

### Storage

Blobs (`schema.toml`, `README.md`, example files) live in the same `omp-store-marketplace` instance as probe blobs. Schema-side blobs are typically tiny (TOML + Markdown); example files are the main size driver and are explicitly allowed up to a per-publish quota (default 10 MiB total) to keep the marketplace lean.

### Trust model

- **Identity = WorkOS `sub`,** same as doc 23. Publish requires a valid `omp_session` cookie or token-mode Bearer; the gateway forwards a signed `TenantContext` to the marketplace; `publisher_sub` is recorded from the envelope, never from a client-supplied identifier.
- **No code-signing.** Schemas are pure data — they cannot execute. The blast radius of a malicious schema is bounded by the four field sources, all of which are sandboxed (constants are inert; probe calls run in the existing wasmtime sandbox; user_provided is opaque storage; field references are pure transforms). This is a strictly weaker risk profile than probes.
- **Probe references must resolve at publish time.** The publish endpoint rejects a schema that references a probe `<ns>.<name>` not present in the live (non-yanked) probe catalog. This prevents publishing a schema that is dead-on-arrival for installers.
- **Yanked schemas still resolve by hash.** A user who ingested a file when schema X was live has `schema_hash = <hash>` in their manifest. Replay/audit code calls `marketplace.GET /schemas/<id>/blobs/<hash>`; yanked schemas still answer (only the catalog row is hidden, not the blobs), preserving historical reproducibility.

## Visual schema editor (deferred implementation)

The reason this design isn't just "doc 23 with `s/probe/schema/`." Two new frontend surfaces, contracts pinned now so the API is stable when the implementation lands.

### `/ui/schema-marketplace` — browse + install

Mirrors `/ui/marketplace` from doc 23 row-for-row: search bar over `file_type` and `q`, paginated list, per-row "Install" button. Clicking install does `POST /marketplace/install/schema/<id>`, then redirects to `/ui/commit` with the staged schema folder *and any newly-staged probe folders* ready to commit. The Commit page already shows a path-by-path diff — the user sees both the schema and the probes about to land.

A "Publish to marketplace" affordance lands on the editor (next section), mirroring doc 23's Build-page button.

### `/ui/schemas/<file_type>/edit` — the visual editor

A form view over `schemas/<file_type>/schema.toml`. New schemas enter via a "New schema" button on the schemas list, which opens the same form against a blank state with a `file_type` input.

Top-level layout:

```
┌─ Schema: pdf ────────────────────────────────────────────┐
│  file_type: [pdf            ]   version: [free-text   ]  │
│  mime_patterns: [+]                                      │
│    [application/pdf                              ] [×]   │
│  allow_extra_fields: ☐                                   │
│  Render kind: [markdown ▾]   max_inline_bytes: [65536]   │
├─ Fields ─────────────────────────────────────────────────┤
│  ▸ title                 user_provided  string  [edit]   │
│  ▸ page_count            probe          int     [edit]   │
│  ▸ summary               user_provided  string  [edit]   │
│  ▸ slug                  field          string  [edit]   │
│  [+ Add field]                                           │
└──────────────────────────────────────────────────────────┘
```

A field row, expanded:

```
┌─ page_count ─────────────────────────────────────────────┐
│  Name:        [page_count                ]               │
│  Source:      [probe ▾]                                  │
│  Probe:       [pdf.page_count           ▾] [Browse...]   │  ← picker
│  Type:        [int ▾]                                    │
│  Required:    ☑                                          │
│  Description: [Number of pages in the document.        ] │
│  ▸ Add fallback                                          │
└──────────────────────────────────────────────────────────┘
```

#### The probe picker (the most important affordance)

A typeahead dropdown that lists, in two grouped sections:

1. **Installed in this tenant** — from `GET /probes`. Selecting one is a no-op beyond writing the `<ns>.<name>` into the schema.
2. **Available in the marketplace** — from `GET /marketplace/probes`, shown grayed with an "install on save" badge. Selecting one stages the probe install alongside the schema edit, so a single Commit click publishes both. This re-uses the gateway install endpoint from doc 23 — no new server logic.

The picker's "Browse..." button opens a side-panel mini-marketplace scoped to probes, so a user who doesn't know what they're looking for can read READMEs without leaving the editor.

When `source = probe` is selected, the picker also loads the chosen probe's manifest and renders an `args` editor whose keys/types come from the probe — no free-form key/value typing, no chance of submitting `args = { N = 5 }` when the probe expected `n`.

#### Dropdowns for everything that's a closed set

Every value with a closed set in [`04-schemas.md`](./04-schemas.md) is a dropdown, so neither a human nor an LLM driving the editor can introduce a typo that fails write-time validation:

| Field | Control |
| --- | --- |
| `source` | dropdown: `constant` / `probe` / `user_provided` / `field` |
| `type` | dropdown: `string` / `int` / `float` / `bool` / `datetime` / `list[*]` / `object` |
| `transform` (when source=field) | dropdown: `identity` / `slugify` / `lower` |
| `render.kind` | dropdown: `text` / `hex` / `image` / `markdown` / `binary` / `none` |
| `from` (when source=field) | dropdown over the schema's other field names |
| `required`, `allow_extra_fields` | checkbox |

#### Free-form text boxes for everything else, with inline validation

| Field | Notes |
| --- | --- |
| `file_type` | Validated against `^[a-z0-9_-]+$`; must match the URL path / folder basename. |
| `mime_patterns[]` | Repeatable rows; each entry validated as an fnmatch-style MIME glob. |
| Field `name` | Validated `^[a-z][a-z0-9_]*$`. |
| `description` | Multiline, no validation beyond length. |
| `value` (when source=constant) | Type-aware input: number for `int`/`float`, checkbox for `bool`, text for `string`, JSON editor for `object` and `list[*]`. |
| `version` | Free text; semver-shaped advised but not enforced (matches doc 23's deferral). |

#### Round-trip with TOML

The editor's source of truth is the schema's TOML bytes. On load, parse via the existing `omp_core::schema::Schema::parse` (exposed through a new `GET /schemas/<file_type>/parsed` helper that returns the structured form already used by `GET /schemas`). On save, re-serialize to TOML using a stable formatter (preserving form key order) and `POST /files schemas/<file_type>/schema.toml`. A "View raw TOML" toggle shows the rendered output before save; a "Dry-run" button hits `POST /test/ingest` per doc 04 §Editing a schema safely and shows the manifest that would be produced.

The editor is *one* authoring surface, not the only one. CLI users and LLM authors keep using the raw `POST /files` path; their TOML formatting is preserved verbatim and their workflow is unchanged. The "View raw TOML" toggle and the dry-run path mean an editor user can hand off to an LLM mid-edit without losing fidelity.

#### Publish-to-marketplace

After a successful save, the editor surfaces a "Publish to marketplace" button. Clicking opens a small modal asking for `version`, optional README contents (a Markdown editor pre-filled from the existing `README.md` if any), and example file uploads. Submit fires `POST /marketplace/schemas`. Same affordance shape as doc 23's probe Build page.

## Build & deployment

**No new pod.** `omp-marketplace` already exists per doc 23; this doc adds a module to it. Helm gains no new top-level values; the existing `marketplace.*` block is reused unchanged.

**Gateway routing.** No change. `^/marketplace/` → `omp-marketplace` already covers `/marketplace/schemas/*`. The new gateway endpoint `/marketplace/install/schema/<id>` is the only additional path-prefix on the gateway side, mirroring `/marketplace/install/<id>` from doc 23.

**CI.** The existing `cargo test -p omp-marketplace` step covers the new module; `publish_install.rs` gains a schema round-trip test (publish schema referencing a marketplace probe → list → install → confirm both staged).

**No new external secrets.** WorkOS still lives only in the gateway.

## Risks & deferrals

- **Probe-availability mismatch at install.** A schema published when probe `pdf.page_count v1` was live may install into a tenant that already has `pdf.page_count v2`. The install endpoint stages the schema's referenced probe only if the probe `<ns>.<name>` is *not present at all* in the tenant; otherwise it leaves the existing version alone. The schema is only required to validate against probe *names* (per doc 04's write-time validation), not against specific versions. If the existing probe's manifest has incompatible `args`, validation fails at commit time — same behavior as today.
- **Editor / LLM-author drift.** A schema authored in the editor and then hand-edited via raw POST should re-load cleanly in the editor. Achieved by re-parsing the canonical structured form on every load; formatting differences don't affect parsing because the editor only reads logical structure, not byte-level layout. The "View raw TOML" toggle plus the schema-formatting note from doc 04 ("Reformatting churns `schema_hash`") are both surfaced in the editor's help text.
- **Schema-spam.** Same posture as doc 23's probe-spam mitigation: WorkOS auth gate on publish, future per-publisher rate limiting, future moderation. Course-project demo doesn't need either.
- **Versioning.** v1 stores `version` as free-text. Semver enforcement is deferred (matches doc 23). The catalog row accommodates it when added.
- **Search.** v1 supports exact-match `file_type` and substring on description. Full-text search (sqlite FTS5 or Postgres tsvector) is a follow-up.
- **Editor for `object` / `list[*]` constants.** v1 ships a JSON editor with type validation. A structured nested-table editor is a follow-up if real schemas demand it.
- **Multi-region.** Single-region only, same as doc 23.

## Relationship to other docs

- [`02-object-model.md`](./02-object-model.md) — content-addressed framed-hash storage is the basis for the schema marketplace's blob model. The `schema_hash` invariant in manifests is what makes yanking safe.
- [`04-schemas.md`](./04-schemas.md) — superseded for the on-disk layout (flat → per-folder); reaffirmed for everything else (TOML spec, four field sources, validation, render hints, dry-run, formatting note).
- [`05-probes.md`](./05-probes.md) — unchanged. The schema editor's probe picker reads from the existing `GET /probes` and `GET /marketplace/probes`.
- [`14-microservice-decomposition.md`](./14-microservice-decomposition.md) — no service added; `TenantContext.sub` from doc 23 is reused.
- [`19-web-frontend.md`](./19-web-frontend.md) — closes the "no in-app schema editor" deferral.
- [`21-schema-reprobe.md`](./21-schema-reprobe.md) — preserved. Schema commits from the editor trigger the same atomic per-file_type rebuild as commits from any other path.
- [`22-workos-auth.md`](./22-workos-auth.md) — `sub` from the WorkOS session is the publisher identity; the marketplace runs entirely behind the existing WorkOS gate.
- [`23-probe-marketplace.md`](./23-probe-marketplace.md) — supersedes the "Schema marketplace... reopened only if real usage forces it" footer entry. Folder-layout asymmetry called out there is now resolved.

## What's deferred

- The `schemas.rs` module in `omp-marketplace`, its catalog table, its endpoints.
- Frontend `/ui/schema-marketplace` and `/ui/schemas/<file_type>/edit` pages.
- The probe picker component (typeahead over installed + marketplace probes).
- Cross-tenant probe-install staging from the gateway install endpoint.
- The `GET /schemas/<file_type>/parsed` helper.
- Semver-aware publishing, full-text search, rate limiting, moderation, structured nested-table constant editor.

## Implementation status

This iteration commits to:

- ✅ Per-schema folder layout — code lands now.
- ✅ Schema walker + `init_tenant` + starter schemas + tests updated.
- ✅ `docs/design/04-schemas.md` updated to reference the new layout.
- ✅ `docs/design/23-probe-marketplace.md` "What's deferred" footer updated to point at this doc.
- ⏸ `crates/omp-marketplace/src/schemas.rs` — new module, deferred.
- ⏸ Gateway `/marketplace/install/schema/<id>` endpoint — deferred until the marketplace module exists.
- ⏸ Frontend marketplace page + visual editor + probe picker — deferred.
- ⏸ Helm chart changes — none needed; `omp-marketplace` already covers it.
