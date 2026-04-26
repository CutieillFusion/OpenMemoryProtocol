# 16 — Event streaming

Once the services from [`14-microservice-decomposition.md`](./14-microservice-decomposition.md) exist, two things follow naturally: writes happen in one place (refs / ingest) and reads happen in another (query, plus external consumers). An event broker between them lets every interested party react to commits without polling and without a tight RPC coupling.

This doc designs the event surface: what fires, what carries it, what subscribes, and what guarantees apply. Like every other doc in this series, it stays small on purpose.

## What events exist

Six event types, all keyed by `(tenant_id, ...)`. Tenant id is always part of the key so consumers can filter at the broker layer instead of in the consumer.

| Event | Producer | Carries | Use |
|---|---|---|---|
| `commit.created` | refs | `{tenant, branch, commit_hash, parent_hashes, paths_touched}` | query change-feed, downstream indexers, audit |
| `ref.updated` | refs | `{tenant, ref_name, old_hash, new_hash}` | replication, audit, consistency monitors |
| `manifest.staged` | ingest | `{tenant, path, manifest_hash, blob_hash, stage_id}` | dry-run preview, dev-time observability |
| `commit.failed` | refs | `{tenant, reason_code, idempotency_key, attempted_paths}` | client error reporting, alerting |
| `quota.exceeded` | gateway | `{tenant, quota_name, current, limit}` | tenant notification, ops alerting |
| `gc.completed` | refs (admin path) | `{tenant, objects_reclaimed, bytes_reclaimed}` | capacity tracking |

That's the complete v1 set. Adding a seventh later is additive — consumers ignore unknown event types.

`commit.created` is the load-bearing one. Most consumers care about it and nothing else.

## Topics and partitioning

One topic per event type, named `omp.<event-type>` (e.g., `omp.commit.created`). Within a topic, the partition key is **tenant id**. Consequences:

- All events for one tenant land on one partition, in order. Consumers process per-tenant streams sequentially without cross-tenant interleaving.
- Tenants distribute across partitions roughly uniformly (good as long as tenant population is itself roughly balanced).
- Adding more partitions later is a one-time rebalance, not a redesign.

We do **not** put events for different event types on one shared topic with a `type` discriminator. Consumers would have to filter; broker-side filtering on a `type` field across one topic is more expensive than topic-per-type and makes per-event retention impossible.

## Event envelope

Every event on the wire shares an envelope:

```jsonc
{
  "version": 1,
  "type": "commit.created",
  "tenant": "alice",
  "occurred_at": "2026-04-25T17:32:00.123Z",
  "trace_id": "0xabc...",     // see 18-observability.md
  "idempotency_key": "...",   // mirrors the producer's idempotency key when one exists
  "payload": { /* event-specific */ }
}
```

Format: protobuf on the wire (Kafka's Schema Registry, Connect, and the entire consumer-side ecosystem assume protobuf or Avro; protobuf wins because OMP's data plane already uses it). Schema-Registry-compatible IDs prefix each message so consumers can look up the schema without knowing it ahead of time.

Schemas for every event type live in a single file `proto/events.proto` — separate from `proto/store.proto` (which is the inter-service RPC contract from [`14-microservice-decomposition.md`](./14-microservice-decomposition.md)) but in the same `proto/` directory. Schema evolution follows the standard protobuf rules — additive only, no field reuse, version bump for breaking changes — same discipline as the TOML schemas in the tree.

Logs render the JSON projection (protobuf JSON mapping) for human-readable observability output.

## Delivery guarantees

**At-least-once** delivery on every topic. Consumers must be idempotent — which they naturally are, because every event carries a hash that uniquely identifies the work it represents. A retried `commit.created` for the same `commit_hash` is a no-op for any well-written consumer.

**Per-partition ordering** is preserved (which means per-tenant ordering, given the partitioning scheme). Cross-tenant ordering is undefined.

**Producer side**: every event is published only after the underlying state change has been committed durably. No "fire then write" — that gets ordering wrong on crashes. The producer accepts the cost of a synchronous publish (a few extra ms per commit) in exchange for the consumer never seeing an event for state that doesn't exist.

**No exactly-once.** Building exactly-once on top of at-least-once is trivial when consumers are idempotent and pointless otherwise.

## Broker choice

**Apache Kafka or Redpanda** for production. Both speak the Kafka protocol; Redpanda has a smaller operational surface (one binary, no ZooKeeper/JVM) and is the recommended dev/test deployment. NATS JetStream is the runner-up but carries fewer ecosystem tools.

The broker is a *deployment* dependency, not a runtime dependency of OMP itself. A monolithic `omp serve --no-broker` deployment skips publishing entirely; the change feed in [`15-query-and-discovery.md`](./15-query-and-discovery.md) falls back to in-process polling. This keeps local development cheap.

The broker is **not** in the critical path for reads. A failed broker affects observers (change feeds, replication, indexers) but does not stop ingest or commits. If the producer can't publish, it logs at WARN and continues — the underlying state is already durable. Consumers that come online later will miss events from any window where the broker was down; they can recover by replaying from `/log` over the affected commit range. That's a deliberate trade: durability of OMP's source-of-truth (the object store) is decoupled from durability of the event stream.

## Consumers

**Internal consumers shipped with OMP:**

- **Query service** — subscribes to `omp.commit.created` to drive watch endpoints and to invalidate manifest caches.
- **Audit log writer** — subscribes to all six topics, writes structured records to the audit sink (see [`18-observability.md`](./18-observability.md)).

**External consumers (out of scope for OMP, but unblocked by this doc):**

- A search-index sidecar that consumes commits and updates an inverted index or vector store. Lives outside OMP — different repo, different release cycle.
- A replication consumer that copies commits to a hot-standby OMP deployment for DR.
- A webhook fan-out service that translates events into HTTP callbacks for tenant-controlled endpoints. (This is the natural way to expose events to LLM agents that don't want to hold a long-lived `/watch` connection.)

The OMP repo will not ship these. It will ship one example consumer in `examples/` so the wire shape is testable.

## Auth on the broker

Producers authenticate with the broker using mTLS workload certs (same CA as the inter-service certs from [`14`](./14-microservice-decomposition.md)). Topics are partitioned by tenant; the broker does not enforce tenant-level ACLs. Consumers that join the cluster see all tenants' events.

This is intentional for v1 of the broker layer: every internal consumer is a trusted OMP service. When external consumers (webhook fan-out, third-party indexers) become a real shape, the gateway grows a webhook subsystem that subscribes broker-side and re-publishes to per-tenant HTTP endpoints — never giving external code direct broker access. This keeps the tenant boundary identical to the HTTP one.

## Replay and retention

**Default retention: 7 days.** Long enough for a downed consumer to catch up, short enough that the broker's disk usage stays predictable. Retention is per-topic so `commit.created` (load-bearing) and `quota.exceeded` (operational ephemera) can have different policies.

**Replay** is the broker's standard offset-rewind. A consumer that needs older history reconstructs from `/log` over the missing commit range — `/log` is durable forever, the broker is not.

This is the central trade: the **object store is the source of truth, the broker is a notification optimization**. Every event is reconstructible from the object store. The broker just makes "reconstruct" cheap and timely.

## What event streaming does *not* do

- **Does not run probes or schema validation.** Probes run in ingest, before the event is published. The event carries hashes, not raw input.
- **Does not move bytes.** Event payloads carry hashes; consumers fetch bytes from the object store if they need them.
- **Does not introduce a transaction across services.** OMP doesn't need distributed transactions because it's content-addressed and its writes are CAS'd; the broker just notifies after the fact.
- **Does not make ingest async to the client.** Clients still get a synchronous `200` from the gateway when their commit lands. The broker is downstream of the client response.
- **Does not replace the change feed in [`15-query-and-discovery.md`](./15-query-and-discovery.md).** The change feed is the *client-facing* API; it's implemented on top of the broker, not exposed as the broker.

## Fixed points this layer does *not* move

The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md) all hold. The broker carries metadata — hashes, names, timestamps — and never participates in canonical-bytes-of-an-object questions. The object model is unchanged whether or not the broker is running.
