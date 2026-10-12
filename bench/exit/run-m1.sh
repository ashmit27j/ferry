#!/usr/bin/env bash
# M1 exit test: 10k req/s across 3 backends with zero errors while (a) one
# backend is killed and (b) the config reloads every second.
#
# Everything runs in one Linux container network namespace, so the numbers
# do not include Docker Desktop's port-forwarding path:
#   - "env" container: 3 mock backends (cores 4-9), Ferry (core 0), reloader;
#   - k6 container sharing env's network, on cores 10-19.
#
# Usage (from the repo root, Docker running, Linux binaries built into the
# `ferry-target` volume; see bench/README.md):
#   bench/exit/run-m1.sh            # writes bench/exit/results/*.json
set -euo pipefail
export MSYS_NO_PATHCONV=1
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT="$ROOT/bench/exit/results"
mkdir -p "$OUT"
IMAGE=rust:1.99.0-bookworm
K6=grafana/k6:1.3.0
RATE=${RATE:-10000}

start_env() { # $1 = RELOAD flag
  docker rm -f ferry-m1 >/dev/null 2>&1 || true
  docker run -d --name ferry-m1 --cpuset-cpus 0-9 \
    -e RELOAD="$1" -v ferry-target:/target:ro -v "$ROOT/bench/exit:/exit:ro" \
    $IMAGE bash /exit/m1-env.sh >/dev/null
  for _ in $(seq 1 50); do
    docker exec ferry-m1 bash -c 'exec 3<>/dev/tcp/127.0.0.1/8080' 2>/dev/null && return 0
    sleep 0.2
  done
  echo "ferry did not start"; docker exec ferry-m1 cat /run/ferry/ferry.log; exit 1
}

k6() { # $1 = name, $2 = target, $3 = duration
  docker run --rm --network container:ferry-m1 --cpuset-cpus 10-19 \
    -v "$ROOT/bench/k6:/scripts:ro" -v "$OUT:/out" \
    -e TARGET="$2" -e RATE="$RATE" -e DURATION="$3" \
    $K6 run --quiet --summary-export "/out/$1.json" /scripts/proxy.js 2>&1 \
    | grep -E "http_req_duration|http_req_failed|http_reqs|dropped_iterations|checks" || true
}

echo "== warm-up"
start_env 0
k6 warmup http://127.0.0.1:8080 10s >/dev/null

echo "== baseline: k6 -> backend a directly"
k6 direct http://127.0.0.1:9001 30s

echo "== steady: k6 -> ferry (1 core) -> 3 backends, no reloads"
k6 steady http://127.0.0.1:8080 30s

echo "== chaos: reload every second; kill backend c at t=20s"
start_env 1
k6 warmup2 http://127.0.0.1:8080 5s >/dev/null
( sleep 20; docker exec ferry-m1 bash -c 'kill -9 $(cat /run/ferry/mock-c.pid)'; echo "   killed backend c at t=20s" ) &
k6 chaos http://127.0.0.1:8080 60s
wait
echo "   reloads applied: $(docker exec ferry-m1 grep -c 'config reloaded' /run/ferry/ferry.log)"
echo "   health events:"; docker exec ferry-m1 grep -E 'endpoint (un)?healthy|host set rebuilt' /run/ferry/ferry.log | sed 's/^/     /' | head -5
echo "   ferry RSS: $(docker exec ferry-m1 bash -c 'grep VmRSS /proc/$(cat /run/ferry/ferry.pid)/status')"
docker rm -f ferry-m1 >/dev/null
rm -f "$OUT/warmup.json" "$OUT/warmup2.json"
