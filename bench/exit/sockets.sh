#!/usr/bin/env bash
# Counts TCP sockets by role and state from /proc/net/tcp (no iproute2 needed).
# Upstream = remote port 9001-9003; downstream = local port 8080.
awk 'NR > 1 {
  split($2, l, ":"); split($3, r, ":"); st = $4
  if (r[2] == "2329" || r[2] == "232A" || r[2] == "232B") up[st]++
  if (l[2] == "1F90") down[st]++
} END {
  printf "upstream ESTAB %d TIME_WAIT %d | downstream ESTAB %d TIME_WAIT %d\n", up["01"], up["06"], down["01"], down["06"]
}' /proc/net/tcp
