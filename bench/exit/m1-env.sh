#!/usr/bin/env bash
# Runs inside the bench container: three mock backends, Ferry pinned to one
# core, and a reloader that rewrites the config every second.
set -euo pipefail
BIN=/target/release
mkdir -p /run/ferry
cp /exit/m1.yaml /run/ferry/template.yaml
sed "s/RELOAD_MARK/0/" /run/ferry/template.yaml > /run/ferry/ferry.yaml

names=(a b c)
for i in 1 2 3; do
  name=${names[$((i - 1))]}
  taskset -c 4-9 "$BIN/mock-backend" --name "$name" --http "127.0.0.1:900$i" \
    > "/run/ferry/mock-$name.log" 2>&1 &
  echo $! > "/run/ferry/mock-$name.pid"
done

# Ferry gets exactly one core: tokio sizes its worker pool from the affinity mask.
FERRY_LOG=info taskset -c "${FERRY_CPUS:-0}" "$BIN/ferry" proxy \
  --config /run/ferry/ferry.yaml --watch-interval-ms 200 \
  > /run/ferry/ferry.log 2>&1 &
echo $! > /run/ferry/ferry.pid

if [ "${RELOAD:-1}" = "1" ]; then
  (
    n=0
    while true; do
      sleep 1
      n=$((n + 1))
      sed "s/RELOAD_MARK/$n/" /run/ferry/template.yaml > /run/ferry/ferry.yaml.tmp
      mv /run/ferry/ferry.yaml.tmp /run/ferry/ferry.yaml
    done
  ) &
fi

wait
