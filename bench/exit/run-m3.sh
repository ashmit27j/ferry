#!/usr/bin/env bash
# M3 exit run: every SLO can be read from the dashboard, and one trace spans
# client -> Ferry -> backend.
#
# Starts, on one Docker network: Tempo, an OpenTelemetry Collector,
# Prometheus (scraping Ferry's admin API every 5 s), Grafana (provisioned
# with deploy/grafana/ferry-dashboard.json), and an env container running
# Ferry and three traced mock backends. k6 drives load; part-way through, a
# backend is killed and the config keeps reloading. Then trace-client sends
# traced requests, and bench/exit/m3-check.py evaluates every SLO panel's
# query against Prometheus and fetches the traces from Tempo.
#
# Usage (repo root, Docker running, Linux binaries in the `ferry-target`
# volume; see bench/README.md):
#   bench/exit/run-m3.sh            # leaves the stack up; KEEP=0 tears it down
# Grafana: http://localhost:3000 (anonymous admin), dashboard "Ferry".
set -euo pipefail
export MSYS_NO_PATHCONV=1
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OBS="$ROOT/deploy/observability"
OUT="$ROOT/bench/exit/results"
mkdir -p "$OUT"
NET=ferry-m3
RUST=rust:1.99.0-bookworm
K6=grafana/k6:1.3.0
TEMPO=grafana/tempo:2.8.1
OTEL=otel/opentelemetry-collector:0.130.0
PROM=prom/prometheus:v3.5.0
GRAFANA=grafana/grafana:12.1.0
RATE=${RATE:-2000}
DURATION=${DURATION:-180s}

cleanup() { docker rm -f ferry-m3 m3-tempo m3-otel m3-prom m3-grafana >/dev/null 2>&1 || true; }
cleanup
docker network create $NET >/dev/null 2>&1 || true

echo "== observability stack"
docker run -d --name m3-tempo --network $NET -p 3200:3200 \
  -v "$OBS/tempo.yaml:/etc/tempo.yaml:ro" $TEMPO -config.file=/etc/tempo.yaml >/dev/null
docker run -d --name m3-otel --network $NET \
  -v "$OBS/otel-collector.yaml:/etc/otelcol/config.yaml:ro" $OTEL --config /etc/otelcol/config.yaml >/dev/null
docker run -d --name m3-prom --network $NET -p 9090:9090 \
  -v "$OBS/prometheus.yaml:/etc/prometheus/prometheus.yml:ro" $PROM >/dev/null
docker run -d --name m3-grafana --network $NET -p 3000:3000 \
  -e GF_AUTH_ANONYMOUS_ENABLED=true -e GF_AUTH_ANONYMOUS_ORG_ROLE=Admin \
  -e GF_AUTH_DISABLE_LOGIN_FORM=true \
  -e GF_DASHBOARDS_DEFAULT_HOME_DASHBOARD_PATH=/var/lib/grafana/dashboards/ferry.json \
  -v "$OBS/grafana/provisioning:/etc/grafana/provisioning:ro" \
  -v "$ROOT/deploy/grafana/ferry-dashboard.json:/var/lib/grafana/dashboards/ferry.json:ro" \
  $GRAFANA >/dev/null

echo "== ferry + 3 traced backends"
docker run -d --name ferry-m3 --network $NET -p 9901:9901 \
  -e OTLP=http://m3-otel:4317 -e SAMPLE=0.01 -e RELOAD_EVERY=5 \
  -v ferry-target:/target:ro -v "$ROOT/bench/exit:/exit:ro" \
  $RUST bash /exit/m3-env.sh >/dev/null
for _ in $(seq 1 50); do
  docker exec ferry-m3 bash -c 'exec 3<>/dev/tcp/127.0.0.1/8080' 2>/dev/null && break
  sleep 0.2
done

echo "== load: $RATE req/s for $DURATION; kill backend c at t=60s"
( sleep 60; docker exec ferry-m3 bash -c 'kill -9 $(cat /run/ferry/mock-c.pid)'; echo "   killed backend c" ) &
( for i in $(seq 1 30); do docker exec ferry-m3 curl -s -o /dev/null http://127.0.0.1:8080/limited; sleep 0.05; done ) &
docker run --rm --network $NET -v "$ROOT/bench/k6:/scripts:ro" -v "$OUT:/out" \
  -e TARGET=http://ferry-m3:8080 -e RATE="$RATE" -e DURATION="$DURATION" \
  $K6 run --quiet --summary-export /out/m3-load.json /scripts/proxy.js 2>&1 \
  | grep -E "http_req_duration|http_req_failed|http_reqs|dropped_iterations" || true
wait

echo "== traced requests: client -> ferry -> backend"
docker exec ferry-m3 /target/release/trace-client --url http://127.0.0.1:8080/traced \
  --otlp-endpoint http://m3-otel:4317 --count 3 | tee "$OUT/m3-trace-ids.txt"
sleep 15   # collector batch + Tempo ingest

echo "== checks"
python "$ROOT/bench/exit/m3-check.py" "$ROOT/deploy/grafana/ferry-dashboard.json" \
  "$OUT/m3-trace-ids.txt" "$OUT/m3-check.json"
echo "   ferry RSS: $(docker exec ferry-m3 bash -c 'grep VmRSS /proc/$(cat /run/ferry/ferry.pid)/status')"
echo "   access log lines: $(docker exec ferry-m3 wc -l /run/ferry/access.log)"
if [ "${KEEP:-1}" = "0" ]; then cleanup; docker network rm $NET >/dev/null; fi
