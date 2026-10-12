# ADR 0014: Carry balancer, health and pool state across reloads

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M1
- **Amends:** [ADR 0005](0005-config-model-and-snapshots.md), [ADR 0007](0007-deterministic-weighted-selection.md)

## Context

In M0 every reload rebuilt all runtime state. With the controller pushing
snapshots often, and the M1 exit test reloading every second at 10k req/s,
rebuilding everything would reset least-request counts and latency estimates,
restart health checks (forgetting that an endpoint is dead) and drop warm
connections.

## Decision

The proxy's control state, touched only under a mutex off the hot path, builds
each new runtime by reusing what has not changed (as Envoy keeps a cluster whose
config is unchanged):

- A **cluster** whose `Cluster` value is identical keeps its whole runtime:
  balancer, cursor, host set and Maglev table.
- A **listener** whose routes are identical keeps its router, including the
  weighted-split counters of ADR 0007.
- **Endpoint state** (health, in-flight count, latency EWMA) is keyed by
  `(cluster name, address)` and reused by a changed cluster that still lists
  the endpoint.
- **Connection pools** are keyed by `(address, protocol)`, shared across
  clusters, and forgotten only when no cluster lists the endpoint.
- **Health checkers** keep running when the endpoint and settings are unchanged,
  and are started or cancelled to match the new snapshot.

Reloads and health-driven host-set rebuilds take the same lock, so a health
change is never applied to a runtime that is about to be replaced.

## Consequences

- 66 reloads during the exit test caused no errors and no connection churn.
- Requests in flight keep the runtime they started with; an old runtime and its
  pools are freed when the last such request finishes.
- Listener addresses are still bound once: an added listener or a changed
  address takes effect on restart, and a removed listener answers 503.
