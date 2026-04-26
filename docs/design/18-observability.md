# 18 — Observability

A multi-tenant hosted OMP deployment is a small distributed system. When something is slow, the operator needs to know whether it's the gateway, ingest, the object store, the broker, or a particular tenant; when something fails, the operator needs to reproduce what the client sent and what each service did with it. This doc designs the three observability planes — **logs, metrics, traces** — plus the **audit log**, which is structurally similar but serves a different audience.

The bar is "an operator can answer, in under five minutes, the questions: which tenant is hot? which service is slow? what did this commit do?" If the answer takes longer, the instrumentation isn't sufficient.

## Three planes, one audit channel

| Plane | Format | Backend | Retention | Audience |
|---|---|---|---|---|
| Logs | Structured JSON to stdout | Loki / ELK / journald | 30 days | Operators debugging incidents |
| Metrics | Prometheus exposition | Prometheus / Mimir | 90 days | Operators tracking trends, alerting |
| Traces | OpenTelemetry, OTLP | Jaeger / Tempo | 7 days | Operators debugging request paths |
| Audit | Structured JSON | Append-only object kind in the store | Forever (per-tenant retention configurable) | Tenants reviewing their own activity, compliance |

The first three are operator-facing. The audit log is tenant-facing. It deliberately lives **inside** the object store rather than in Loki because tenants need to read it through the same auth path as everything else, and because it carries multi-year retention that operator log storage isn't sized for.

## Logs

Every service emits structured JSON to stdout with a fixed key set:

```jsonc
{
  "ts": "2026-04-25T17:32:00.123Z",
  "level": "info",                // trace | debug | info | warn | error
  "service": "ingest",            // gateway | ingest | object_store | refs | query
  "tenant": "alice",              // omitted for service-level events
  "trace_id": "0xabc...",         // matches the trace plane
  "span_id":  "0xdef...",
  "event": "manifest.assembled",  // dotted-namespace name; one event per code call site
  "msg": "assembled manifest for path",
  "fields": { "path": "...", "manifest_hash": "...", "probe_count": 4, "duration_ms": 142 }
}
```

**Rules:**

- One event per call site, named in dotted namespace (`service.subsystem.action`). Naming events as data lets log queries grep by event name regardless of the human-readable `msg`.
- `tenant` is set on every event that occurs inside a request. Service-level events (startup, shutdown, GC) omit it.
- Never log token values, plaintext secrets, or full file bytes. Hashes only. The encryption design from [`13`](./13-end-to-end-encryption.md) makes plaintext unavailable on the server anyway, but the rule applies even where it's redundant.
- `level` defaults to `info` in production. `debug` and `trace` are off by default; flipped per-tenant via a runtime config knob, scoped to a time window so they don't pile up forever.

PII handling: tenant ids are not PII unless a deployment chooses tenant ids that are. Names that *are* PII (file paths in user trees, manifest field values) are logged as hashes when log retention exceeds the deployment's compliance window. This is a deployment-level config switch — strict mode hashes paths, default mode logs them.

## Metrics

Each service exposes `/metrics` in Prometheus exposition format. The metric set is small on purpose; ten well-chosen series beat fifty noisy ones.

**Per-service RED metrics** (Rate, Errors, Duration), labeled by `tenant`, `route` (gateway) or `op` (gRPC services), and `status_code`:

- `omp_request_total{service, op, tenant, status}` — counter
- `omp_request_duration_seconds{service, op, tenant}` — histogram
- `omp_request_in_flight{service, op}` — gauge

**Per-service resource metrics**, labeled by `service`:

- `omp_object_store_bytes{tenant}` — gauge, sum of stored bytes per tenant
- `omp_object_count{tenant}` — gauge
- `omp_probe_fuel_consumed_total{tenant, probe}` — counter, drives quota visibility
- `omp_wasm_sandbox_active{service}` — gauge, only on ingest
- `omp_quota_exceeded_total{tenant, quota_name}` — counter

**Broker metrics** come from the broker's own exporter; we don't reinvent them.

The `tenant` label is a cardinality concern at scale (hundreds of tenants × dozens of metrics × multiple buckets). Mitigations:

1. Histograms use a small fixed bucket set: `[5ms, 25ms, 100ms, 500ms, 2s, 10s, 60s]`. Seven buckets, not the Prometheus default of fifteen.
2. Below ~500 tenants, label freely. Above, the gateway aggregates low-traffic tenants into an `__other__` bucket and only top-N tenants get their own series. The cutoff is a deployment config.
3. `quota_exceeded` and `request_total` are cheap (no histogram), so they keep full per-tenant labels regardless.

This is the part of observability where "design" matters: metrics with bad labels become unfixable in production because dashboards and alerts depend on them. Better to undershoot here.

## Traces

OpenTelemetry, OTLP exporter, instrumented at the gRPC and HTTP layers (via `tower-http` and `tonic` middleware so it's not per-handler). The gateway makes the sampling decision per request:

- 100% on requests that take longer than 1 second.
- 1% baseline.
- 100% on any request that returns a 5xx error.

The decision is encoded in the trace context propagated to downstream services so they don't independently sample (which would produce broken traces).

**Span coverage:**

- Gateway: one span per HTTP request, child spans per gRPC call out.
- Ingest: spans for `add`, `validate_schema`, `run_probe` (one per probe), `assemble_manifest`, `put_blob`, `put_manifest`, `stage_change`.
- Refs: spans for `read_ref`, `write_ref_cas`, `commit`.
- Object store: one span per backend call (`put`, `get`, `iter_refs`).
- Query: one span per query, child spans for the manifest walk and predicate evaluation.

Span attributes carry hashes, never bytes. Probe spans carry the probe's `(namespace, name, version)` so a slow probe is identifiable from the trace.

The probe sandbox is *inside* the ingest span — wasmtime fuel consumption shows up as a span attribute. This is the one place where trace data really pays back: "why was this commit slow?" almost always reduces to "which probe took 3 seconds on which file?"

## Audit log

A separate channel because audiences are different: operators have access to logs; tenants have access only to the audit log for their own tenant.

**What's audited (every write-side event):**

- Successful and failed authentications (token presented, accepted/rejected, source IP).
- Tenant context issuance (which token id, which tenant, which IP).
- Every state-changing API call: `add`, `patch_fields`, `remove`, `commit`, `branch`, `checkout`, plus the admin ops.
- Every quota event.
- Every encryption-related operation: identity creation, recipient wraps, share grants and revocations (per [`13`](./13-end-to-end-encryption.md)).

**Where it lives.** A new object kind in the store, `audit`, append-only per tenant. Each entry is a CBOR blob; entries are content-addressed (which gives free integrity verification — the audit log can prove it hasn't been edited). The most recent audit head is a tenant ref `refs/audit/HEAD`; entries form a hash-linked chain.

That puts the audit log on the same durability and query path as the rest of OMP. Tenants read their audit log via `GET /audit?since=<ts>&where=...` (the predicate grammar from [`15`](./15-query-and-discovery.md), reused). The query service serves it.

**Retention.** Per-tenant config; default forever. Tenants who need to comply with deletion requests can rotate their audit ref to a truncated chain. The truncation operation itself is audited — recursion that has to bottom out somewhere; the bottom is a known event of type `audit.rotated` that the operator can verify.

**Off-host audit (deployment option).** For deployments where the host operator should not be able to see or alter the audit log, audit entries are sealed with a tenant-side key on write. This composes with [`13`](./13-end-to-end-encryption.md) — the audit log is just another stream of objects subject to the same encryption rules. Optional because the cost is real (audit becomes opaque to operator-side investigation tools).

## Health endpoints

Distinct from metrics, distinct from each other (per the K8s probe taxonomy in [`17-deployment-k8s.md`](./17-deployment-k8s.md)):

- `/livez` — process is responsive. Returns 200 unless the process is wedged. Drives liveness probe / restart.
- `/readyz` — process is ready to serve. Checks downstream dependencies (object-store reachable, refs reachable, broker reachable if required for the workload). Drives readiness probe / load balancer membership.
- `/startupz` — process has finished startup work (cache warm-up, log replay). Drives startup probe / delays liveness.
- `/status` — backwards-compat endpoint from v1, identical to `/readyz` for clients that already use it.

Each returns a JSON body with per-dependency check results so an operator can curl the failing replica and see *why* it's not ready.

## Alerting (sketch, not a rule set)

The chart from [`17`](./17-deployment-k8s.md) ships PrometheusRule resources for a small alerting starter pack:

- **High-priority**: any 5xx rate > 1% for 5 minutes, gateway p99 latency > 2s for 10 minutes, refs CAS-conflict rate > 10/min (indicates a livelock), broker unavailability for any producer.
- **Medium-priority**: per-tenant quota approaching (90%) for 1 hour, per-tenant 4xx rate > 50% for 30 minutes (tenant likely sending bad input — page their support, not yours), object-store backend latency p99 > 1s for 15 minutes.
- **Low-priority / dashboards only**: GC-eligible bytes growing, probe-fuel consumption shifting, audit log size growth.

Alert rules are starter-pack opinions, not contracts; operators tune them.

## Cost shape

Observability is a cost. Approximate per-replica steady state:

- Logs: ~5–20 MB/hour structured JSON per service, dominated by request logs. Loki ingestion cost is the line item that surprises people.
- Metrics: ~50 KB/scrape per service at moderate cardinality; one scrape every 30s ⇒ ~6 MB/hour. Cardinality is the hidden multiplier — see the metrics section.
- Traces: with 1% sampling + tail sampling, ~1 MB/hour per service. Tracing is cheap if sampling is right.
- Audit: ~1–10 KB per audited event; tenants drive volume. The audit log lives in the object store so it amortizes against the same storage budget as everything else.

These are budgeted-for, not free. The chart's `observability.enabled=false` switch exists so dev clusters and graders running locally don't pay this cost.

## What this layer does *not* do

- **Does not log file contents.** Hashes only.
- **Does not export to vendor APMs by default.** The OTLP and Prometheus formats are vendor-neutral; integrating with Datadog, New Relic, Honeycomb, etc., is a deployment-side adapter.
- **Does not replace the broker.** The audit log is a *separate* durability story. Events on the broker have 7-day retention; audit entries are forever.
- **Does not solve log-based alerting.** Alerts come from metrics. Logs answer questions; metrics answer "is something wrong right now?"
- **Does not provide RUM or client-side telemetry.** Observability stops at the OMP boundary.

## Fixed points this layer does *not* move

The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md) hold. Observability is metadata about object operations; it never changes the bytes or their hashes. The audit log is built *out of* the object model, not bolted onto it — same framing, same content addressing, same backend.
