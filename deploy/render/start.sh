#!/bin/sh
# Ferry, free version: start the demo backends, then Ferry as PID 1 so it
# receives SIGTERM and drains (ADR 0038).
set -eu

mock-backend --name a --http 127.0.0.1:9001 --ws 127.0.0.1:9201 &
mock-backend --name b --http 127.0.0.1:9002 &
mock-backend --name flaky --http 127.0.0.1:9003 --error-rate 0.5 &
mock-backend --name site --http 127.0.0.1:9004 --page /etc/ferry/index.html &

exec ferry proxy --config /etc/ferry/ferry.yaml --admin-address 127.0.0.1:9901 "$@"
