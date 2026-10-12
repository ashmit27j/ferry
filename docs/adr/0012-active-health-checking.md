# ADR 0012: Active health checks with consecutive thresholds, starting healthy

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M1

## Context

v1 requires active HTTP and gRPC health checks. Envoy's model is per-endpoint
probes on an interval with consecutive unhealthy and healthy thresholds. Envoy
holds new hosts out of rotation until their first probe passes (cluster warming).

## Decision

- One task per `(cluster, endpoint)` probes every `interval` (default 5 s, at
  least 100 ms) with ±10% jitter; the first probe runs within 10% of an interval.
  Each probe opens a fresh connection and must finish within `timeout`
  (default 1 s).
- `unhealthy_threshold` consecutive failures (default 3) mark the endpoint
  unhealthy; `healthy_threshold` consecutive successes (default 2) mark it
  healthy. Every change rebuilds that cluster's host set.
- **HTTP:** `GET path` over HTTP/1.1 with `Host` (default: the cluster name) and
  `Connection: close`; healthy if the status is in `expected_statuses`
  (default `[200]`).
- **gRPC:** `grpc.health.v1.Health/Check` over h2c, encoded by hand (the
  messages have one field each), so the data plane needs no protobuf runtime.
  Healthy only for `grpc-status: 0` with `SERVING`.
- **Endpoints start healthy.** This departs from Envoy's warming on purpose.
  Ferry's safe retries (ADR 0011) absorb the window before a dead endpoint is
  detected, while starting unhealthy would make every reload that adds an
  endpoint briefly shrink capacity.
- Checkers survive reloads when the endpoint and settings are unchanged
  (ADR 0014). Without a health check, endpoints are always healthy.

## Consequences

- Detection takes about `interval × unhealthy_threshold`. With the exit test's
  1 s × 2 settings the killed backend was removed within ~2 s.
- One TCP connection per probe: negligible at 5 s intervals, but very short
  intervals across thousands of endpoints should reuse connections (M5).
- Passive outlier ejection (M2) complements this by reacting to live traffic
  within one request rather than one interval.
