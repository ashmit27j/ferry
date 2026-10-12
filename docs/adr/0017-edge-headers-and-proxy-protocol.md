# ADR 0017: Client identity at the edge: PROXY protocol, X-Forwarded-*, X-Request-Id

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

Ferry runs behind an AWS NLB. With IP targets the NLB's own address replaces
the client's at the TCP level, so the real client IP only survives if the NLB
prepends a PROXY protocol header. Backends expect `X-Forwarded-For`,
`X-Forwarded-Proto` and a request ID. Client IP also feeds Maglev source-IP
stickiness and rate limiting.

## Decision

- **PROXY protocol** (`proxy_protocol: true` per listener): v1 and v2 are
  accepted. The header must arrive within 5 s and is parsed by a small,
  allocation-free, fuzzed parser (`ferry_proxy::proxy_protocol::parse`). Ferry
  peeks at the socket and consumes exactly the header, so the HTTP bytes behind
  it are untouched. LOCAL commands (the NLB's health checks) and non-TCP
  families use the socket peer. A connection without a valid header is closed
  without a response, as in Envoy.
- **X-Forwarded-For:** the client IP is appended, as Envoy does with
  `use_remote_address: true`. Ferry is the trust boundary; backends that need
  the client should read the last entry.
- **X-Forwarded-Proto:** overwritten with the listener's scheme (`http`, or
  `https` once TLS terminates).
- **X-Request-Id:** a client value is kept if it is 1–128 visible ASCII bytes,
  otherwise a random UUIDv4 is generated. The ID is sent upstream and echoed on
  the response, so a client can quote it and an operator can find the request
  in the access logs (M3).

## Consequences

- Client IP is correct behind the NLB for logging, stickiness and rate limits.
- A listener with `proxy_protocol: true` cannot be reached directly by a
  client that does not send the header; local testing uses a listener without it.
- Clients can choose their own request IDs. That is useful for tracing and
  harmless: the ID identifies a request but never authorises anything.
