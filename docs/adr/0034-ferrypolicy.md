# ADR 0034: FerryPolicy uses direct policy attachment

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M4

## Context

Most of Ferry's M1–M2 settings (balancing policies, health checks, circuit
breakers, outlier detection, retry budgets, rate limits, retries, listener
limits, upstream mTLS) have no Gateway API field. The plan asks for one CRD,
`FerryPolicy`, using the policy-attachment pattern (GEP-713), and for
cluster-wide rate limits that the controller divides between proxy
replicas. The human chose the API group `gateway.ferry.dev`.

## Decision

- `ferrypolicies.gateway.ferry.dev/v1alpha1`, namespaced, with
  `spec.targetRefs` naming objects in the policy's own namespace (direct
  attachment; no inheritance). Fields apply by target kind: `listener` to
  Gateways; `rateLimit`, `retry` and `timeouts.idle` to HTTPRoutes and
  GRPCRoutes; backend settings and `backendTLS` to Services. A field set for
  the wrong kind makes the policy `Accepted=False, reason=Invalid`.
- Conflicts: when two policies target one object, the older (then the first
  by namespace and name) wins; the other reports `reason=Conflicted`. A
  missing target is `TargetNotFound`. Status is a top-level `conditions`
  list with one `Accepted` condition.
- Durations use the Gateway API's GEP-2257 syntax, parsed by ferry-config.
- **Rate-limit split:** each ready proxy replica of a Gateway gets
  `ceil(requests / replicas)` per period and `ceil(burst / replicas)` of
  bucket, so the fleet together never allows less than the limit and at most
  one extra request per replica per period (proptest). The replica count is
  the fleet Deployment's `readyReplicas`; a change republishes the snapshot.
- `backendTLS` names `kubernetes.io/tls` Secrets in the policy's namespace
  (`ca.crt` to trust, `tls.crt`/`tls.key` for mTLS). If they cannot be read
  the cluster gets no endpoints (503) rather than falling back to plaintext.

## Consequences

- Per-replica division assumes the NLB spreads clients evenly; one heavy
  client hashed to one replica is limited at its share. Exact global limits
  need a shared store, which the plan puts after v1.0.
- Direct attachment is simple to reason about but cannot set defaults for a
  namespace; inherited policies (GEP-713's "inherited" kind) can be added as
  a new field without breaking this one.
- `v1alpha1` signals that fields may still move before v1.0.
