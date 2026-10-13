# FerryHigh5xxRate

**Severity:** critical when the 5xx ratio burns the 99.95% success SLO 14×
faster than allowed (over both 5 minutes and 1 hour); warning at 6× (over
30 minutes and 6 hours).

## Meaning

More than 0.7% (critical) or 0.3% (warning) of a route's downstream
responses are 5xx. The alert is per route of one Gateway's proxy fleet
(`fleet` is the fleet's Deployment name, `ferry-<gateway-ns>-<gateway>`):

```promql
sum by (fleet, route) (rate(ferry_requests_total{code_class="5xx"}[5m]))
  / sum by (fleet, route) (rate(ferry_requests_total[5m]))
```

5xx responses come either from a backend, passed through, or from Ferry
itself (a *local reply*). The fix differs, so tell them apart first.

## Impact

Clients of that route see errors. This alert is the SLO: treat critical as
an outage.

## Diagnosis

1. **Local or upstream?** On the dashboard, compare "Local replies by
   reason" with "Upstream 5xx ratio" and "Upstream failures", or query:

   ```promql
   sum by (reason) (rate(ferry_local_replies_total{fleet="$fleet"}[5m]))
   sum by (cluster, code_class) (rate(ferry_upstream_responses_total{fleet="$fleet"}[5m]))
   ```

2. **Local replies, by reason:**

   | Reason | Status | Usually means | Go to |
   | --- | --- | --- | --- |
   | `no_healthy_upstream` | 503 | Every endpoint of the cluster is unhealthy or ejected | [backends-ejected.md](backends-ejected.md) |
   | `overflow` | 503 + `x-ferry-overloaded` | A circuit breaker is full | step 4 |
   | `upstream_failure` | 502 | Connect failures, resets, or invalid responses | step 3 |
   | `timeout` | 504 | Route or per-try timeout | step 3 |
   | `misconfigured` | 500 | A backendRef is unresolved (missing Service, missing ReferenceGrant) or all weights are 0 | step 5 |

3. **Upstream trouble.** Check connect failures, resets and timeouts per
   cluster (`ferry_upstream_connect_failures_total`,
   `ferry_upstream_resets_total`, `ferry_upstream_attempt_timeouts_total`)
   and the backend's own logs and metrics. `curl localhost:19001/clusters`
   on a proxy shows per-endpoint health, ejection, in-flight requests and
   latency EWMA. A single bad endpoint should be ejected by outlier
   detection; if it is not, check the cluster's `outlier_detection` and
   `max_ejection_percent`.

4. **Breakers full** (`ferry_circuit_breaker_overflows_total` rising): the
   backend is slower than usual, or traffic grew past the limits.
   `ferry_circuit_breaker_active` against `ferry_circuit_breaker_max` shows
   which limit. Raise the limit in the route's FerryPolicy only if the
   backend can take it; otherwise scale the backend.

5. **Configuration.** Check the route's status:

   ```sh
   kubectl -n <route-ns> get httproute <name> -o jsonpath='{.status.parents[*].conditions}'
   ```

   `ResolvedRefs=False` names the broken reference (`BackendNotFound`,
   `RefNotPermitted`, `InvalidKind`).

6. **Recent change?** Compare with deploys of the backend and with
   `ferry_config_version` changes on the proxies. A bad canary weight shows
   up as 5xx from only one cluster of the route.

## Mitigation

- Backend at fault: roll back or scale the backend. Shift traffic away by
  setting the bad backendRef's `weight` to 0 in the HTTPRoute.
- Breakers or timeouts too tight for a known-slow backend: raise them in
  the FerryPolicy.
- Ferry itself suspected (5xx across all routes with healthy backends): roll
  the proxy Deployment back to the previous image
  (`helm rollback ferry`), and capture `/clusters` and logs from one proxy
  first.

## Escalation

Critical for more than 15 minutes with healthy backends: page the Ferry
on-call and open an incident.
