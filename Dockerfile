# Multi-stage Dockerfile producing a single image with every OMP binary.
# The Helm chart in charts/omp/ uses different `command:` overrides on each
# Deployment to choose which binary to launch.
#
# See docs/design/17-deployment-k8s.md.

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
RUN cargo build --release \
        -p omp-cli \
        -p omp-server \
        -p omp-store \
        -p omp-gateway

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

USER omp
WORKDIR /home/omp

# Documented ports: 8000 (omp-server), 8080 (omp-gateway), 9001 (omp-store).
EXPOSE 8000 8080 9001

# Default command runs the gateway with `--help`; the Helm chart overrides
# `command:` per Deployment.
ENTRYPOINT ["omp-gateway"]
CMD ["--help"]
