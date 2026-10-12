# ADR 0021: Passive outlier ejection alongside active health checks

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

Active health checks (ADR 0012) notice a dead endpoint within an interval or
more, and miss one that passes `/healthz` while failing real requests. v1
requires passive outlier ejection: consecutive 5xx and gateway errors, with
ejection backoff and a maximum ejection percentage, as in Envoy.

## Decision

- Every upstream attempt is classified: a response below 500 is a success; 502,
  503 and 504, as well as connect failures, resets and attempt timeouts, are
  gateway failures; other 5xx are server errors. A success resets both streaks;
  a server error breaks the gateway streak. Gateway failures count towards both
  streaks, as in Envoy.
- `consecutive_5xx` (default 5) or `consecutive_gateway_failure` (default 5)
  ejects the endpoint for `base_ejection_time × n` (default 30 s; n counts its
  ejections), capped at `max_ejection_time` (default 300 s). n decays by one for
  every `base_ejection_time` the endpoint spends back in rotation.
- At most `max_ejection_percent` (default 10) of a cluster is ejected at once,
  but one endpoint always may be.
- An ejection rebuilds the host set immediately, inline on the request that
  triggered it, because it is rare. A maintenance loop ends expired ejections
  every 500 ms. Host-set rebuilds are serialised by a small mutex in the
  balancer, so a health-check rebuild and an ejection rebuild cannot race and
  leave a stale set; picks never take it.
- Ejection state lives in the reload-surviving endpoint state (ADR 0014).
  Outlier detection is off unless the cluster configures it, as in Envoy.

## Consequences

- An endpoint failing real traffic leaves rotation after a handful of requests,
  not after health-check intervals; combined with safe retries, clients rarely
  see its failures.
- Ejection and health are independent: an ejected endpoint keeps being health
  checked, and an unhealthy one is not ejected.
- The ejection cap protects capacity when many endpoints fail at once: a
  correlated failure of the whole cluster is left to health checks and panic
  mode rather than ejecting everything.
