#!/usr/bin/env bash
# Ferry, free version: check a live deployment does what
# docs/free-version.md says (ADR 0038). Needs curl; the WebSocket check also
# needs cargo (ws-flood). The first request may take a minute: the free tier
# wakes the container on demand.
#
#   deploy/render/smoke.sh [https://ferry-free.onrender.com]
set -euo pipefail

URL=${1:-https://ferry-free.onrender.com}
fail=0
check() { # name, ok
    if [ "$2" = 1 ]; then echo "ok    $1"; else echo "FAIL  $1"; fail=1; fi
}
backend() { curl -s -m 20 -o /dev/null -D - "$@" | tr -d '\r' | sed -n 's/^x-mock-backend: //Ip'; }
status() { curl -s -m 20 -o /dev/null -w '%{http_code}' "$@"; }

curl -s -m 120 -o /dev/null "$URL/healthz" || true # wake it up

check "/healthz is 200" "$([ "$(status "$URL/healthz")" = 200 ] && echo 1)"
check "/ serves the landing page" \
    "$(curl -s -m 20 "$URL/" | grep -q '<title>Ferry, free version</title>' && echo 1)"
check "x-ferry-edition: free" \
    "$(curl -s -m 20 -o /dev/null -D - "$URL/hello" | grep -qi '^x-ferry-edition: free' && echo 1)"

rr=$(for _ in 1 2 3 4; do backend "$URL/hello"; done | tr -d '\n')
check "/hello alternates a and b ($rr)" "$([[ $rr == abab || $rr == baba ]] && echo 1)"

b=$(for _ in $(seq 40); do backend "$URL/canary"; done | grep -c '^b$' || true)
check "/canary sends ~10% to b ($b of 40)" "$([ "$b" -ge 1 ] && [ "$b" -le 10 ] && echo 1)"
check "x-canary: always picks b" "$([ "$(backend -H 'x-canary: always' "$URL/canary")" = b ] && echo 1)"

# Three attempts against a 50% failure backend: 12.5% 503s expected.
bad=$(for _ in $(seq 80); do status "$URL/retry"; echo; done | grep -c '^503$' || true)
check "/retry retries 503s ($bad of 80 failed)" "$([ "$bad" -le 24 ] && echo 1)"

check "unknown path is 404" "$([ "$(status "$URL/nope")" = 404 ] && echo 1)"
check "/stats serves Ferry's metrics" "$(curl -s -m 20 "$URL/stats" | grep -q '^ferry_requests_total{' && echo 1)"
for p in /metrics /config /clusters; do
    check "admin $p is not public" "$([ "$(status "$URL$p")" = 404 ] && echo 1)"
done

if command -v cargo > /dev/null; then
    ws=${URL/#http/ws}/ws
    check "WebSocket echo on $ws" "$(cargo run -q -p ferry-testkit --bin ws-flood -- \
        --url "$ws" --count 5 --hold 5s > /dev/null 2>&1 && echo 1)"
fi

exit $fail
