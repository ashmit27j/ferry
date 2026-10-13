#!/usr/bin/env bash
# Chaos for the M5 soak, repeating every 20 minutes until $1 seconds pass.
# Prints a timeline (UTC epoch seconds) that m5-check.py reads:
#   <epoch> <event> <target>
# tc netem runs from a netshoot sidecar sharing the backend's network
# namespace, so the backend image needs no tools and no extra privileges.
set -uo pipefail
export MSYS_NO_PATHCONV=1
TOTAL=$1
NETSHOOT=${NETSHOOT:-nicolaka/netshoot:v0.14}
start=$(date -u +%s)
log() { echo "$(date -u +%s) $*"; }
netem() {  # container, netem args... (empty clears)
  local c=$1; shift
  if [ $# -eq 0 ]; then
    docker run --rm --net "container:$c" --cap-add NET_ADMIN $NETSHOOT tc qdisc del dev eth0 root >/dev/null 2>&1
  else
    docker run --rm --net "container:$c" --cap-add NET_ADMIN $NETSHOOT tc qdisc replace dev eth0 root netem "$@" >/dev/null 2>&1
  fi
}
left() { echo $(( TOTAL - ($(date -u +%s) - start) )); }
pause() { local s=$1; [ "$(left)" -le "$s" ] && s=$(left); [ "$s" -gt 0 ] && sleep "$s"; }

docker pull -q $NETSHOOT >/dev/null 2>&1 || true
while [ "$(left)" -gt 0 ]; do
  log kill m5-be-c
  docker kill m5-be-c >/dev/null
  pause 60
  docker start m5-be-c >/dev/null && log restart m5-be-c
  pause 240
  [ "$(left)" -gt 0 ] || break

  log latency-on m5-be-b
  netem m5-be-b delay 200ms 50ms
  pause 180
  netem m5-be-b && log latency-off m5-be-b
  pause 120
  [ "$(left)" -gt 0 ] || break

  log loss-on m5-be-a
  netem m5-be-a loss 5%
  pause 180
  netem m5-be-a && log loss-off m5-be-a
  pause 420
done
# Leave everything healthy.
netem m5-be-a; netem m5-be-b
docker start m5-be-c >/dev/null 2>&1 || true
log end -
