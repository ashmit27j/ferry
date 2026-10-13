#!/usr/bin/env bash
# Runs inside the M3 env container: three traced mock backends, Ferry with its
# admin API, tracing and access log on, and a reloader.
set -euo pipefail
BIN=/target/release
mkdir -p /run/ferry
cp /exit/m3.yaml /run/ferry/template.yaml
sed "s/RELOAD_MARK/0/" /run/ferry/template.yaml > /run/ferry/ferry.yaml

names=(a b c)
for i in 1 2 3; do
  name=${names[$((i - 1))]}
  "$BIN/mock-backend" --name "$name" --http "127.0.0.1:900$i" \
    --otlp-endpoint "$OTLP" > "/run/ferry/mock-$name.log" 2>&1 &
  echo $! > "/run/ferry/mock-$name.pid"
done

FERRY_LOG=info "$BIN/ferry" proxy \
  --config /run/ferry/ferry.yaml --watch-interval-ms 200 \
  --admin-address 0.0.0.0:9901 \
  --otlp-endpoint "$OTLP" --trace-sample-ratio "${SAMPLE:-0.01}" \
  --access-log /run/ferry/access.log \
  > /run/ferry/ferry.log 2>&1 &
echo $! > /run/ferry/ferry.pid

# Reload every RELOAD_EVERY seconds (a real change each time).
(
  n=0
  while true; do
    sleep "${RELOAD_EVERY:-5}"
    n=$((n + 1))
    sed "s/RELOAD_MARK/$n/" /run/ferry/template.yaml > /run/ferry/ferry.yaml.tmp
    mv /run/ferry/ferry.yaml.tmp /run/ferry/ferry.yaml
  done
) &

wait
