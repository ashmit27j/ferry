# ADR 0022: Local rate limits in a fixed table of GCRA slots

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

v1 needs a local token bucket per route, keyed by client IP or a header.
Cluster-wide limits work by having the controller divide each limit across
proxy replicas (M4); the proxy only enforces its share. The plan's stack lists
`governor`. Its keyed limiter stores one bucket per distinct key in a sharded
map that grows until it is explicitly pruned. With IPv6, or a header the
client chooses, an attacker can create unbounded keys, so the limiter itself
becomes a memory-exhaustion target.

## Decision

- Each route with a `rate_limit` gets 4,096 slots, one `AtomicU64` each (32 KiB).
  A key (client IP, or a header's value, with "absent" being one key) is
  hashed with a per-process random seed to pick a slot.
- Each slot runs GCRA: it stores the theoretical arrival time of the next
  request; a request is allowed when that time is at most `(burst − 1) × per /
  requests` ahead of now, and pushes it forward by `per / requests`. One
  compare-and-swap per request; no locks, no allocation, no cleanup task.
- A refused request gets 429 (or `UNAVAILABLE` for gRPC) with `retry-after` in
  whole seconds and `x-ferry-ratelimited: true`, before redirects or any
  upstream work.
- Limits apply per route, to every rule of the route. Buckets survive reloads
  that leave the route's limit unchanged, and restart when it changes.
- `governor` is not used.

## Consequences

- Memory per route is fixed, whatever the number of clients.
- Two keys that hash to the same slot share a bucket, which can only make
  their limit stricter. With 4,096 slots, collisions matter only when many
  clients sit near the limit at once, and the random seed stops a client from
  choosing whom it collides with.
- Limits are per replica; the controller's division (M4) makes them
  approximately cluster-wide, exact only under even load balancing.
