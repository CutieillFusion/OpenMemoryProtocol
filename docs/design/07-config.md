# 07 — Config

OMP distinguishes two kinds of configuration, and they live in different places for a specific reason.

## The two kinds

### Versioned repo config — `omp.toml`

Lives at the **repo root** as `omp.toml`. Tracked in the tree. Changes are committed. Time-travels with the rest of the repo.

Used for anything that is **semantically part of the repo's identity** — decisions that should travel with clones, show up in diffs, and be consistent across checkouts of the same commit.

### Machine-local config — `.omp/local.toml`

Lives inside `.omp/`, which is private and un-versioned. Tracked by nothing. Changes don't show up in `git status`–style listings and are never committed.

Used for anything that is **about the machine running OMP**, not about the repo itself — bind address, default author identity, paths to local caches.

## Why the split matters

Git doesn't have versioned config — everything in `.git/config` is machine-local. OMP diverges because schema policy, ignore patterns, and default behavior are **repo-level decisions that the LLM agent needs to see and reason about**. An agent checking out an old commit should see the config that was in effect then.

Meanwhile, the machine-local config exists for the same reason Git's `.git/config` does: a local server's bind address, the user's name, API tokens — these aren't the repo's business and shouldn't travel.

## `omp.toml` — versioned

Example:

```toml
[ingest]
# What to do when ingesting a file whose MIME type matches no schema.
# "reject"  — error out; force the user to define a schema first
# "minimal" — build a minimal manifest with only file.* probes
default_schema_policy = "reject"

# Whether unknown file types are allowed to be committed as plain blobs
# even if no schema matches. Default false.
allow_blob_fallback = false

[workdir]
# Patterns skipped during tree walk. Globs in gitignore syntax.
ignore = [
  ".git/",
  "node_modules/",
  "*.log",
  "*.tmp",
  "__pycache__/",
]

# Whether to follow symlinks when walking.
follow_symlinks = false

[probes]
# Default resource caps for every WASM probe execution (see 05-probes.md).
# A probe's own .probe.toml may lower these; it cannot raise them.
memory_mb = 64
fuel = 1_000_000_000
wall_clock_s = 10
```

All sections are optional. OMP has defaults for everything.

The schemas directory (`schemas/`) and schema filename pattern (`<type>.schema`) are hardcoded conventions in v1 — making them configurable is deferred so the tree walker and schema loader can assume them unconditionally.

### What does NOT go in `omp.toml`

- API keys (machine-specific, never versioned)
- LLM provider config (OMP doesn't call LLMs)
- Server bind address (machine-specific)
- Author identity (machine-specific)
- Local filesystem paths (machine-specific)

## `.omp/local.toml` — machine-local

Example:

```toml
[server]
# Where the HTTP server binds when `omp serve` runs.
bind = "127.0.0.1:8000"

[author]
# Default author identity stamped on commits made from this machine.
name = "claude-code"
email = "claude@local"

[cache]
# Optional: where OMP caches probe results between runs.
# Defaults to `.omp/cache/` if unspecified.
dir = ".omp/cache/"
```

Rarely edited by the user directly. `omp init` creates a skeleton; commands like `omp config` (deferred to post-v1) let you edit it safely.

### Environment variable overrides

Any machine-local setting can be overridden by an environment variable for ephemeral runs (e.g., Docker containers):

```
OMP_SERVER_BIND=0.0.0.0:8000
OMP_AUTHOR_NAME="research-agent"
OMP_AUTHOR_EMAIL="agent@example.com"
```

The precedence is: env var > `.omp/local.toml` > built-in default.

## Loading order

On startup:

1. Read `.omp/local.toml` (if present). Any missing keys get built-in defaults.
2. Apply environment variable overrides.
3. Read `omp.toml` at the tree root of HEAD (if present). Merge its values over the machine defaults.
4. For commands operating on a specific commit (`--at`), re-read `omp.toml` from that commit's tree before executing.

This means **the repo config in effect at a historical commit is the `omp.toml` as of that commit** — which is what makes time-travel self-consistent.

## Handling `omp.toml` changes

An `omp.toml` edit is staged like any other file change — `POST /files path=omp.toml file=@omp.toml` or the CLI `omp add omp.toml` equivalent. On `POST /commit`, it's included in the commit.

Because `omp.toml` is a `blob` in the tree (not a manifest), it has no schema and no ingest step. It's read as plain TOML by OMP's config loader.

## Why two files, not one

A single unified config file would force us to either:

- Version everything (including API keys — security disaster).
- Version nothing (lose the core design benefit — LLM sees historical config).

Splitting into a versioned "repo config" and an un-versioned "machine config" gives us both properties cleanly. It's the same decision Git made with `.gitignore` (versioned) vs. `.git/config` (local) — we're just applying it to a second config surface.
