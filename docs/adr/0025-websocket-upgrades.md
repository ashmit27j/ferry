# ADR 0025: Proxy HTTP/1.1 upgrades (WebSocket) as byte tunnels

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

v1 proxies WebSockets. A WebSocket starts as an HTTP/1.1 request with
`Connection: upgrade` and `Upgrade: websocket`; after a 101 the connection
carries WebSocket frames, which a proxy does not need to understand. Both
headers are hop-by-hop and are normally stripped. RFC 8441 (WebSockets over
HTTP/2 extended CONNECT) also exists, but the Gateway API conformance tests
and AWS NLB targets use HTTP/1.1 upgrades.

## Decision

- For an HTTP/1.1 request with `Connection: upgrade` routed to an HTTP/1.1
  cluster, Ferry forwards `Upgrade` and `Connection: upgrade` instead of
  stripping them. Ferry's server connections and upstream pool connections are
  both created with hyper's upgrade support.
- On a 101 from the backend, Ferry returns the 101 (with `Upgrade`,
  `Connection: upgrade` and `X-Request-Id`) and spawns a tunnel that, once both
  sides have switched protocols, copies bytes in both directions until either
  side closes. Any other response is proxied as usual.
- The tunnel holds the endpoint's in-flight guard and the cluster's
  `max_requests` slot until it closes, so least-request balancing and circuit
  breakers count open WebSockets. The upgraded upstream connection never
  returns to the pool.
- Route timeouts bound the request up to the 101. The tunnel itself has no idle
  timeout: WebSocket applications keep connections alive with ping/pong, and
  M5 soaks 10,000 idle WebSockets.
- Upgrades to an HTTP/2 cluster, and HTTP/2 extended CONNECT from clients, are
  not supported. Such requests are forwarded without upgrade headers, so the
  backend answers them as plain requests.

## Consequences

- WebSockets work through Ferry with the same routing, filters, balancing,
  retries before the 101, and circuit breakers as any request.
- Each open WebSocket is a long-lived `max_requests` slot; clusters serving
  many WebSockets need that limit raised from the default 1,024.
- RFC 8441 can be added later behind hyper's `enable_connect_protocol` if
  HTTP/2 clients need it.
