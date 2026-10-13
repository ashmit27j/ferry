#!/usr/bin/env bash
# Runs inside the M4 runner container, attached to the kind network, with
# the repo at /ferry and results in /out. See bench/exit/run-m4.sh.
#
# PHASES selects steps: setup (MetalLB, image, helm install), smoke, chaos
# (M5: pod kills under load), conformance. Default: setup smoke conformance.
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

if has chaos; then
  # M5: pod kills under load. Three proxies behind the MetalLB address; a
  # constant-rate client from outside the cluster; graceful deletes of
  # proxy, controller (the leader) and backend pods, then, in a second
  # window, one forced proxy kill (no drain), reported separately.
  echo "== chaos: pod kills under load"
  kubectl apply -f /ferry/bench/exit/m4/smoke.yaml >/dev/null
  kubectl -n smoke rollout status deploy/echo --timeout=180s
  helm upgrade ferry /ferry/deploy/helm/ferry -n ferry-system --reuse-values \
    --set proxy.replicas=3 --set proxy.drainDelaySeconds=5 --wait --timeout 5m >/dev/null
  fleet=$(kubectl -n ferry-system get deploy -l app.kubernetes.io/name=ferry-proxy -o jsonpath='{.items[0].metadata.name}')
  for _ in $(seq 1 60); do
    [ "$(kubectl -n ferry-system get deploy "$fleet" -o jsonpath='{.status.readyReplicas}')" = 3 ] && break
    sleep 5
  done
  kubectl -n ferry-system get pods -l app.kubernetes.io/name=ferry-proxy -o wide
  addr=$(kubectl -n ferry-system get gateway ferry -o jsonpath='{.status.addresses[0].value}')
  mkdir -p /tmp/load && cp /ferry/bench/exit/m4/load.go /tmp/load/
  (cd /tmp/load && go mod init load >/dev/null 2>&1 && go build -o /tmp/loadgen .)
  for _ in $(seq 1 60); do
    curl -fsS --max-time 2 -o /dev/null -H 'Host: smoke.example' "http://$addr/smoke/hello" && break
    sleep 2
  done

  events=/out/m5-k8s-chaos-events.txt
  : > "$events"
  ev() { echo "$(date -u +%s) $*" | tee -a "$events"; }
  proxy_pod() { kubectl -n ferry-system get pods -l app.kubernetes.io/name=ferry-proxy \
    --field-selector=status.phase=Running -o jsonpath="{.items[$1].metadata.name}"; }

  /tmp/loadgen -url "http://$addr/smoke/hello" -host smoke.example -rate 500 -duration 210s \
    > /out/m5-k8s-chaos.json &
  load=$!
  sleep 20; p=$(proxy_pod 0); ev delete proxy "$p"; kubectl -n ferry-system delete pod "$p" --wait=false >/dev/null
  sleep 40
  leader=$(kubectl -n ferry-system get lease ferry-controller -o jsonpath='{.spec.holderIdentity}')
  ev delete controller-leader "$leader"; kubectl -n ferry-system delete pod "$leader" --wait=false >/dev/null
  sleep 30
  e=$(kubectl -n smoke get pods -l app=echo -o jsonpath='{.items[0].metadata.name}')
  ev delete backend "$e"; kubectl -n smoke delete pod "$e" --wait=false >/dev/null
  sleep 30; p=$(proxy_pod 1); ev delete proxy "$p"; kubectl -n ferry-system delete pod "$p" --wait=false >/dev/null
  sleep 30; p=$(proxy_pod 2); ev delete proxy "$p"; kubectl -n ferry-system delete pod "$p" --wait=false >/dev/null
  set +e; wait "$load"; graceful=$?; set -e
  echo "   graceful kills: $(cat /out/m5-k8s-chaos.json)"

  kubectl -n ferry-system rollout status deploy/"$fleet" --timeout=300s >/dev/null
  sleep 15
  /tmp/loadgen -url "http://$addr/smoke/hello" -host smoke.example -rate 500 -duration 60s \
    > /out/m5-k8s-chaos-forced.json &
  load=$!
  sleep 20; p=$(proxy_pod 0); ev force-kill proxy "$p"
  kubectl -n ferry-system delete pod "$p" --grace-period=0 --force >/dev/null 2>&1
  set +e; wait "$load"; set -e
  echo "   forced kill (informational): $(cat /out/m5-k8s-chaos-forced.json)"
  kubectl -n ferry-system rollout status deploy/"$fleet" --timeout=300s >/dev/null
  [ "$graceful" = 0 ] || { echo "requests failed during graceful pod kills"; exit 1; }
  echo "   chaos OK"
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
