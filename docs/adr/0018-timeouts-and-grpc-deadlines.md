# ADR 0018: Timeouts, gRPC deadlines and gRPC-shaped local replies

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

v1 needs per-route request and idle timeouts. The Gateway API defines
`HTTPRouteTimeouts.request` (whole request) and `backendRequest` (one upstream
attempt). gRPC clients send their deadline in `grpc-timeout`. They also
expect every response, including errors Ferry generates itself, to be a valid
gRPC response; a plain-text 503 shows up client-side as a protocol error that
hides the cause.

## Decision

- **`request`** bounds everything from routing to response headers, retries
  included. Expiry cancels the attempt in progress (closing an HTTP/1.1
  upstream connection, or resetting the h2 stream) and answers 504
  `upstream request timeout`.
- **`backend_request`** bounds one attempt and must not exceed `request`.
- **`idle`** (default 5 minutes, as in Envoy) bounds silence on the request and
  response bodies. A stalled response body is reset; a stalled request body
  answers 408. The timer is only armed when a poll has to wait.
- **gRPC deadline:** for `application/grpc` requests, the effective request
  timeout is the smaller of the route's `request` and the client's
  `grpc-timeout`. `grpc-timeout` is forwarded unchanged.
- **gRPC local replies:** for gRPC requests, every reply Ferry generates is
  trailers-only (HTTP 200, `grpc-status`, `grpc-message`). Codes follow the gRPC
  spec's HTTP mapping: 404 becomes `UNIMPLEMENTED`; 429, 502 and 503 become
  `UNAVAILABLE`; 504 becomes `DEADLINE_EXCEEDED`; 413 and 431 become
  `RESOURCE_EXHAUSTED`.

## Consequences

- A slow backend cannot hold client connections beyond the configured bounds,
  and gRPC callers get `DEADLINE_EXCEEDED` from Ferry rather than a reset.
- Timeouts apply to response headers. Long streams (server streaming, downloads)
  are bounded only by `idle`, so they are not cut off while data keeps flowing.
- Forwarding `grpc-timeout` unchanged lets the backend over-estimate its time
  budget by the time already spent in Ferry. Envoy behaves the same by default;
  decrementing it is a later refinement.
