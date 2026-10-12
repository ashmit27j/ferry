# ADR 0010: Five endpoint policies over health-aware host sets

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M1

## Context

v1 requires round robin, weighted round robin, least request, P2C-EWMA and
Maglev (by header, cookie or source IP). Picks are on the hot path and must not
lock or allocate. Health checks (ADR 0012) remove endpoints from rotation, and
retries (ADR 0011) need to avoid endpoints already tried.

## Decision

`ferry_lb::ClusterBalancer` owns a cluster's hosts and an `ArcSwap<HostSet>`. A
host set lists the eligible hosts plus any precomputed schedule. It is rebuilt
off the hot path by `refresh()` whenever health changes. A pick is one atomic
load plus the policy's work.

- **Panic mode (Envoy):** if fewer than `panic_threshold` percent (default 50) of
  hosts are healthy, every host is eligible. `0` disables panic mode, so a
  cluster with no healthy host answers 503 "no healthy upstream".
- **Round robin:** an atomic cursor over eligible hosts, kept across rebuilds.
- **Weighted round robin:** ADR 0007's interleaved selector over endpoint weights
  (1–128, Envoy's range): exact proportions in every window, no bursts.
- **Least request:** power of two choices on in-flight requests (Envoy's default
  least-request algorithm). The in-flight count is held by a guard that lives in
  the response body, so it covers the whole exchange.
- **P2C-EWMA:** power of two choices on `peak_ewma_latency × (in_flight + 1)`
  (Finagle and Linkerd). Samples above the estimate replace it; lower samples
  blend in with τ = 10 s; reads decay the estimate towards zero, so an unpicked
  host becomes attractive again. A connection-level failure records a 500 ms
  sample, because a refused connect is "fast" and would otherwise attract traffic.
- **Maglev:** Envoy's weighted Maglev with a 65,537-slot table, keyed by the host
  address text and hashed with XXH64 under fixed seeds, so every replica builds
  the same table. The request key is a header value, a cookie or the client IP.
  A missing header falls back to a random choice. A missing cookie makes Ferry
  generate one (16 hex digits), hash it for this request and return it in
  `Set-Cookie` (with `Max-Age` when `ttl` is set), so the client sticks from its
  first request on.
- **Retries:** `pick(hash, exclude)` avoids hosts already tried when any other
  host is eligible, falling back to round robin over the rest.

## Consequences

- Picks cost 17–72 ns (bench/RESULTS.md). A Maglev rebuild costs ~1 ms, paid on
  reload or health change, never per request.
- Rebuilding the Maglev table over the healthy hosts moves the lost host's keys
  plus only a few percent of the others (tested).
- Cookie stickiness reveals nothing about the host: the cookie is a random token.
- Source-IP stickiness sees the NLB's address until PROXY protocol lands in M2.
