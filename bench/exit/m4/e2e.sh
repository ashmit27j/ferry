#!/usr/bin/env bash
# Runs inside the M4 runner container, attached to the kind network, with
# the repo at /ferry and results in /out. See bench/exit/run-m4.sh.
#
# PHASES selects steps: setup (MetalLB, image, helm install), smoke,
# conformance. Default: all three.
set -euo pipefail
CLUSTER=${CLUSTER:-ferry-m4}
PHASES=${PHASES:-setup smoke conformance}
# Extended features to claim on top of the core ones (EXTENDED=1).
EXTENDED_FEATURES=GatewayPort8080,HTTPRoute303RedirectStatusCode,HTTPRoute307RedirectStatusCode,HTTPRoute308RedirectStatusCode,HTTPRouteQueryParamMatching,HTTPRouteMethodMatching,HTTPRouteResponseHeaderModification,HTTPRoutePortRedirect,HTTPRouteSchemeRedirect,HTTPRoutePathRedirect,HTTPRouteHostRewrite,HTTPRoutePathRewrite,HTTPRouteRequestTimeout,HTTPRouteBackendTimeout,HTTPRouteParentRefPort,HTTPRouteBackendProtocolH2C,HTTPRouteBackendProtocolWebSocket
mkdir -p /root/.kube
kind get kubeconfig --internal --name "$CLUSTER" > /root/.kube/config

has() { [[ " $PHASES " == *" $1 "* ]]; }

retry() { # retry <attempts> <cmd...>
  local n=$1; shift
  for i in $(seq 1 "$n"); do "$@" && return 0; sleep 3; done
  return 1
}

if has setup; then
  echo "== load the ferry image into kind"
  kind load docker-image ferry:dev --name "$CLUSTER"

  echo "== MetalLB, for LoadBalancer addresses on the kind network"
  kubectl apply -f https://raw.githubusercontent.com/metallb/metallb/v0.16.1/config/manifests/metallb-native.yaml >/dev/null
  kubectl -n metallb-system rollout status deploy/controller --timeout=300s
  kubectl -n metallb-system rollout status ds/speaker --timeout=300s
  subnet=$(docker network inspect kind -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' | tr ' ' '\n' | grep -v ':' | head -1)
  prefix=$(echo "$subnet" | cut -d. -f1-2)
  pool=$(cat <<EOF
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata: { name: kind, namespace: metallb-system }
spec: { addresses: ["${prefix}.255.100-${prefix}.255.250"] }
---
apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata: { name: kind, namespace: metallb-system }
EOF
)
  # The webhook needs a moment after the controller reports ready.
  retry 40 kubectl apply -f - <<<"$pool" >/dev/null
  echo "   pool ${prefix}.255.100-${prefix}.255.250"

  echo "== helm install ferry"
  helm upgrade --install ferry /ferry/deploy/helm/ferry \
    --namespace ferry-system --create-namespace \
    -f /ferry/deploy/helm/ferry/values-kind.yaml \
    --wait --timeout 5m
  kubectl -n ferry-system get pods -o wide
fi

if has smoke; then
  echo "== smoke: the Helm-created Gateway serves an HTTPRoute"
  kubectl apply -f /ferry/bench/exit/m4/smoke.yaml >/dev/null
  kubectl -n smoke rollout status deploy/echo --timeout=180s
  kubectl -n ferry-system wait --for=condition=Programmed gateway/ferry --timeout=300s
  addr=$(kubectl -n ferry-system get gateway ferry -o jsonpath='{.status.addresses[0].value}')
  echo "   gateway address $addr"
  ok=""
  for _ in $(seq 1 60); do
    if body=$(curl -fsS --max-time 2 -H 'Host: smoke.example' "http://$addr/smoke/hello"); then ok=1; break; fi
    sleep 2
  done
  [ -n "$ok" ] || { echo "smoke request never succeeded"; exit 1; }
  echo "$body" | tee /out/m4-smoke.json
  echo "$body" | grep -q '"namespace": *"smoke"' || { echo "unexpected backend"; exit 1; }
  echo "$body" | grep -qi 'x-ferry-smoke' || { echo "header filter missing"; exit 1; }
  code=$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: other.example' "http://$addr/smoke")
  [ "$code" = 404 ] || { echo "expected 404 for an unknown host, got $code"; exit 1; }
  kubectl -n smoke get httproute smoke -o jsonpath='{range .status.parents[*]}{.parentRef.name}: {range .conditions[*]}{.type}={.status} {end}{"\n"}{end}' | tee -a /out/m4-smoke.txt
  kubectl -n smoke get ferrypolicy echo -o jsonpath='{range .status.conditions[*]}FerryPolicy {.type}={.status} ({.reason}){"\n"}{end}' | tee -a /out/m4-smoke.txt
  kubectl -n ferry-system get gateway ferry -o wide | tee -a /out/m4-smoke.txt
  echo "   smoke OK"
fi

if has conformance; then
  features=Gateway,HTTPRoute,GRPCRoute,ReferenceGrant
  profile=core
  if [ "${EXTENDED:-0}" = 1 ]; then
    features="$features,$EXTENDED_FEATURES"
    profile=extended
  fi
  report=/out/m4-conformance-$profile.yaml
  log=/out/m4-conformance-$profile.log
  summary=/out/m4-conformance-$profile-summary.txt
  echo "== Gateway API ${GATEWAY_API_VERSION:-v1.6.3} conformance: GATEWAY-HTTP, GATEWAY-GRPC ($features)"
  # A previous run's namespaces linger while they terminate, and the suite
  # cannot recreate its fixtures in them until they are gone.
  for ns in $(kubectl get ns -o name | grep '^namespace/gateway-conformance-' || true); do
    kubectl wait --for=delete "$ns" --timeout=5m
  done
  set +e
  conformance.test -test.v -test.timeout 90m -test.run '^TestConformance$' \
    -gateway-class ferry \
    -supported-features "$features" \
    -conformance-profiles GATEWAY-HTTP,GATEWAY-GRPC \
    -report-output "$report" \
    -organization ferry -project ferry -url https://github.com/ashmit27j/ferry \
    -version v0.1.0 -contact https://github.com/ashmit27j/ferry/issues \
    ${SKIP_TESTS:+-skip-tests "$SKIP_TESTS"} ${RUN_TEST:+-run-test "$RUN_TEST"} \
    > "$log" 2>&1
  rc=$?
  set -e
  grep -E -- '^ *--- (PASS|FAIL)' "$log" | sed 's/^ *//' > "$summary" || true
  echo "   $(grep -c -- '--- PASS' "$summary") passed, $(grep -c -- '--- FAIL' "$summary") failed (sub-tests included)"
  [ -f "$report" ] && sed -n '/profiles:/,$p' "$report" | head -60
  exit $rc
fi
