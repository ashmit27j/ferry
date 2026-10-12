# ADR 0009: Match the full Gateway API vocabulary, indexed by hostname

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M1
- **Amends:** [ADR 0005](0005-config-model-and-snapshots.md) (precedence)

## Context

M0 matched host and path prefix/exact by scanning every route on a listener:
360 ns at 100 routes and growing linearly. M1 adds the rest of `HTTPRouteMatch`:
regular-expression paths, method, headers and query parameters. The Gateway API
fixes most of the precedence but leaves some details to implementations:
where regular expressions rank, how repeated headers or query parameters
compare, and whether query values are percent-decoded.

## Decision

**Precedence.** Hostname first: an exact hostname beats the longest matching
wildcard suffix, which beats routes without hostnames. If no rule in a hostname
group matches, the next less specific group is tried (as in ADR 0005). Within a
group the order is: path kind (Exact, then RegularExpression, then PathPrefix),
then path length, then presence of a method condition, then number of header
conditions, then number of query conditions, then route order, then rule order.
Ranking regular expressions between exact and prefix matches is what Envoy
Gateway does.

**Matching details.**

- Regular expressions use the `regex` crate (linear time, no backtracking), are
  implicitly anchored (`^(?:…)$`), and are limited to 1 KiB of source and 1 MiB
  of compiled program.
- Repeated request headers are joined with `,` before comparing, as Envoy does.
- Query parameters compare the first occurrence, raw (not percent-decoded);
  `?flag` has the value `""`.
- Methods are limited to the Gateway API's `HTTPMethod` enum.
- At most 16 header conditions, 16 query conditions and 16 filters per rule (the
  Gateway API limits).

**Index.** `Router::new` builds one candidate list per hostname group, holding
every (route, rule, match) triple already sorted by the precedence above: a map
of exact hostnames, a map of wildcard suffixes (`.example.com`) and a catch-all
list. A lookup walks the request host's suffixes from longest to shortest (one
hash lookup per label) and returns the first candidate that matches.

## Consequences

- Routing is ~44 ns for a hit at both 100 and 1,000 routes (was 360 ns at 100).
- A property test checks the index against a brute-force reference that applies
  the precedence rules directly.
- Candidates are duplicated per hostname a route lists; memory grows with
  hostnames × matches, which is small for realistic tables.
- Gateway API conformance tests (M4) are the final arbiter of these choices.
