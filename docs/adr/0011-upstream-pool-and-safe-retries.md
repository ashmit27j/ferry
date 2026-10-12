# ADR 0011: Own the upstream connection pool; retry only what provably did not arrive

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M1

## Context

The M1 exit test kills a backend under 10k req/s and allows zero errors. Until a
health check notices (one interval or more), the balancer keeps choosing the
dead endpoint: new connections are refused and pooled ones reset. Configurable
retry policies and budgets are M2 work. M0 used hyper-util's legacy `Client`,
which connects inside `request()` and drops the request on a connect error, so
even a refused connect could not be retried for a request with a body. It also
derives `:authority` from the connect address, which is wrong for HTTP/2
upstreams.

## Decision

1. **Own pool** (`ferry_lb::pool`), one per `(address, protocol)`:
   - HTTP/1.1: idle connections in a lock-free `SegQueue` (at most 1,024 per
     endpoint, closed after 90 s idle). After response headers arrive, a task
     waits for the connection to finish the response and returns it, as hyper's
     pooled client does.
   - h2c: one shared connection per endpoint, multiplexed, with keep-alive pings
     every 20 s; a closed connection is replaced under a dial lock.
   - The pool connects before it takes the request (connect timeout per cluster,
     default 5 s).
2. **Safe retries:** up to 3 endpoints per request (`MAX_ATTEMPTS`), a different
   one each time when possible, in two cases only:
   - the request provably never reached an upstream: the connect failed, or
     hyper's `try_send_request` handed the request back unwritten. Any method
     qualifies.
   - the connection broke before response headers, and the request is idempotent
     (`GET`, `HEAD`, `OPTIONS`, `TRACE`, `PUT`, `DELETE`) with an empty body. Its
     headers are kept so it can be replayed.

   Nothing is retried after response headers arrive.
3. **Authority:** h2 upstream requests carry the client's `Host` as `:authority`
   (as Envoy does); HTTP/1.1 upstreams get origin-form URIs and the client's `Host`.
4. **Local replies:** 503 "no healthy upstream" when no endpoint is eligible;
   502 "upstream unavailable" when every attempt failed.

## Consequences

- Killing a backend under load produced 0 errors in 600,001 requests
  (bench/RESULTS.md).
- Both retry cases are always safe, so they are on by default and need no
  budget. M2's retry policy (5xx, timeouts, budgets) builds on the same loop.
- Replay keeps a copy of the headers of idempotent bodiless requests: one
  `HeaderMap` clone each. Measure in M5 and drop the copy if it shows.
- On Windows a refused connect takes ~2 s (SYN retries), so retries there are
  slow; Linux refuses at once.
- We now maintain pool code that hyper-util would otherwise maintain. In
  exchange, mTLS (M2), circuit breakers (M2) and pool metrics (M3) attach to
  code we own.
