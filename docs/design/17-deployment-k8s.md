# 17 — Deployment on Kubernetes

[`08-deployability.md`](./08-deployability.md) sketches Kubernetes deployment for the v1 monolith: one StatefulSet, one PVC, one Service, a `GET /status` liveness probe. This doc takes that further: once the services from [`14-microservice-decomposition.md`](./14-microservice-decomposition.md) exist and the broker from [`16-event-streaming.md`](./16-event-streaming.md) is in play, the deployment is a small graph of independently-scalable workloads, not a single pod.

The goal is a Helm chart that a grader (or a real operator) can `helm install` cold against a Kubernetes cluster and end up with a working multi-tenant OMP, the broker, and observability wired up out of the box.

## Implementation status

What ships in code today (see `Dockerfile`, `charts/omp/`, `scripts/k8s-smoke.sh`):

- ✅ **Multi-stage `Dockerfile`** producing one image with every binary (`omp`, `omp-server`, `omp-store`, `omp-gateway`). Per-component `command:` in the chart selects which binary runs.
- ✅ **Helm chart** with templates for the gateway (`Deployment` + `Service`), shards (`StatefulSet` per shard with PVC), and gRPC store (`StatefulSet` + headless `Service`). `values.yaml` + `values-dev.yaml` covers prod/dev. `helm lint` + `helm template` are green; verified with `helm template omp charts/omp -f charts/omp/values-dev.yaml`.
- ✅ **End-to-end kind smoke test** at `scripts/k8s-smoke.sh`: builds the image, creates a kind cluster, loads the image, `helm install`, waits for pods Ready, port-forwards the gateway, and verifies POST `/files` → POST `/commit` → GET `/files` round-trips through the gateway. **Verified working on 2026-04-25** with 4 pods (gateway + 2 shards + store) all reaching Ready and serving real traffic.
- ✅ **Liveness/readiness probes** wired to `/livez` and `/readyz` on the shards (per [`18-observability.md`](./18-observability.md)).

What's deferred from the design above:

- ⏸ **mTLS via cert-manager** — the chart leaves a hook for it but ships plain HTTP/HTTP2 inside the cluster trust boundary today.
- ⏸ **Network policies + PodSecurity restricted profile** — designed; not yet templated.
- ⏸ **Prometheus ServiceMonitor + Loki/Tempo sub-charts** — Phase 5 work; the per-pod `/metrics` endpoint is already there.
- ⏸ **Broker (Redpanda) sub-chart** — bundled in the design; the in-cluster bus is currently in-process per `omp-server` replica (see [`16-event-streaming.md`](./16-event-streaming.md)).

## Workload shapes

| Component | K8s kind | Replicas | Why |
|---|---|---|---|
| Gateway | `Deployment` | 2+ | Stateless; horizontally scalable; behind a `Service` of type `LoadBalancer` (or `Ingress`). |
| Ingest | `Deployment` | 2+ | Stateless; CPU-heavy; replicas scale with probe load. |
| Object store | `Deployment` (S3/Postgres backend) or `StatefulSet` (disk backend) | 1+ | Stateless when wrapping S3/Postgres; stateful when wrapping a PVC. |
| Refs | `StatefulSet` | 1 (sharded later) | Holds per-tenant write locks; horizontal scale needs distributed locking — see "Scaling refs" below. |
| Query | `Deployment` | 2+ | Stateless cache; replica count scales with read QPS. |
| Broker (Redpanda) | `StatefulSet` | 3 | External dependency; quorum requires odd count. |
| Tenant registry | bundled into gateway | — | Not a separate workload. |

The gateway, ingest, and query Deployments support `HorizontalPodAutoscaler`s on CPU and request rate. Refs is intentionally not horizontally scaled in v1 of the K8s deploy — see below.

## Resource envelopes

Initial resource requests/limits, sized from the numbers in [`08-deployability.md`](./08-deployability.md):

```yaml
gateway:    { requests: { cpu: 100m, memory: 64Mi },  limits: { cpu: 500m,  memory: 256Mi } }
ingest:     { requests: { cpu: 500m, memory: 256Mi }, limits: { cpu: 2000m, memory: 1Gi  } }
object_store_s3:    { requests: { cpu: 100m, memory: 64Mi  }, limits: { cpu: 500m,  memory: 256Mi } }
object_store_disk:  { requests: { cpu: 200m, memory: 128Mi }, limits: { cpu: 1000m, memory: 512Mi } }
refs:       { requests: { cpu: 200m, memory: 128Mi }, limits: { cpu: 1000m, memory: 512Mi } }
query:      { requests: { cpu: 200m, memory: 256Mi }, limits: { cpu: 1000m, memory: 1Gi  } }
broker:     { requests: { cpu: 500m, memory: 1Gi   }, limits: { cpu: 2000m, memory: 4Gi  } }
```

Ingest's memory limit dominates because each concurrent probe sandbox can spike to 64 MB. Limit divided by sandbox size sets the per-pod ingest concurrency ceiling.

## Networking and Services

```
                               ┌─────────────┐
                Internet ─────▶│ LoadBalancer│
                               └──────┬──────┘
                                      │
                                  ClusterIP
                                      │
                                  ┌───▼────┐
                                  │gateway │
                                  └─┬──┬──┬┘
            ┌─────────────────────┘  │  │
            │                        │  └────────┐
            │ ClusterIP              │ ClusterIP │ ClusterIP
        ┌───▼────┐               ┌───▼───┐  ┌────▼───┐
        │ ingest │               │ query │  │  refs  │
        └────┬───┘               └───┬───┘  └────────┘
             │                       │             ▲
             │  ClusterIP            │             │
        ┌────▼────────┐              │             │
        │ object_store│◀─────────────┘             │
        └─────────────┘                            │
             ▲                                     │
             └─────────────────────────────────────┘
                                      │
                                 ┌────▼────┐
                                 │ broker  │
                                 └─────────┘
```

Internal services use `ClusterIP`. Only the gateway is externally reachable. mTLS between services uses cert-manager-issued certs from a private CA (chart includes the `Issuer` resource).

## Persistence

- **Object store with disk backend**: `StatefulSet` with one PVC per replica. Storage class is provider-specific (gp3 on AWS, pd-ssd on GCP). The chart exposes `objectStore.storage.size` and `objectStore.storage.class`.
- **Object store with S3 backend**: no PVC. Credentials come from a `Secret` referenced via `envFrom`. `Deployment`, not `StatefulSet`.
- **Object store with Postgres backend**: no PVC. `DATABASE_URL` from a `Secret`. The chart does *not* manage the Postgres instance — that's a value (`objectStore.postgres.host`) that points at a managed Postgres service or a separate operator (Zalando, CrunchyData).
- **Refs**: a small PVC for the tenant lock state and idempotency-key window. ~1 GB is plenty for hundreds of tenants.
- **Broker (Redpanda)**: PVC per replica via Redpanda's own operator chart, included as a sub-chart dependency.

## Probes

Every workload exposes:

- **Liveness** (`/livez`): returns 200 if the process is responsive. Restarts the pod on failure.
- **Readiness** (`/readyz`): returns 200 only if downstream dependencies (object store, refs, broker if required) are reachable. Removes from `Service` endpoints on failure but does not restart.
- **Startup** (`/startupz`): only on workloads that warm caches (query) or replay logs (refs); generous failure threshold.

These three are distinct: a startup probe failing during cache warmup must not trigger a restart loop; a readiness probe failing must not restart the pod (otherwise rolling deploys cascade); a liveness probe failing must restart.

The probes are HTTP endpoints on the same port as the gRPC server (gateway also has its public HTTPS port) using the `tower-http` `health` layer.

## Configuration and secrets

- A single `ConfigMap` per workload type for non-sensitive config (bind address, log level, broker URL, sample rates).
- A single `Secret` per workload type for sensitive config (mTLS cert/key, S3/Postgres credentials, broker SASL if used, gateway signing key).
- Tenant tokens: managed entirely outside the chart. `omp admin tenant create <name>` runs as a `Job` against the gateway and writes tokens to stdout for the operator to distribute.
- The gateway's Ed25519 signing key (for the signed tenant context from [`14`](./14-microservice-decomposition.md)) lives in its `Secret`. Rotation is a key-version field in the signed context; multiple keys can validate concurrently during a rotation window.

## Scaling refs

Refs is the one component that doesn't scale horizontally in v1 of the K8s deploy, because it owns per-tenant write locks. Three options for the future, in increasing order of work:

1. **Vertical scale**: bigger pod, more concurrent tenant locks. Trivial. Sufficient for hundreds of tenants.
2. **Tenant-sharded StatefulSet**: N refs replicas, each owning a hash range of tenants; gateway routes by tenant id. Locks remain in-process per replica. Adds a routing layer; rebalancing is a planned operational event.
3. **Distributed locks**: locks move to Postgres advisory locks or Redis. Refs becomes stateless; scales like the others. More moving parts; only worth it past the point where (2) gets uncomfortable.

The chart ships option (1) with values that would let you migrate to (2) without redeploying clients. Option (3) is a later design.

## Observability wiring

The chart includes optional sub-charts (off by default, on with `observability.enabled=true`):

- **Prometheus** scraping `/metrics` on every workload. ServiceMonitors are pre-defined.
- **Loki** for log aggregation; every workload logs structured JSON to stdout, picked up by the cluster log forwarder.
- **Jaeger** (or Tempo) for traces. Every gRPC call carries OpenTelemetry context; sampling is gateway-decided.

See [`18-observability.md`](./18-observability.md) for what's instrumented and how.

## Helm chart structure

```
charts/omp/
  Chart.yaml
  values.yaml
  values-prod.yaml              # production overrides (replicas, resources)
  values-dev.yaml               # single-replica everything, disk backend
  templates/
    gateway/                    # Deployment, Service, HPA, ConfigMap, Secret
    ingest/
    object-store/               # one of: disk-statefulset.yaml, s3-deployment.yaml, postgres-deployment.yaml
    refs/
    query/
    broker/                     # Redpanda sub-chart values
    cert-manager/               # Issuer + Certificate resources
    network-policies/           # default-deny + per-workload allow
    podsecurity/                # PSP / PodSecurity Standards "restricted"
  charts/
    redpanda/                   # vendored sub-chart
```

Two named values files (`-prod`, `-dev`) cover the common cases. Production sets `objectStore.backend=s3`, replicas at 2+, observability on; dev sets `objectStore.backend=disk`, replicas at 1, observability off.

## Network policies

Default-deny on every namespace. Per-workload `NetworkPolicy` allowing only:

- Gateway: ingress from `Ingress`/`LoadBalancer`; egress to ingest, query, refs, object-store, broker, observability.
- Ingest: egress to object-store, refs, broker.
- Query: egress to object-store, refs, broker.
- Refs: egress to object-store, broker.
- Object-store: egress to whatever its backend is (S3/Postgres/disk has none).
- Broker: ingress from gateway, ingest, query, refs, audit consumer; egress none.

This makes "ingest accidentally talks to the public internet" structurally impossible.

## Pod security

`PodSecurity: restricted` namespace label. Every workload runs as non-root, with read-only root filesystem, no privilege escalation, no host namespaces, and a seccomp default profile. Ingest's wasmtime sandbox provides probe-level isolation; the K8s `restricted` profile provides container-level isolation. Two layers because probes are user-supplied content.

## Dev-mode deployment

`helm install omp ./charts/omp -f values-dev.yaml` brings up:

- 1× gateway, 1× ingest, 1× object-store (disk + 10 GiB PVC), 1× refs, 1× query, 1× broker (single-node Redpanda).
- Self-signed mTLS certs (cert-manager generates them on install).
- One pre-created tenant `dev` with a printed token.
- Total ~2 GB RAM, fits on a developer's `kind` or `minikube` cluster.

The dev-mode install **is** the demo: it's what a grader runs to see the system end-to-end. The `scripts/demo-multi-tenant.sh` already in the repo is rewritten to run against this deploy.

## What this layer does *not* do

- **Does not introduce a service mesh.** Istio/Linkerd would simplify mTLS but add operational surface. The chart issues mTLS certs directly; consider a mesh later.
- **Does not run a managed Postgres.** Postgres is a value, not a workload. Operators bring their own.
- **Does not manage tenant lifecycle.** Creating tenants, distributing tokens, billing — all out of scope.
- **Does not encrypt at rest beyond what the storage class provides.** Tenant-level encryption is the job of [`13-end-to-end-encryption.md`](./13-end-to-end-encryption.md), not the K8s layer.
- **Does not solve cluster federation.** Multi-region, multi-cluster OMP is a deferred concern.

## Fixed points this layer does *not* move

K8s changes deployment, not protocol. The five fixed points from [`10-why-no-v2.md`](./10-why-no-v2.md) hold. A pod's death loses no committed state — every commit is durable in the object store and every ref is in refs' PVC (or in the Postgres backend) before any client gets a `200`.
