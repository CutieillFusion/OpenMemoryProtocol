# Multi-stage Dockerfile producing a single image with every OMP binary.
# The Helm chart in charts/omp/ uses different `command:` overrides on each
# Deployment to choose which binary to launch.
#
# See docs/design/17-deployment-k8s.md and docs/design/19-web-frontend.md.

# ----------------------------------------------------------------------------
# Stage 1: build the SvelteKit frontend that the gateway embeds via rust-embed.
# Lives in a separate stage so the Rust builder doesn't need Node at all.
# ----------------------------------------------------------------------------
FROM node:20-bookworm-slim AS web

WORKDIR /web
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

# ----------------------------------------------------------------------------
# Stage 2: build all Rust binaries.
# ----------------------------------------------------------------------------
FROM rust:1.94-slim-bookworm AS builder

# protoc is needed by tonic-build at compile time.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
        protobuf-compiler \
        pkg-config \
        libssl-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Pull the built UI from the web stage so rust-embed can find it. Setting
# OMP_SKIP_UI_BUILD=1 tells crates/omp-gateway/build.rs not to try invoking
# npm itself (Node isn't installed in this stage).
COPY --from=web /web/build /src/frontend/build
ENV OMP_SKIP_UI_BUILD=1

RUN rustup target add wasm32-unknown-unknown

# Compile and stage the starter-pack wasm probes that omp-core embeds
# via include_bytes! at compile time. build/wasm/ is gitignored, so we
# need to build it before invoking cargo on the workspace.
RUN bash scripts/build-probes.sh

RUN cargo build --release \
        -p omp-cli \
        -p omp-server \
        -p omp-store \
        -p omp-gateway \
        -p omp-builder

# ----------------------------------------------------------------------------

FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --create-home --shell /usr/sbin/nologin omp

COPY --from=builder /src/target/release/omp           /usr/local/bin/omp
COPY --from=builder /src/target/release/omp-server    /usr/local/bin/omp-server
COPY --from=builder /src/target/release/omp-store     /usr/local/bin/omp-store
COPY --from=builder /src/target/release/omp-gateway   /usr/local/bin/omp-gateway
COPY --from=builder /src/target/release/omp-builder   /usr/local/bin/omp-builder

# omp-builder needs the rustc toolchain at runtime (it shells out to cargo).
# Installed system-wide so any container user can invoke cargo. ~400 MiB
# of toolchain — only worth it on pods running the builder. The Helm
# chart's `builder` Deployment is the only consumer; other Deployments
# override `command:` and don't invoke cargo.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates build-essential \
 && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --target wasm32-unknown-unknown --profile minimal \
 && chmod -R a+rX,a+w /usr/local/cargo /usr/local/rustup \
 && rm -rf /var/lib/apt/lists/*

# Ship the probe-common path-dep alongside the binary so omp-builder can
# inject it into the per-build skeleton's Cargo.toml at runtime. The
# default is `/usr/local/share/omp/probe-common`; override with
# `omp-builder --probe-common-path`.
COPY --from=builder /src/probes-src/probe-common /usr/local/share/omp/probe-common

USER omp
WORKDIR /home/omp

# Documented ports: 8000 (omp-server), 8080 (omp-gateway), 9001 (omp-store).
EXPOSE 8000 8080 9001

# Default command runs the gateway with `--help`; the Helm chart overrides
# `command:` per Deployment.
ENTRYPOINT ["omp-gateway"]
CMD ["--help"]
