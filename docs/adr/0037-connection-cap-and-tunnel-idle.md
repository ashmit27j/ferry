# ADR 0037: Listener connection cap and tunnel idle timeout

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M5

## Context

M5 asks for 10,000 idle WebSockets, and the threat model (B1) lists two
gaps that make idle connections a denial-of-service lever:

- Nothing caps how many downstream connections a listener holds. Each open
  connection costs a file descriptor, a task and its buffers, so a client
  that opens connections and never sends anything can exhaust descriptors or
  memory long before the header read timeout recycles them. HTTP/2
  connections that answer pings never time out at all.
- An upgraded connection (WebSocket, or any HTTP/1.1 `Upgrade`) becomes a
  byte tunnel with no timeout (a known M2 gap, ADR 0025). An abandoned tunnel
  holds a downstream socket, an upstream socket, an endpoint's in-flight slot
  and a circuit-breaker request slot forever.

Envoy bounds the first with per-listener connection limits and the overload
manager (`envoy.resource_monitors.downstream_connections`), and the second
with the stream idle timeout (5 minutes by default), which also applies to
upgraded streams.

## Decision

- **`limits.max_connections`** (listener, optional, default 100,000). A
  connection accepted while the listener already holds that many is closed
  immediately, before PROXY protocol or TLS, and counted in
  `ferry_downstream_connection_overflows_total{listener}`. The check reads
  the existing active-connections gauge, so it adds one atomic load per
  accept. 100,000 is far above what one replica should hold (the HPA scales
  out first) and below the 1,048,576-descriptor limit of a default container.
- **`timeouts.tunnel_idle`** (listener, optional, default `1h`). A tunnel with
  no bytes in either direction for that long is closed in both directions.
  The default is longer than Envoy's 5 minutes because WebSocket
  applications commonly ping every 30–60 s and some (dashboards, chat
  presence) go quiet for long stretches. The 1-hour bound still releases
  abandoned tunnels and their breaker slots. Activity is a timestamp the copy
  loops update; one timer per tunnel checks it. There is no timer reset per
  read.
- Both are new optional ferry-config fields that skip serialising when unset
  (ADR 0013, ADR 0033). FerryPolicy's `listener` section exposes them as
  `maxConnections` and `tunnelIdleTimeout`.

## Consequences

- A flood of idle connections now hits a ceiling and shows up in a metric,
  instead of exhausting descriptors without a trace.
- WebSocket applications that are silent for over an hour must ping, or
  raise `tunnelIdleTimeout` in a FerryPolicy.
- The cap counts connections per listener and per replica. A cluster-wide
  cap is the replica count times the limit, the same model as rate limits
  (ADR 0022).
- An upgraded connection stays in the open-connection count (and under
  the cap) for the life of its tunnel. hyper releases it from the HTTP
  server when it upgrades, so the tunnel holds its own guard. The M5 dry
  run caught this: without it, open tunnels did not count.
- A tunnel also holds its cluster's circuit-breaker request slot (ADR
  0025). A cluster that carries many long-lived WebSockets needs
  `max_requests` and `max_connections` above its peak tunnel count. The
  defaults (1024, as in Envoy) cut the M5 dry run off at 1,024 sockets.
- Connections refused by the cap get a TCP close with no HTTP response. That
  matches Envoy and avoids spending work on a request we will not serve.
