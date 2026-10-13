# ADR 0029: JSON access logs written off the request path

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M3

## Context

The plan asks for JSON access logs. Writing to stdout is a blocking syscall
and must not run on a tokio worker per request. Envoy writes access logs
through an asynchronous buffered sink and logs at stream end, so duration and
byte counts are final.

## Decision

- One JSON object per line, emitted when the response body finishes or is
  dropped. Field names follow Envoy's where one exists: `start_time`
  (RFC 3339, ms), `duration_ms`, `upstream_service_time_ms`, `listener`,
  `route`, `upstream_cluster`, `upstream_host`, `attempts`, `method`,
  `authority`, `path` (with query), `protocol`, `response_code`,
  `grpc_status`, `response_flags`, `bytes_sent`, `downstream_remote_address`,
  `x_forwarded_for`, `user_agent`, `request_id`, `trace_id`. Absent values
  are omitted.
- `response_flags` uses Envoy's letters where they exist: `NR` no route, `RL`
  rate limited, `UH` no healthy upstream, `UO` overflow, `UF` upstream
  failure, `UT` timeout, `DC` response not delivered in full. Ferry adds `BTL`
  (body too large), `BT` (body timeout), `BR` (bad request) and `MC`
  (misconfigured).
- Request fields are captured when the request arrives, so local replies
  (404, 429) are logged too.
- Lines are serialized on the request thread and handed to a dedicated writer
  thread through a bounded channel (16,384 lines). When it is full, lines are
  dropped and counted (`ferry_access_log_dropped_total`); the request never
  waits. The writer flushes whenever its queue runs dry and on shutdown.
- `--access-log stdout|stderr|<path>|off`, default off, so local runs stay
  quiet; the Helm chart turns it on. Ferry's own logs stay on stderr.

## Consequences

- Cost: about 0.65 µs to serialize a line (`telemetry/access_log_line`). The
  M1 load run at 10k req/s on one core shows about +21 µs at p50 with the log
  on (0.167 → 0.188 ms), zero errors and zero drops.
- During a log pipeline outage or on a slow disk, lines are lost rather than
  latency added. The drop counter makes that visible.
- Request fields are copied (cheap `Bytes` clones) only when the log is on.
