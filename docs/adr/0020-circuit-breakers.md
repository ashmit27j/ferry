# ADR 0020: Circuit breakers fail fast instead of queueing

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

v1 requires circuit breakers on max connections, pending requests, requests and
retries per cluster, which is Envoy's model. Envoy queues requests waiting for
a connection (bounded by `max_pending_requests`). Every limit must be
enforced without locks on the hot path.

## Decision

- Each cluster has four atomic limits (`ferry_lb::Limit`, defaults 1024, 1024,
  1024 and 3, as in Envoy). Acquiring is one compare-and-swap; the returned
  `Permit` releases the slot when dropped.
- **`max_requests`:** one slot per upstream attempt, held by the response body,
  so it covers the whole exchange, streaming included.
- **`max_pending_requests`:** held while the request waits in `checkout` for a
  connection: usually an instant pop from the idle queue, longer while
  dialing.
- **`max_connections`:** taken when the pool dials a connection for this
  cluster, and held by the connection's task until it closes. A request that
  needs a new connection while the limit is full fails at once; Ferry has no
  connection wait queue.
- **`max_retries`:** a policy retry (ADR 0019) needs a slot, held until the
  request finishes. Safe retries (ADR 0011) do not.
- Any breaker that trips answers 503 `upstream overloaded` with
  `x-ferry-overloaded: true` (or `UNAVAILABLE` for gRPC). Breaker overflows are
  never retried, because retrying is exactly the load they shed.

## Consequences

- Overload shows up immediately, as distinct, countable 503s (M3 adds a metric
  per breaker), rather than as rising latency in a hidden queue.
- Without a wait queue, bursts beyond `max_connections` are rejected even if a
  connection would free up a millisecond later. The default of 1024 makes this
  irrelevant in practice; revisit if small connection limits are needed for
  fragile backends.
- Pools are shared across clusters by endpoint address, so a connection dialed
  for one cluster and reused by another counts only against the first.
