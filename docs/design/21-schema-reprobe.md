# 21 — Schema-update reprobe

When a tenant updates `schemas/<file_type>.schema` to add a field, files committed before the schema change still have their old manifests. The new field is missing on every existing file of that type. To populate it, the tenant has to re-ingest each file individually — and there's no UI for that.

This doc closes that gap. A schema-changing commit, as part of the same atomic operation, walks every existing file of the affected `file_type` and rebuilds its manifest against the new schema. The same commit contains the new schema and every rebuilt manifest. After the commit, HEAD reflects the current schema for every file; HEAD~1 still has the pre-change schema and pre-change manifests — time-travel queries are unchanged.

The hard requirement the design hangs off: **smart enough to skip work**. Most schema changes add exactly one new field. The N-1 fields that didn't change should not re-run probes. A correctly-implemented reprobe of an N-file repo with one new probe-driven field requires N probe runs, not N × M.

## Goals

- A schema commit propagates to every existing manifest of that file_type, atomically. After the commit, querying for the new field works for files committed years ago.
- Old manifests stay reachable via time-travel — historical commits don't change, only HEAD does.
- Smart caching: unchanged fields copy verbatim from the old manifest; identical source bytes re-use probe results within one reprobe pass.
- Probe failures isolate to a single file. The commit succeeds with a list of skipped paths. One bad PDF doesn't block schema iteration on a 10k-file corpus.

## Non-goals

- **Retroactive ingestion of files that aren't in the tree.** This is reprobe of *committed* files only. Untracked files in the working directory are still untracked.
- **Background processing.** Reprobe runs synchronously inside the commit critical section. For a few hundred files this is invisible; for repos with millions of files there will be noticeable commit latency. Async/queue-backed reprobe is a v2 concern.
- **Cross-tenant invalidation.** Each tenant reprobes their own repo independently. Single-writer-per-tenant from [`11-multi-tenancy.md`](./11-multi-tenancy.md) means there's no race.
- **A standalone `omp reprobe-everything`.** The trigger is "schema in this commit changed". A plain `omp reprobe <file_type>` exists as an escape hatch (for cases where someone set `OMP_DEFER_REPROBE=1` or wants to force a re-derivation without a schema change), but there's no "rebuild every manifest in the repo" command.

## What does *not* change

- The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md). Manifests are still SHA-256-framed canonical TOML; probes still run in the wasmtime sandbox; schemas still source fields from the same closed set. Reprobe is just another caller of `engine::ingest`-style logic.
- **Manifest immutability.** A reprobed file gets a *new* manifest object with a *new* hash. The old manifest still exists in the object store; it's still referenced by HEAD~1's tree. We never overwrite an existing object — we always emit a new one and update the tree pointer.
- The probe ABI from [`05-probes.md`](./05-probes.md). Probes don't know they're being run during a reprobe vs. an initial ingest.
- The schema TOML grammar from [`04-schemas.md`](./04-schemas.md). Reprobe is purely about *applying* a committed schema; authoring stays the same.
- The `commit()` API surface. Callers continue to call `repo.commit("message", author)`. The response payload grows additively (a new optional `reprobed` field).

## Architecture

```
                 ┌──────────────────────────────────────────┐
   POST /commit  │  commit transaction (single-writer lock) │
   ──────────────►                                          │
                 │  1. load .omp/index.json                 │
                 │  2. detect schemas/*.schema in staging   │
                 │  3. for each changed schema:             │
                 │       reprobe_for_schema(file_type, …)   │
                 │       (stages new manifests in-place)    │
                 │  4. build new tree from index            │
                 │  5. write commit object, update HEAD     │
                 │  6. publish commit.created event         │
                 └──────────────────────────────────────────┘
```

The reprobe step is sandwiched between step 1 and step 4. It writes new manifest objects to the store and adds new entries to the index (alongside the user's original staged changes). When step 4 builds the tree, those new manifests are picked up automatically.

Importantly, this means **the reprobe and the schema change are in the same commit**. There's no two-step "commit the schema, then commit the reprobed manifests". One commit, one new tree, one new HEAD.

## The algorithm

`Repo::reprobe_for_schema(file_type, new_schema, new_schema_hash, probes, &mut cache)`:

1. Walk HEAD's tree (`paths::walk`). Filter for `Mode::Manifest` entries.
2. For each, parse the manifest. Skip if `manifest.file_type != file_type`. Skip if `manifest.schema_hash == new_schema_hash` (already current — happens when the same commit ingested files *after* staging the new schema).
3. Read the source bytes via `manifest.source_hash` (plaintext via `store.get`; chunked via `ChunksBody::parse` + reassembly; encrypted tenants are skipped — see below).
4. Call `reprobe_one(path, old_manifest, source_bytes, new_schema, new_schema_hash, probes, &mut cache)`.
5. Stage the resulting new manifest via `stage_upsert(path, Mode::Manifest, new_hash)`. The existing index path takes care of replacement.
6. On per-file failure, push to `Vec<ReprobeSkip>` and continue. After the loop, the count of successes + the skip list are returned for the commit response.

`reprobe_one(path, old_manifest, source_bytes, new_schema, …)`:

For each field in `new_schema.fields`:

- **Field-level reuse.** Look up `field.name` in `old_schema.fields` (where the old schema is loaded from the manifest's `schema_hash`).
  - If old has a field with the same name AND `old_field.source == new_field.source` AND (when source is a probe) `old_manifest.probe_hashes[probe_name] == probes[probe_name].framed_hash`, copy `old_manifest.fields[field.name]` verbatim. Add the probe hash to the new manifest's `probe_hashes` map. **No probe run.**
- **Otherwise** call the same `engine::resolve_field` machinery the ingest path uses. The probe-output cache is consulted before invoking wasmtime.

Source-variant rules within the "otherwise" branch:

| Source | Reprobe behavior |
|---|---|
| `Probe { probe, args }` | Cache lookup → probe run if missing → cache insert. New manifest's `probe_hashes[probe] = view.probes[probe].framed_hash`. |
| `Constant { value }` | Use the new constant. |
| `Field { from, transform }` | Re-evaluate from the (already-resolved-this-pass) sibling field. |
| `UserProvided` | Carry the value over from `old_manifest.fields[field.name]` if the field name is unchanged. If the field is new (didn't exist in old schema), the value is `Null` — there's no human at the keyboard. |

Top-level manifest fields:

- `source_hash`: copied from old manifest. The bytes haven't changed.
- `file_type`: copied (the schema's `file_type` should match anyway).
- `schema_hash`: set to `new_schema_hash`.
- `ingested_at`: stamped to `now()`. This is a fresh ingestion artifact.
- `ingester_version`: set to current `INGESTER_VERSION`. Same reason.
- `probe_hashes`: rebuilt per-field as above.
- `fields`: rebuilt per-field as above.

If the new manifest is byte-for-byte identical to the old one (e.g. a schema commit that only changed a description field — though that's unlikely to pass the diff check earlier — or a re-derivation that produced the same bytes), the `stage_upsert` becomes a no-op against the index.

## The probe-output cache

```rust
type ProbeOutputCache = HashMap<(SourceHash, ProbeFramedHash, ArgsCanonical), FieldValue>;
```

`ArgsCanonical` is a deterministic string form of the field's `args: BTreeMap<String, FieldValue>`. We already have `crates/omp-core/src/toml_canonical.rs` for canonical TOML; reuse it. Two semantically-identical `args` maps (same keys, same values, any order) produce the same string.

Lifetime: one cache instance per `commit()` call. It survives across multiple `reprobe_for_schema` calls within the same commit (e.g. if both `schemas/text.schema` and `schemas/pdf.schema` were staged together) but never across commits. No persistence, no LRU, no eviction — just a HashMap.

Hit rate analysis for the typical scenario (the user's case):
- 6 text files in HEAD, schema gains one new field driven by a brand-new probe.
- For each file: 2 fields are field-level-reused (no probe runs, no cache touches). 1 field needs the new probe.
- The 6 source files have distinct content → 6 cache misses → 6 probe runs.
- Net: 6 probe runs total, against 6 × 3 = 18 in the naive "rerun everything" approach. The cache buys nothing in this scenario; field-level reuse buys everything.

For repos with duplicate file content (a common test corpus pattern, or fan-out of the same template), the cache buys real work. Same probe + same content = one run, N-1 cache hits.

## Failure handling

A probe can fail in three documented ways: timeout, memory cap, sandbox refusal. There's also a fourth: the probe binary referenced by the new schema isn't in HEAD's tree (would-be `OmpError::SchemaValidation` from `validate_probe_refs`).

The first three are caught at probe-run time. The fourth is caught earlier at schema-validation time (the schema staging itself fails, never reaching commit).

For the first three, when reprobe encounters one:

1. Don't re-stage that file's manifest. The old manifest stays in the index (i.e. the path's tree entry is unchanged).
2. Push `{ path, reason }` to the skip list. `reason` is a short message — first 200 chars of the underlying `OmpError`.
3. Continue with the next file.

The commit succeeds. The response payload includes:

```json
{
  "hash": "abc...",
  "reprobed": { "file_type": "text", "count": 4 },
  "skipped": [
    { "path": "huge.txt", "reason": "ProbeFailed: timeout exceeded 5s" }
  ]
}
```

The user can re-run `omp reprobe text` after fixing the issue (e.g. lifting a memory cap on the probe), or accept that some files don't have the new field.

This is consistent with the existing ingest-time philosophy: a single `repo.add` failing one file doesn't kill a batch upload (the user retries that file). Schema iteration shouldn't be more brittle than initial ingestion.

## Atomicity and concurrency

The single-writer-per-tenant lock from [`11-multi-tenancy.md`](./11-multi-tenancy.md) holds across the whole commit transaction, including reprobe. So:

- No other ingest can run while reprobe is in flight.
- The reprobe operates on a stable HEAD tree.
- The same lock protects the index file — staged manifests are written atomically into `.omp/index.json` (or the equivalent encrypted store).

The downside: long reprobes block other writes. For the demo this is fine. For a tenant with millions of files and a rebuild that takes minutes, you'd want a queue + status endpoint — see deferred section.

## Encrypted tenants

Per [`13-end-to-end-encryption.md`](./13-end-to-end-encryption.md), encrypted tenants do client-side ingest: the server never sees plaintext bytes or keys. Reprobe on the server can't run probes on plaintext it can't read.

Defensive behavior: when commit detects a staged schema change for an encrypted tenant (detected via tenant config, or via the same `OmpError::Unauthorized` from `paths::walk`), it logs `INFO` and skips reprobe. The schema commit itself succeeds; the client is responsible for orchestrating reprobe on its end (decrypt locally, re-run probes, re-encrypt manifests, push back). The server-side primitive in this doc isn't applicable.

This matches the pattern from [`20-server-side-probes.md`](./20-server-side-probes.md) where server-side compilation is also a plaintext-tenant concern.

## Opting out

Some workflows want to defer the cost (committing schema changes during a high-load period, then reprobing off-hours). For those:

```
OMP_DEFER_REPROBE=1
```

When set in the server's environment, `commit()` skips the auto-trigger entirely. The user runs `omp reprobe <file_type>` later to apply the change. This is a server-level setting, not per-request — it's an operator's escape hatch, not a per-tenant feature.

The explicit primitive `omp reprobe <file_type>` is also useful when a probe binary is replaced under the same qualified name. The schema's `Source` hasn't changed in shape (probe name is the same), but the probe's `framed_hash` is different — field-level reuse correctly fires. With a `--force` flag, reuse is bypassed and every probe-driven field is re-derived.

## Implementation status

Nothing yet. This is the design step.

- ⏸ Schema-diff detection in `Repo::commit`.
- ⏸ `Repo::reprobe_for_schema` + `Repo::reprobe_one`.
- ⏸ `engine::ProbeOutputCache` + `engine::resolve_field_with_cache`.
- ⏸ `commit()` response payload extension to include `reprobed` and `skipped`.
- ⏸ UI banner on the commit page.
- ⏸ `omp reprobe <file_type>` CLI primitive.

## What's deferred

- **Async/queue-backed reprobe.** A schema commit could enqueue a background job and return immediately. Per-tenant progress endpoint (`GET /reprobe/status`). Useful at scale; overkill for v1.
- **Per-file progress streaming.** SSE feed of `reprobe.file.completed` events as each file is rebuilt. Possible to add on top of the existing `commit.created` event mechanism in [`16-event-streaming.md`](./16-event-streaming.md). Deferred.
- **Manifest churn cleanup.** Every reprobe creates a new manifest object; the old one is still in the store, reachable from HEAD~1. The existing `gc.rs` walker doesn't reclaim old manifests because they're transitively referenced by every historical commit. To reclaim, you'd need a "rebase" operation that rewrites old commits to point at... but that breaks content addressing. The right answer is: tenants pay disk for history; if they want to compress, they `git gc`-style truncate history (also deferred).
- **Schema diff diagnostic UI.** A page that shows "old schema vs new schema, 1 field added (`contains_test_string_2`), no fields removed" before commit, with a confirm button. Possible enhancement; not blocking.
- **Per-tenant rate-limiting on reprobe scale.** A tenant with 100M files committing a schema change would block writes for hours. The single-writer lock prevents corruption but not starvation. Address with a quota (max files reprobed per commit) or by making it async. Deferred.
- **Configurable reprobe granularity.** Today: schema change on file_type X reprobes every file of type X. Future: a tenant-level setting like "reprobe only files matching prefix `reports/`" for partial application. Not requested; not designed.
