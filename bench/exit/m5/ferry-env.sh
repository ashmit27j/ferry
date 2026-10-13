#!/usr/bin/env bash
# Runs inside m5-ferry: Ferry with its admin API, and a reloader that makes
# a real config change every RELOAD_EVERY seconds.
set -euo pipefail
mkdir -p /run/ferry
cp /m5/m5.yaml /run/ferry/template.yaml
sed "s/RELOAD_MARK/0/" /run/ferry/template.yaml > /run/ferry/ferry.yaml

FERRY_LOG=info /target/release/ferry proxy \
  --config /run/ferry/ferry.yaml --watch-interval-ms 500 \
  --admin-address 0.0.0.0:9901 \
  > /run/ferry/ferry.log 2>&1 &
echo $! > /run/ferry/ferry.pid

(
  n=0
  while true; do
    sleep "${RELOAD_EVERY:-60}"
    n=$((n + 1))
    sed "s/RELOAD_MARK/$n/" /run/ferry/template.yaml > /run/ferry/ferry.yaml.tmp
    mv /run/ferry/ferry.yaml.tmp /run/ferry/ferry.yaml
  done
) &

wait
