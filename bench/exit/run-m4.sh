#!/usr/bin/env bash
# M4 exit run: `helm install` on a fresh kind cluster yields a working
# gateway, and the Gateway API core conformance suite passes for HTTPRoute
# and GRPCRoute.
#
# Everything Kubernetes-side runs in a runner container (kind, kubectl, helm,
# the conformance suite; bench/exit/m4/runner.Dockerfile) that talks to the
# host's Docker and joins the kind network, so it can reach the LoadBalancer
# addresses MetalLB hands out there.
#
# Usage (repo root, Docker running):
#   bench/exit/run-m4.sh                 # fresh cluster, all phases
#   PHASES="smoke" bench/exit/run-m4.sh  # reuse the cluster
#   EXTENDED=1 PHASES=conformance bench/exit/run-m4.sh   # also claim extended features
#   KEEP=0 bench/exit/run-m4.sh          # delete the cluster afterwards
#   FRESH=1 bench/exit/run-m4.sh         # delete any existing cluster first
# Results land in bench/exit/results/m4-*.
set -euo pipefail
export MSYS_NO_PATHCONV=1
# A Windows path under Git Bash (docker.exe does not understand /d/...).
ROOT=$(cd "$(dirname "$0")/../.." && (pwd -W 2>/dev/null || pwd))
OUT="$ROOT/bench/exit/results"
mkdir -p "$OUT"
CLUSTER=${CLUSTER:-ferry-m4}
PHASES=${PHASES:-setup smoke conformance}
KEEP=${KEEP:-1}
SOCK=/var/run/docker.sock

runner() { docker run --rm -v "$SOCK:$SOCK" "$@"; }

if [[ " $PHASES " == *" setup "* ]]; then
  echo "== images"
  docker build -q -t ferry:dev "$ROOT" >/dev/null
  docker build -q -t ferry-m4-runner -f "$ROOT/bench/exit/m4/runner.Dockerfile" "$ROOT/bench/exit/m4" >/dev/null
  echo "== kind cluster $CLUSTER"
  if [ "${FRESH:-0}" = 1 ]; then
    runner ferry-m4-runner kind delete cluster --name "$CLUSTER"
  fi
  if ! runner ferry-m4-runner kind get clusters | grep -qx "$CLUSTER"; then
    runner ferry-m4-runner kind create cluster --name "$CLUSTER" --wait 3m
  fi
fi

status=0
runner --network kind -v "$ROOT:/ferry:ro" -v "$OUT:/out" \
  -e CLUSTER="$CLUSTER" -e PHASES="$PHASES" -e EXTENDED="${EXTENDED:-0}" \
  -e SKIP_TESTS="${SKIP_TESTS:-}" -e RUN_TEST="${RUN_TEST:-}" \
  ferry-m4-runner bash /ferry/bench/exit/m4/e2e.sh || status=$?

if [ "$KEEP" = 0 ]; then
  runner ferry-m4-runner kind delete cluster --name "$CLUSTER"
fi
exit $status
