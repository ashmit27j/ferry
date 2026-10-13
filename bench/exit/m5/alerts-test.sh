#!/usr/bin/env bash
# Checks and unit-tests Ferry's alerting rules with promtool (in Docker).
# Usage (repo root): bench/exit/m5/alerts-test.sh
set -euo pipefail
export MSYS_NO_PATHCONV=1
HERE=$(cd "$(dirname "$0")" && (pwd -W 2>/dev/null || pwd))
ROOT=$(cd "$(dirname "$0")/../../.." && (pwd -W 2>/dev/null || pwd))
PROMETHEUS_IMAGE=${PROMETHEUS_IMAGE:-prom/prometheus:v3.5.0}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$ROOT/deploy/helm/ferry/files/alerts.yaml" "$work/alerts.rendered.yaml"
cp "$HERE/alerts-test.yaml" "$work/"
WORK=$(cd "$work" && (pwd -W 2>/dev/null || pwd))

docker run --rm -v "$WORK:/rules" --entrypoint promtool "$PROMETHEUS_IMAGE" \
  check rules /rules/alerts.rendered.yaml
docker run --rm -v "$WORK:/rules" -w /rules --entrypoint promtool "$PROMETHEUS_IMAGE" \
  test rules alerts-test.yaml
