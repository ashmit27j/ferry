# ADR 0005: Gateway API-shaped config, validated into immutable snapshots

- **Status:** Accepted; precedence amended by [ADR 0009](0009-route-matching-and-precedence.md), reload state by [ADR 0014](0014-state-across-reloads.md), API frozen by [ADR 0013](0013-freeze-ferry-config-api.md)
- **Date:** 2026-10-13
- **Milestone:** M0

## Context

The same configuration must come from two places: a YAML file (standalone mode,
tests) and the controller's reconciliation of Gateway API resources (M4). The
data plane must never see a half-valid config, and reloads must not affect
in-flight requests.

## Decision

1. **Shape mirrors the Gateway API.** Listeners hold routes (≈ `HTTPRoute`
   attached to a `Gateway` listener); routes hold `hostnames` and `rules`; rules
   hold `matches` (`path: { type: Exact | PathPrefix, value }`) and weighted
   `backends`. Clusters are named endpoint groups (≈ a Service's
   EndpointSlices). The controller's translation is therefore close to 1:1.
2. **Two types, one direction.** `Config` is the serde form (`deny_unknown_fields`).
   `Config::into_snapshot(version)` validates everything and returns either a
   `Snapshot` or *all* errors with field paths. `Snapshot` has private fields and
   no mutators; holding one proves validity. Cluster references are resolved to
   `ClusterId` indices, so the hot path never looks up by name.
3. **Gateway API semantics where they apply:**
   - Empty `matches` means `PathPrefix /`. A trailing `/` on a prefix is ignored.
     Prefixes match on `/` element boundaries (`/foo` matches `/foo/bar`, not
     `/foobar`).
   - Hostnames are lowercased; `*.example.com` matches one or more labels but not
     `example.com`; IP addresses are rejected.
   - Backend weights are `0..=1_000_000`. A rule whose weights are all zero
     matches and returns **500**, as the spec requires, rather than failing
     validation.
   - Precedence across all routes on a listener: most specific hostname (exact,
     then longest wildcard suffix, then no hostnames), then exact path, then
     longest prefix, then the route and rule listed first. We read the spec's
     "precedence must be given to rules from the HTTPRoute with the most specific
     hostname" as applying among *matching* rules, so a request for
     `api.example.com/other` falls back to a `*.example.com` route when the exact
     route has no rule for `/other`. Conformance tests in M4 confirm or overturn
     this.
4. **Reload = swap.** `ferry-proxy` wraps a `Snapshot` in a `Runtime` (routers,
   balancer counters, pre-parsed upstream authorities) and publishes it with
   `ArcSwap::store`. Each request does one `load_full()` and keeps that `Arc`
   until it finishes.

## Consequences

- `check-config` and the controller share one validator, so the controller can
  NACK exactly what a file user would see.
- Balancer counters reset on every swap. Acceptable for round robin; M1 must
  carry over state for least-request and P2C-EWMA, and decides how.
- Listener addresses are bound once; a swap that changes listeners takes effect
  on restart. M1 decides whether to support live listener changes.
- The public API of `ferry-config` is frozen, and tagged in a new ADR, at the
  end of M1.
