#!/usr/bin/env bash
# M5 exit run: a soak at steady load with chaos, while 10,000 WebSockets
# stay open, then checks that the SLOs held and memory stayed flat.
#
# On one Docker network (172.30.5.0/24, static IPs, because Ferry endpoints
# are IP addresses):
#   m5-ferry   .10  Ferry, admin on 9901, config reloaded every RELOAD_EVERY s
#   m5-be-a/b/c .11-.13  HTTP mock backends (cluster app, P2C-EWMA)
#   m5-ws      .14  WebSocket echo backend
#   m5-prom    .20  Prometheus, scraping Ferry every 5 s
#   ws-flood   .31  holds WS_COUNT WebSockets through Ferry for the whole soak
#   k6         .30  RATE req/s, run in SEGMENT-long pieces (k6 keeps every
#                   sample in memory, so one 4-hour run would exhaust it)
# Chaos, repeating every 20 minutes (bench/exit/m5/chaos.sh): kill backend c
# and restart it a minute later; 200 ms +-50 ms latency on b for 3 minutes;
# 5% packet loss on a for 3 minutes (tc netem from a netshoot sidecar).
#
# Usage (repo root, Docker running):
#   DURATION=4h bench/exit/run-m5.sh           # the M5 gate run
#   DURATION=10m RATE=2000 bench/exit/run-m5.sh   # a dry run
# Results: bench/exit/results/m5-* (k6 segments, ws-flood report, chaos
# timeline, check report). The stack is left up unless KEEP=0.
set -euo pipefail
export MSYS_NO_PATHCONV=1
ROOT=$(cd "$(dirname "$0")/../.." && (pwd -W 2>/dev/null || pwd))
HERE="$ROOT/bench/exit/m5"
OUT="$ROOT/bench/exit/results"
mkdir -p "$OUT"
NET=ferry-m5
RUST=rust:1.99.0-bookworm
K6=grafana/k6:1.3.0
PROM=prom/prometheus:v3.5.0
DURATION=${DURATION:-4h}
RATE=${RATE:-5000}
SEGMENT=${SEGMENT:-600}
WS_COUNT=${WS_COUNT:-10000}
RELOAD_EVERY=${RELOAD_EVERY:-60}
FERRY_CPUS=${FERRY_CPUS:-2}

seconds() {
  case $1 in
    *h) echo $(( ${1%h} * 3600 )) ;;
    *m) echo $(( ${1%m} * 60 )) ;;
    *s) echo "${1%s}" ;;
    *) echo "$1" ;;
  esac
}
TOTAL=$(seconds "$DURATION")

cleanup() {
  docker rm -f m5-ferry m5-be-a m5-be-b m5-be-c m5-ws m5-prom m5-flood m5-k6 >/dev/null 2>&1 || true
}
cleanup
docker network rm $NET >/dev/null 2>&1 || true
docker network create --subnet 172.30.5.0/24 $NET >/dev/null

if [ "${BUILD:-1}" = 1 ]; then
  echo "== build Linux release binaries into the ferry-target volume"
  docker run --rm -v "$ROOT:/src" -v ferry-target:/target -v ferry-cargo:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target -w /src $RUST \
    cargo build --release --locked -p ferry -p ferry-testkit --bins 2>&1 | tail -1
fi

run() {  # name ip [docker args...] -- command...
  local name=$1 ip=$2; shift 2
  local args=()
  while [ "$1" != "--" ]; do args+=("$1"); shift; done; shift
  docker run -d --name "$name" --network $NET --ip "$ip" --ulimit nofile=262144:262144 \
    -v ferry-target:/target:ro "${args[@]}" $RUST "$@" >/dev/null
}

echo "== backends"
for b in a b c; do
  ip=172.30.5.1$(( $(printf '%d' "'$b") - 96 ))
  run m5-be-$b "$ip" --restart no -- /target/release/mock-backend --name "$b" --http 0.0.0.0:9000
done
run m5-ws 172.30.5.14 -- /target/release/mock-backend --name ws --ws 0.0.0.0:9000

echo "== ferry ($FERRY_CPUS CPUs)"
run m5-ferry 172.30.5.10 --cpus "$FERRY_CPUS" -p 9901:9901 -e RELOAD_EVERY="$RELOAD_EVERY" \
  -v "$HERE:/m5:ro" -- bash /m5/ferry-env.sh
for _ in $(seq 1 100); do
  docker exec m5-ferry bash -c 'exec 3<>/dev/tcp/127.0.0.1/8080' 2>/dev/null && break
  sleep 0.2
done

echo "== prometheus"
docker run -d --name m5-prom --network $NET --ip 172.30.5.20 -p 9090:9090 \
  -v "$HERE/prometheus.yaml:/etc/prometheus/prometheus.yml:ro" $PROM \
  --config.file=/etc/prometheus/prometheus.yml --storage.tsdb.retention.time=7d >/dev/null
START=$(date -u +%s)
echo "$START" > "$OUT/m5-start.txt"

echo "== $WS_COUNT WebSockets, held for the soak"
docker run -d --name m5-flood --network $NET --ip 172.30.5.31 --ulimit nofile=262144:262144 \
  -v ferry-target:/target:ro $RUST /target/release/ws-flood --url ws://172.30.5.10:8080/ws \
  --count "$WS_COUNT" --concurrency 200 --hold "$((TOTAL + 60))s" --interval 30s >/dev/null
sleep 60

echo "== chaos every 20 minutes (timeline: results/m5-chaos.txt)"
bash "$HERE/chaos.sh" "$TOTAL" > "$OUT/m5-chaos.txt" 2>&1 &
CHAOS=$!

echo "== soak: $RATE req/s for $DURATION in ${SEGMENT}s segments"
rm -f "$OUT"/m5-k6-*.json
elapsed=0; seg=0
while [ "$elapsed" -lt "$TOTAL" ]; do
  len=$(( TOTAL - elapsed < SEGMENT ? TOTAL - elapsed : SEGMENT ))
  seg=$((seg + 1))
  docker run --rm --name m5-k6 --network $NET --ip 172.30.5.30 \
    -v "$ROOT/bench/k6:/scripts:ro" -v "$OUT:/out" \
    -e TARGET=http://172.30.5.10:8080 -e RATE="$RATE" -e DURATION="${len}s" \
    $K6 run --quiet --no-thresholds --summary-export "/out/m5-k6-$(printf %03d $seg).json" /scripts/proxy.js \
    >/dev/null 2>&1 || true
  elapsed=$(( elapsed + len ))
  printf '   segment %d done (%d/%d s), ferry RSS %s\n' "$seg" "$elapsed" "$TOTAL" \
    "$(docker exec m5-ferry bash -c 'grep VmRSS /proc/$(cat /run/ferry/ferry.pid)/status | tr -s " " | cut -d" " -f2-3')"
done
wait $CHAOS || true
END=$(date -u +%s)

echo "== ws-flood report"
docker wait m5-flood >/dev/null
docker logs m5-flood 2>/dev/null | tail -1 | tee "$OUT/m5-ws-flood.json"

echo "== checks"
python -I "$HERE/m5-check.py" --prometheus http://localhost:9090 --start "$START" --end "$END" \
  --k6 "$OUT" --ws "$OUT/m5-ws-flood.json" --rate "$RATE" --out "$OUT/m5-check.json"
status=$?
if [ "${KEEP:-1}" = "0" ]; then cleanup; docker network rm $NET >/dev/null; fi
exit $status
