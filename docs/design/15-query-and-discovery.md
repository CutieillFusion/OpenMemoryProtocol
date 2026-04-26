# 15 — Query and discovery

[`00-overview.md`](./00-overview.md) opens with **Bet 1**: an LLM agent can browse OMP by reading manifests only — cheap, fast, no content loading. The current API in [`06-api-surface.md`](./06-api-surface.md) delivers that for trees of dozens or hundreds of files. It does **not** deliver it for thousands or tens of thousands, because the only listing primitives are a flat `GET /files` and a one-directory `GET /tree/{path}`. There is no way for an agent to ask "give me the PDFs tagged `policy` from last quarter" without pulling every manifest and filtering client-side.

This doc designs a small query and discovery layer that makes Bet 1 hold at hosted-multi-tenant scale. It is deliberately *not* a search engine, *not* an index of file content, *not* a vector store. Embeddings and full-text search remain deferred per [`09-roadmap.md`](./09-roadmap.md).

## Three capabilities, no more

The minimum viable shape:

1. **Predicate filtering** over manifest fields. "files where `tags ∋ 'policy'` and `pages > 10`."
2. **Cursor pagination** so a result set bigger than one page doesn't blow up either side.
3. **Change feed** so an agent that has already walked the tree can react to new commits without re-walking.

Everything else (sort, group-by, joins) is out of scope. If an agent needs more, it issues several queries and composes client-side.

## Predicate language

A small expression grammar that maps cleanly to TOML field paths and doesn't accidentally become SQL:

```
expr     := atom | atom AND expr | atom OR expr | NOT atom | (expr)
atom     := <field-path> <op> <literal> | exists(<field-path>)
field-path := identifier ("." identifier)*    e.g.  pages, source.mime, tags
op       := = | != | < | <= | > | >= | contains | starts_with
literal  := string | int | float | bool | null
```

`contains` is two things depending on the field's TOML type: substring on strings, membership on arrays. No regex (regex is the foot-cannon you can't take back). No subqueries. No `LIKE`.

A predicate compiles to a boolean function over a manifest's parsed TOML — no SQL engine, no separate index format. Each manifest is filtered in-process by the query service.

Examples:

```
file_type = "pdf" AND pages > 10
tags contains "policy" AND author starts_with "Alice"
exists(transcript) AND duration_seconds > 600
```

## HTTP shape

One new endpoint, plus extensions to the two existing ones:

| Method | Path | Description |
|---|---|---|
| `GET` | `/query` | Predicate query over manifests. `?where=<expr>&prefix=<path>&at=<ref>&cursor=<c>&limit=<n>`. Returns `{matches: [...], next_cursor: "..." \| null}`. |
| `GET` | `/files` | Existing flat list, now with `?where=<expr>` (subset of `/query`'s grammar) and `?cursor=` / `?limit=`. |
| `GET` | `/tree/{path}` | Existing directory list, with `?cursor=` / `?limit=` for very wide directories. |

All accept `?at=<ref>` for time-travel queries. Time-travel through Bet 1 is the demo moment from [`00-overview.md`](./00-overview.md), and predicates make it actually useful: "what did the LLM tag as `policy` on April 10 vs. April 21?"

`/query` returns each match as `{path, manifest_hash, source_hash, file_type, fields}` — the same shape as `/files` plus a `fields` projection so the agent doesn't immediately have to issue N follow-up `GET /files/{path}` calls. A `?fields=` query parameter narrows the projection (`?fields=title,tags,pages`) to keep response size bounded.

## Cursor pagination

Cursors are opaque base64-encoded strings. Internally they encode `(commit_hash, walk_position)` so a paginated walk is reproducible across requests even if new commits land between pages — the walk is anchored to the commit that produced the first page. Clients that want fresh data on every page pass `?at=HEAD` and accept that they may see partial overlap; clients that want a stable snapshot follow the cursor.

`limit` defaults to 100, max 1000. Pages cap at 1 MB serialized regardless of `limit`.

## Watch / change feed

```
GET /watch?since=<commit>&where=<expr>
```

A long-lived HTTP response (server-sent events, one event per line) emitting `{commit, path, change_type, manifest_hash}` for every file change matching the predicate, since the cursor commit. Disconnections are normal; clients reconnect with the last commit they saw.

Implementation note: the change feed is a projection of the event stream from [`16-event-streaming.md`](./16-event-streaming.md). The query service subscribes to commit events, filters per-watcher, and forwards. An agent does not have to know the broker exists.

For local-monolith deployments without a broker, the same endpoint is implemented by polling `/log` internally — the wire protocol doesn't change.

## Why query is its own service

[`14-microservice-decomposition.md`](./14-microservice-decomposition.md) gives the query service its own pod for three reasons:

- **Different scaling profile.** Read-heavy, latency-sensitive, no write lock contention. Independent replica count from ingest and refs.
- **Cache locality.** A working set of recently-walked manifests stays warm in the query service's memory; ingest and refs don't share that working set.
- **Failure isolation.** A pathological predicate over a million-manifest tree degrades query without affecting ingest or commit.

The query service holds only cache state. It can be killed and restarted at any time; the only consequence is a cold cache for a few seconds.

## Indexing — what is and isn't built

**v1 of this layer: no index.** Query walks trees and parses manifests on the fly, with two caches:

1. **Manifest LRU** — parsed TOML cached by hash. Hot manifests are free.
2. **Tree LRU** — parsed tree objects cached by hash. The walk doesn't re-parse the same nested tree on every page.

A tree of 10,000 files with 100-byte manifests fits in tens of megabytes; full walks are fast. Predicate evaluation is cheap because manifests are already structured TOML.

**Iteration 2 of this layer (deferred until v1 of the layer is exercised):** an inverted index per (tenant, file_type, field), persisted as its own object kind in the store. Adds an index-update step to ingest's commit path. Buys order-of-magnitude latency improvement on the queries that hit indexed fields. Stay aware of the cost: an index commits to a query shape, and Bet 2 of OMP is that schemas (and therefore fields) are data the user can change. The deferred work is "add an index that *follows* schema changes," not "design the index now."

## What query does *not* do

- **No full-text search.** "Find documents mentioning Argentina" is not in scope. That's an embedding-search feature, deferred per [`09-roadmap.md`](./09-roadmap.md).
- **No cross-tenant query.** Tenant boundary from [`11-multi-tenancy.md`](./11-multi-tenancy.md) holds. A query carries a tenant context and only sees that tenant's manifests.
- **No content-addressed query.** "Files where the blob hash matches X" is just `iter_refs` + walk; no special endpoint needed.
- **No write side.** Query is read-only by construction. All mutations go through ingest/refs.
- **No SQL.** The predicate grammar is small on purpose — extending it is the temptation that grows the surface area until it resembles SQLite, and at that point the right move is to use SQLite, not to reinvent it.

## Reconciling Bet 1

With this layer in place, Bet 1 reads honestly:

> An LLM agent can browse the store by reading manifests only — cheap, fast, no content loading. Predicate filtering, pagination, and a change feed make this true at the scale of tens of thousands of manifests per tenant; embedding-based retrieval is deferred.

That's the line `00-overview.md` should say once this doc lands. Until then, the bet is bigger than the API.

## Fixed points this layer does *not* move

- SHA-256 / framing / `ObjectStore` / four field sources / WASM probe ABI — all unchanged. Query reads through the object store like every other service.
- The predicate grammar is *additive* to the API. Clients that don't pass `?where=` get the existing behavior. The bet survives even if the grammar is later replaced.
