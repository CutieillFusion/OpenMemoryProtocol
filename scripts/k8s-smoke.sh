#!/usr/bin/env bash
# End-to-end smoke test for the Helm chart on a local kind cluster.
#
# 1. Builds a Docker image `omp:dev` with every OMP binary.
# 2. Creates (or reuses) a kind cluster `omp-test`.
# 3. Loads the image into the cluster.
# 4. helm install / upgrade the chart with values-dev.yaml.
# 5. Waits for pods to be Ready.
# 6. Port-forwards the gateway and runs a few `curl` checks.
# 7. Tears down on exit.

set -euo pipefail

CLUSTER_NAME=${CLUSTER_NAME:-omp-test}
RELEASE=${RELEASE:-omp}
NAMESPACE=${NAMESPACE:-omp}
IMAGE=${IMAGE:-omp:dev}

log() { printf '[k8s-smoke] %s\n' "$*"; }

cleanup() {
  if [[ "${KEEP_CLUSTER:-0}" == "0" ]]; then
    log "deleting kind cluster $CLUSTER_NAME"
    kind delete cluster --name "$CLUSTER_NAME" >/dev/null 2>&1 || true
  else
    log "leaving cluster $CLUSTER_NAME running (KEEP_CLUSTER=1)"
  fi
  if [[ -n "${PF_PID:-}" ]]; then
    kill "$PF_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

require() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing: $1"; exit 1; }
}
require docker
require kind
require helm
require kubectl
require curl

log "building image $IMAGE (this takes a few minutes the first time)"
docker build -t "$IMAGE" .

if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER_NAME"; then
  log "creating kind cluster $CLUSTER_NAME"
  kind create cluster --name "$CLUSTER_NAME" --wait 60s
fi

log "loading image into kind"
kind load docker-image "$IMAGE" --name "$CLUSTER_NAME"

log "creating namespace $NAMESPACE"
kubectl create namespace "$NAMESPACE" --dry-run=client -o yaml | kubectl apply -f -

log "installing chart"
helm upgrade --install "$RELEASE" charts/omp \
  --namespace "$NAMESPACE" \
  -f charts/omp/values-dev.yaml \
  --set "image.repository=omp" \
  --set "image.tag=dev" \
  --wait --timeout 8m \
  || {
    log "helm install failed; dumping pod state"
    kubectl -n "$NAMESPACE" get pods -o wide || true
    kubectl -n "$NAMESPACE" describe pods | tail -120 || true
    for pod in $(kubectl -n "$NAMESPACE" get pods -o name 2>/dev/null); do
      log "logs from $pod (last 80 lines):"
      kubectl -n "$NAMESPACE" logs --tail=80 --all-containers=true "$pod" || true
      echo "---"
    done
    exit 1
  }

log "pods:"
kubectl -n "$NAMESPACE" get pods -o wide

log "verifying pods reach Ready=True"
kubectl -n "$NAMESPACE" wait --for=condition=Ready pods --all --timeout=120s

log "port-forwarding gateway to localhost:18080"
kubectl -n "$NAMESPACE" port-forward "svc/${RELEASE}-omp-gateway" 18080:8080 \
  >/tmp/omp-pf.log 2>&1 &
PF_PID=$!
# Give port-forward a moment to bind.
for _ in $(seq 1 20); do
  if curl -fsS http://127.0.0.1:18080/healthz >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

log "curl gateway /healthz"
curl -fsS http://127.0.0.1:18080/healthz
echo

log "POST a file via gateway as alice (should land on a shard)"
TMP=$(mktemp)
echo "hello from k8s" >"$TMP"
curl -fsS -X POST http://127.0.0.1:18080/files \
  -H "Authorization: Bearer dev-alice" \
  -F "path=k8s.txt" \
  -F "file=@$TMP" >/dev/null
rm "$TMP"
curl -fsS -X POST http://127.0.0.1:18080/commit \
  -H "Authorization: Bearer dev-alice" \
  -H "Content-Type: application/json" \
  -d '{"message":"k8s test"}' \
  >/dev/null
echo

log "GET /files via gateway as alice"
curl -fsS http://127.0.0.1:18080/files \
  -H "Authorization: Bearer dev-alice" | head -c 400
echo

log "GET /ui/ via gateway (embedded SvelteKit UI)"
ui_body=$(curl -fsS http://127.0.0.1:18080/ui/)
echo "$ui_body" | head -c 200; echo
echo "$ui_body" | grep -q "<html" \
  || { log "ERROR: /ui/ did not return an HTML document"; exit 1; }
ui_ctype=$(curl -fsSI http://127.0.0.1:18080/ui/ | tr -d '\r' | awk -F': ' 'tolower($1)=="content-type"{print $2}')
echo "$ui_ctype" | grep -qi "text/html" \
  || { log "ERROR: /ui/ Content-Type was '$ui_ctype', expected text/html"; exit 1; }

log "GET /ui/file/foo (deep link → SPA fallback)"
curl -fsS http://127.0.0.1:18080/ui/file/foo | grep -q "<html" \
  || { log "ERROR: /ui/file/foo did not return SPA fallback HTML"; exit 1; }

log "GET / (should redirect to /ui/)"
loc=$(curl -fsS -o /dev/null -w '%{redirect_url}' http://127.0.0.1:18080/)
[[ "$loc" == *"/ui/" ]] || { log "ERROR: / did not redirect to /ui/, got '$loc'"; exit 1; }

log "GET /metrics via shard 0 (port-forwarded)"
kubectl -n "$NAMESPACE" port-forward "svc/${RELEASE}-omp-shard-0" 18001:8000 \
  >/tmp/omp-pf-shard.log 2>&1 &
PF2_PID=$!
sleep 2
curl -fsS http://127.0.0.1:18001/metrics | grep -E "^omp_request_total" | head -3 || true
kill $PF2_PID 2>/dev/null || true
echo

log "smoke test PASSED"
