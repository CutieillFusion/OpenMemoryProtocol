# OMP Architecture (High Level)

```mermaid
flowchart LR
    Caller["LLM Agent / Caller<br/>(external)"]

    subgraph OMP["OpenMemoryProtocol"]
        API["API Layer<br/>(HTTP / CLI)"]
        Core["omp_core<br/>(commit, manifest build,<br/>schema resolve)"]
        Probes["Probes<br/>(WASM, sandboxed)"]
        Store[("Object Store<br/>loose objects + trees<br/>+ refs + schemas")]
    end

    Caller -->|put / get / log| API
    API --> Core
    Core -->|run extractors| Probes
    Core <-->|read / write objects| Store
    Probes -->|fields| Core
    Core -->|file + manifest| API
    API --> Caller
```

## What this shows

- **Caller** is anything outside OMP (an LLM agent, a script, a service). OMP itself never calls an LLM.
- **API Layer** is a thin wrapper — both HTTP and CLI dispatch into the same core.
- **omp_core** holds all the logic: hashing bytes, resolving the right schema version, building the manifest, writing commits.
- **Probes** are deterministic WASM modules that extract fields (e.g. `text.word_count`, `pdf.page_count`). They are content in the store, not built-in code.
- **Object Store** holds everything Git-style: blobs, trees, commits, refs, schemas, and probe binaries — all as content-addressed loose objects.

Every stored file ends up as `(bytes, manifest)` where the manifest is assembled from five field sources: `constant`, `probe`, `user_provided`, `field`, `fallback`.
