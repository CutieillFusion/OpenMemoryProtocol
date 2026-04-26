#!/usr/bin/env bash
# Install Kubernetes tooling (helm, kind) needed by docs/design/17-deployment-k8s.md.
# Run with sudo: `sudo bash scripts/install-tooling.sh`
#
# Network downloads run as the invoking user (so DNS works in WSL where sudo's
# resolver sometimes can't reach the internet), and only file installs run as
# root.
#
# Idempotent — skips anything already installed.

set -euo pipefail

KIND_VERSION="v0.25.0"
HELM_VERSION="v3.16.2"

log() { printf '[install-tooling] %s\n' "$*"; }

need_root() {
  if [[ $EUID -ne 0 ]]; then
    echo "This script needs root. Run with: sudo bash scripts/install-tooling.sh" >&2
    exit 1
  fi
}

# Run a command as the user who invoked sudo, with their PATH and HOME, so
# network operations work in WSL. Falls back to root if SUDO_USER isn't set.
as_user() {
  if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
    sudo -u "$SUDO_USER" -H bash -c "$*"
  else
    bash -c "$*"
  fi
}

DL_DIR="${TMPDIR:-/tmp}/omp-tooling-$$"

cleanup() { rm -rf "$DL_DIR" 2>/dev/null || true; }
trap cleanup EXIT

install_helm_binary() {
  if command -v helm >/dev/null 2>&1; then
    log "helm already installed: $(helm version --short 2>/dev/null || echo '?')"
    return 0
  fi
  log "downloading helm ${HELM_VERSION}"
  as_user "mkdir -p '$DL_DIR' && curl -fsSL -o '$DL_DIR/helm.tar.gz' 'https://get.helm.sh/helm-${HELM_VERSION}-linux-amd64.tar.gz'"
  log "extracting helm"
  tar -xzf "$DL_DIR/helm.tar.gz" -C "$DL_DIR"
  install -m 0755 "$DL_DIR/linux-amd64/helm" /usr/local/bin/helm
  log "helm installed: $(helm version --short)"
}

install_kind_binary() {
  if command -v kind >/dev/null 2>&1; then
    log "kind already installed: $(kind version 2>/dev/null || echo '?')"
    return 0
  fi
  log "downloading kind ${KIND_VERSION}"
  as_user "mkdir -p '$DL_DIR' && curl -fsSL -o '$DL_DIR/kind' 'https://kind.sigs.k8s.io/dl/${KIND_VERSION}/kind-linux-amd64'"
  install -m 0755 "$DL_DIR/kind" /usr/local/bin/kind
  log "kind installed: $(kind version)"
}

check_protoc() {
  if command -v protoc >/dev/null 2>&1; then
    log "protoc present: $(protoc --version)"
  else
    log "WARNING: protoc not found — install with 'apt-get install -y protobuf-compiler' or your package manager"
  fi
}

check_docker() {
  if command -v docker >/dev/null 2>&1; then
    if as_user "docker info" >/dev/null 2>&1; then
      log "docker reachable"
    else
      log "WARNING: docker binary present but daemon unreachable. In WSL, enable Docker Desktop -> Settings -> Resources -> WSL integration."
    fi
  else
    log "WARNING: docker not found — install Docker Desktop and enable WSL integration"
  fi
}

check_kubectl() {
  if command -v kubectl >/dev/null 2>&1; then
    log "kubectl present: $(kubectl version --client 2>&1 | head -1)"
  else
    log "WARNING: kubectl not found — install with 'apt-get install -y kubectl' or via the upstream apt repo"
  fi
}

main() {
  need_root
  install_helm_binary
  install_kind_binary
  check_protoc
  check_kubectl
  check_docker
  log "done"
}

main "$@"
