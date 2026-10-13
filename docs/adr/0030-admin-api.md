# ADR 0030: A read-only admin API on its own address

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M3

## Context

The plan asks for an admin API (axum, on a separate port bound to localhost
or the pod IP) exposing `/ready`, `/live`, `/config` (redacted), `/clusters`
and `/metrics`. Kubelet probes and Prometheus must reach it, so in a pod it
binds the pod IP. Envoy's admin API also has mutating endpoints
(`/quitquitquit`, `/runtime_modify`), a known risk when exposed.

## Decision

- `ferry_proxy::admin` serves only GET endpoints: `/live` (200 while the
  process runs), `/ready` (200 while serving, 503 from the moment drain
  starts, ADR 0026), `/metrics`, `/config` (the active snapshot as JSON;
  private keys serialize as `<redacted>`) and `/clusters` (per-endpoint
  health, ejection, in-flight requests, latency EWMA and idle connections;
  per-cluster breakers and retry budget).
- `--admin-address`, default `127.0.0.1:9901` (Envoy's port), or `off`; the
  Helm chart binds the pod IP. The admin address is a process setting, not
  part of the snapshot, so a controller-pushed config cannot move it.
- Metrics are encoded in `spawn_blocking`, because procfs reads block.
- Readiness does not depend on upstream health: a proxy that answers 503 for
  one dead cluster must still receive traffic for the healthy ones.

## Consequences

- Anyone who can reach the pod IP can read routing topology and endpoint
  addresses, but cannot change anything. A NetworkPolicy can restrict the
  port to Prometheus and the kubelet; the M4 chart should ship one.
- There is no authentication on the admin port, as with Envoy. Revisit if a
  mutating endpoint is ever added.
