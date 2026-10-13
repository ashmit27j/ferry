# FerryBackendsEjected and FerryNoEligibleBackends

**Severity:** `FerryBackendsEjected` is a warning: at least a third of a
cluster's endpoints have been ejected by outlier detection for 5 minutes.
`FerryNoEligibleBackends` is critical: a cluster has had no eligible endpoint
(healthy and not ejected) for 2 minutes, so every request to it gets a 503.

## Meaning

Each proxy keeps its own view of every endpoint:

- **Active health checks** mark an endpoint unhealthy after
  `healthCheck.unhealthyThreshold` failed probes (`ferry_endpoint_healthy`).
- **Outlier detection** ejects an endpoint after consecutive 5xx or gateway
  errors (`ferry_endpoint_ejected`). Ejection time grows with each
  ejection, and at most `outlierDetection.maxEjectionPercent` of a cluster is
  ejected at once.
- **Panic mode.** If too few endpoints are healthy (below
  `panicThreshold`, default 50%), the balancer ignores health and spreads load
  over all of them (`ferry_cluster_panic`), as Envoy does.

```promql
max by (fleet, cluster) (ferry_cluster_ejected_endpoints / ferry_cluster_endpoints)   # ejected share
min by (fleet, cluster) (ferry_cluster_eligible_endpoints)                             # 0 = outage
```

## Impact

- Ejected endpoints get no traffic, so the rest carry more. If they cannot,
  latency rises and breakers fill.
- No eligible endpoints: the route answers 503 `no_healthy_upstream` (or
  gRPC `UNAVAILABLE`), unless panic mode kicks in.

## Diagnosis

1. **Which endpoints, and why?** On one proxy of the fleet:

   ```sh
   kubectl -n ferry-system port-forward deploy/<fleet> 19001:19001
   curl -s localhost:19001/clusters | jq '.clusters[] | select(.name=="<cluster>")'
   ```

   Each endpoint shows `healthy`, `ejected`, ejection count, in-flight
   requests and latency EWMA.

2. **Is the backend really failing?** Check its pods (`kubectl get pods`,
   restarts, readiness) and its own error metrics. Ferry's per-cluster
   `ferry_upstream_responses_total{code_class="5xx"}`,
   `ferry_upstream_connect_failures_total` and `ferry_upstream_resets_total`
   show what Ferry saw.

3. **Health check wrong?** If pods look fine but `ferry_endpoint_healthy` is
   0, the probe may target the wrong path or port, or expect the wrong
   status. Compare the FerryPolicy `healthCheck` with what the backend
   serves: `curl` the probe path from a debug pod.

4. **EndpointSlice readiness.** Endpoints that Kubernetes marks not-ready
   are not in the snapshot at all. If `ferry_cluster_endpoints` is lower
   than the pod count, check the pods' readiness probes and
   `kubectl get endpointslices -l kubernetes.io/service-name=<svc>`.

5. **One proxy or all?** Each proxy ejects on its own. If only one proxy
   ejects, suspect its network path (node, zone) rather than the backend.

## Mitigation

- Backend failing: fix or roll back the backend; outlier detection returns
  endpoints automatically once they succeed again (after the ejection time).
- Ejection too aggressive for a backend with expected errors (for example a
  service that returns 503 under deliberate load shedding): raise
  `outlierDetection.consecutive5xx` or lower `maxEjectionPercent` in the
  FerryPolicy.
- Health check misconfigured: fix the FerryPolicy `healthCheck`.
- Last resort during an outage: remove `outlierDetection` from the FerryPolicy
  to put ejected endpoints back at once.

## Escalation

`FerryNoEligibleBackends` is an outage for that route: page the backend
owner first, then the Ferry on-call if the backend looks healthy.
