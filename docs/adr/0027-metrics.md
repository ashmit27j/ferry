# ADR 0027: Prometheus metrics behind pre-resolved, lock-free handles

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M3

## Context

The plan asks for Prometheus metrics: RED per route and per backend, plus pool,
health, ejection, retry-budget and config-version gauges, with
`prometheus-client` in the stack. The hot-path rule forbids locks.
`prometheus-client`'s `Family::get_or_create` takes a read lock and needs an
owned label set (an allocation) per lookup, and its `Histogram::observe` takes
a write lock. Envoy keeps per-cluster and per-route stats in pre-allocated
slots and reads host-level data at scrape time.

The added-latency SLO (p50 < 0.2 ms, p99 < 1 ms) needs a measurement of
Ferry's own time, which no client-side load test reports per request.

## Decision

- `ferry-telemetry::Metrics` owns stats objects keyed by name
  (`ListenerStats`, `RouteStats` per listener and route, `ClusterStats`),
  made of relaxed atomics and a lock-free fixed-bucket `Histogram` (one
  atomic per bucket, the sum in integer nanoseconds). When a snapshot is
  installed, the runtime fetches each `Arc` by name; requests only bump
  atomics. The same name returns the same `Arc`, so counters survive reloads;
  `Metrics::retain` drops names that left the config. A mutex guards the maps
  but is touched only on reload and scrape.
- Export uses `prometheus-client` collectors and its OpenMetrics text
  encoder (`/metrics`, `application/openmetrics-text`).
- Live state (endpoint health, ejections, in-flight requests, latency EWMA,
  idle pool connections, breaker usage and maxima, retry-budget headroom,
  eligible hosts, panic mode, config version, draining) is read from the
  runtime by a scrape-time collector, so it cannot go stale and per-endpoint
  series disappear with the endpoint.
- Labels are bounded: `listener`, `route` (`-` for unmatched), `cluster`,
  `code_class` (1xx–5xx, not the exact code), local-reply `reason` (10
  values), `breaker` (4), `grpc_code` (17, only non-zero). Pod IPs appear
  only on scrape-time endpoint gauges.
- **Added latency** (`ferry_request_overhead_seconds{listener}`): time from
  request arrival to response headers, minus time spent waiting for upstream
  response headers (every attempt) and retry backoff. Connect time stays in:
  dialing is Ferry's pool behaviour. Buckets run from 25 µs to 100 ms, dense
  around 0.2 ms and 1 ms.
- `ferry_request_duration_seconds{listener,route}` covers arrival to the last
  response byte, recorded when the response body finishes or is dropped; a
  dropped body also counts in `ferry_downstream_incomplete_responses_total`.
- `process_*` metrics come from procfs (Linux only, the deployment target).

## Consequences

- Per request: about 22 ns of metrics (`telemetry/histogram_observe` 9 ns,
  `telemetry/route_record` 13 ns on the dev box). The M1 load run at 10k
  req/s with metrics on shows p50 within noise of M2.
- Counters for a hot route are shared cache lines across cores. At 10k req/s
  per core this is noise; if a profile ever shows contention, shard per core
  (the handle API does not change).
- The overhead histogram measures inside the process: kernel and NIC time is
  excluded. The k6 comparison against a direct run remains the outside view;
  gates report both.
- Exact status codes are not labels; the access log has them.
