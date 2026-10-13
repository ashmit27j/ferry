# ADR 0028: OpenTelemetry spans with W3C trace context, exported over OTLP/gRPC

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M3

## Context

The plan asks for OpenTelemetry tracing with W3C `traceparent` propagation
over OTLP, to an in-cluster OpenTelemetry Collector and Grafana Tempo. The
exit criterion is a trace spanning client → Ferry → backend. Envoy creates an
ingress span per request and, optionally, an egress span per upstream attempt.
The `tracing-opentelemetry` bridge would put an OTel span behind every
`tracing` span, which costs more and couples request spans to log spans.

## Decision

- Use the `opentelemetry` SDK directly (`ferry-telemetry::trace`): a server
  span per request, the child of the incoming `traceparent` when valid, and a
  client span per upstream attempt under it. The attempt span's context is
  injected into the upstream request (`traceparent`, `tracestate`), so the
  backend's spans hang under the attempt that reached it.
- Names and attributes follow the HTTP semantic conventions: server span
  `"{method} {route}"` with `http.request.method`, `url.path`, `url.scheme`,
  `server.address`, `client.address`, `http.route`, `user_agent.original`,
  `http.response.status_code`, `rpc.grpc.status_code`, plus `ferry.listener`
  and `ferry.cluster`. Client spans carry `server.address` and `server.port`,
  and `http.request.resend_count` on retries. Server spans are errors for
  5xx, client spans for 4xx and 5xx.
- Sampling is parent-based: follow the caller's sampled flag; sample new
  traces at `--trace-sample-ratio` (default 1.0, the OTel default; the Helm
  chart will lower it). Unsampled requests still propagate a context, so
  backends agree with Ferry.
- Export with the SDK batch processor (its own thread; drops spans when its
  queue is full) and the tonic OTLP/gRPC exporter. Tracing is on only when
  `--otlp-endpoint` / `OTEL_EXPORTER_OTLP_ENDPOINT` is set; otherwise none of
  this runs and `traceparent` passes through as an ordinary header.

## Consequences

- Cost with tracing on: about 0.76 µs per request unsampled and 1.05 µs
  sampled (`telemetry/spans_*`, exporter excluded). With tracing off: zero.
- `tests/it/tracing.rs` proves the chain against a real OTLP collector
  in-process; `bench/exit/run-m3.sh` proves it end to end through the
  OpenTelemetry Collector into Tempo.
- The exporter's connection lives on the tokio runtime and flushes block a
  thread, so shutdown flushes run in `spawn_blocking`.
- OTLP over TLS is not compiled in; the collector is reached in-cluster.
  Revisit if a collector outside the cluster is needed.
- Access-log lines carry `trace_id`; Ferry's diagnostic logs do not.
