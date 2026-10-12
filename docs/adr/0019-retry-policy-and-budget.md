# ADR 0019: Retry policies spend a per-cluster retry budget

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2
- **Extends:** [ADR 0011](0011-upstream-pool-and-safe-retries.md)

## Context

ADR 0011 retries only what provably never reached an upstream. Operators also
want to retry responses that did reach one (503 during a deploy, a timed-out
attempt), which can multiply load on a struggling backend. The plan asks for
"retries with a retry budget (a percentage of active requests, plus a minimum
retries-per-second)". The Gateway API's retry shape (GEP-1731) is
`{ attempts, codes, backoff }`.

## Decision

- **Policy** (`rules[].retry`): `attempts` (0–10, default 2), `codes` (500–599,
  default 502, 503, 504) and `backoff` (default 25 ms). The n-th policy retry
  waits a random time up to `backoff × 2^(n-1)`, capped at `10 × backoff`
  (exponential backoff with full jitter).
- **What a policy retries:**
  - a response whose status is in `codes`;
  - a connection reset before response headers, counted as 502;
  - an attempt that hits `backend_request`, counted as 504.

  Each retry prefers an endpoint not yet tried. Nothing is retried once a
  response has been accepted.
- **Replay:** a request body is buffered before the first attempt when a policy
  applies and `Content-Length` is at most 64 KiB, so it can be resent. Larger
  or chunked bodies stream, and get only ADR 0011's safe retries. With a policy
  any method may be retried (the operator opted in); without one, only
  idempotent bodiless requests are resent after a reset.
- **Budget** (`clusters[].retry_budget`, Linkerd's model): every request
  deposits a token; every policy retry must withdraw one. Withdrawals over the
  last `ttl` (default 10 s) may not exceed `budget_percent` (default 20) of
  deposits plus `min_retries_per_second × ttl` (default 10/s). The budget is
  checked *before* a retriable response is discarded, so when it is exhausted
  the client gets the upstream's real response rather than a synthesised error.
- **Safe retries** (ADR 0011) do not spend budget: they never send a request
  twice, so they cannot add load to a backend.

## Consequences

- A backend failing every request receives at most 1.2× normal traffic (plus
  the floor), not 3× as with unbudgeted 2-attempt retries.
- Buffering trades up to 64 KiB of memory per in-flight retriable request, and
  some latency for bodies of that size, for retry safety.
- The budget is per cluster and per replica; it resets when the cluster's
  config changes.
