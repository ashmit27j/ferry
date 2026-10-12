# ADR 0007: Split weighted backends deterministically with a golden-ratio stride

- **Status:** Accepted; counters now survive reloads that leave the routes unchanged ([ADR 0014](0014-state-across-reloads.md))
- **Date:** 2026-10-13
- **Milestone:** M0

## Context

`backendRefs` weights drive canary and blue-green splits. Envoy's weighted
clusters pick by random number, which is exact only on average: at low request
rates a 1% canary can get nothing or several percent. Naive deterministic
selection (walk cumulative weights with a counter) is exact but bursty: a 90/10
split sends nine requests in a row to stable, then one to the canary.

## Decision

`ferry_lb::WeightedSelector` takes a ticket from an `AtomicU64`, maps it into the
weight space with `ticket * stride mod total`, and binary-searches the cumulative
weights. `stride` is the first integer ≥ 0.618 × total that is coprime with
total, so the map is a permutation of the weight space that spreads consecutive
tickets apart.

## Consequences

- Every window of `total` consecutive picks matches the weights exactly (a
  property test asserts this), and picks are interleaved.
- One relaxed atomic increment per pick; no locks, no RNG state.
- Each proxy replica runs its own counter, so the fleet-wide split is exact per
  replica and approximately exact overall.
- The per-snapshot counter resets on reload, which can skew a split briefly if
  config reloads more often than once per `total` requests.
