# ADR 0026: Two-stage drain for zero-drop rolling restarts

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

M2's exit criterion includes "a rolling restart under load drops zero
requests". In Kubernetes, a pod receives SIGTERM while it is being removed
from Service endpoints, and the NLB stops sending it new connections only
after its deregistration delay. A proxy that stops accepting immediately
refuses connections the NLB still sends. A proxy that closes idle keep-alive
connections immediately races clients that are just sending a request on them.

## Decision

`Proxy::run` drains in two stages (`Drain { delay, timeout }`):

1. **Delay** (`--drain-delay-secs`, default 0; the Helm chart sets it to cover
   NLB deregistration): keep accepting new connections. Every HTTP/1.1
   response carries `Connection: close`, so each client connection closes
   right after a completed exchange, never mid-request, and the client
   reconnects through the load balancer to another replica.
   `ReloadHandle::is_draining` reports the state; the admin `/ready` endpoint
   (M3) will return 503 from this point.
2. **Shutdown:** stop accepting; send HTTP/2 GOAWAY (the client learns
   exactly which streams were processed and retries the rest elsewhere); close
   idle HTTP/1.1 connections; wait up to `timeout` (`--drain-timeout-secs`,
   default 30 s) for in-flight requests.

Upgraded connections (WebSockets) are not closed by the delay stage; they end
at the timeout.

## Consequences

- `rolling_restart_under_load_drops_nothing` replaces both replicas behind a
  layer-4 balancer, with a 100 ms deregistration lag and 16 keep-alive clients,
  and sees no failure (stable over 15 runs).
- Shutdown takes at least `delay`. Locally the default of 0 keeps Ctrl-C
  instant; in Kubernetes `terminationGracePeriodSeconds` must exceed
  `delay + timeout`.
- HTTP/2 clients are only moved at stage 2. GOAWAY makes that safe, but
  long-lived h2 connections stay on the old replica during the delay.
