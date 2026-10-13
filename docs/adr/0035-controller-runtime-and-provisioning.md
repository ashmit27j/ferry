# ADR 0035: Provision a proxy fleet per Gateway; one elected writer

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M4

## Context

The conformance suite runs several Gateways on port 80 at once, which only
works if each Gateway has its own address. GEP-1762 recommends that
implementations deploy infrastructure per Gateway; Envoy Gateway and Istio do.
The plan asks for a 2-replica controller with Lease-based leader election,
and for proxies with at least 3 replicas, a PodDisruptionBudget, topology
spread, an HPA, a LoadBalancer Service for an AWS NLB with IP targets and
PROXY protocol v2, non-root, read-only root, distroless. The human chose
per-Gateway provisioning from Helm values.

## Decision

- **Fleets:** for every accepted Gateway of its classes, the controller
  server-side-applies a Deployment, Service, PodDisruptionBudget and
  (optionally) HPA named `ferry-<namespace>-<name>` (hashed to fit 63
  characters) **in its own namespace**, from a `ProxyTemplate` the chart
  renders from `values.proxy`. Objects carry the Gateway in an annotation
  and are deleted when their Gateway goes away (owner references cannot
  cross namespaces). Applies are skipped while unchanged and repeated every
  10 minutes to repair drift.
- **Programmed** means the fleet has a ready pod (which implies it applied a
  snapshot) and its Service has an address; Gateway `status.addresses` come
  from the Service's load balancer (or its cluster IP for `ClusterIP`).
- **Least privilege:** cluster-wide, the controller can only read the kinds
  it translates and patch the status of Gateway API objects and
  FerryPolicies. Writing Deployments, Services, PDBs, HPAs and its Lease is
  a namespaced Role. Proxies have no API access (no token mounted). The
  certgen hook has its own Role limited to Secrets in the release namespace.
  The controller lists only `kubernetes.io/tls` Secrets (a field selector),
  though RBAC itself cannot be narrowed that far.
- **Leader election** follows client-go: a Lease renewed every 2 s, valid
  for 15 s, taken over with a resourceVersion compare-and-swap; a leader that
  cannot renew for 10 s steps down; a stopping leader releases the Lease.
  Only the leader writes status and provisions; every replica translates
  and serves snapshots (ADR 0033).
- **Reconciling:** reflectors cache every watched kind; any event wakes one
  pass after a 200 ms debounce, plus a pass every 30 s. Status writes compare
  only the fields Ferry owns, keep each condition's transition time while
  its status holds, and leave other controllers' route parents alone (with
  the route's resourceVersion, so a concurrent writer causes a retry, not a
  lost update).

## Consequences

- A Gateway costs a Deployment of at least 3 pods by default; small
  clusters lower `proxy.replicas` (the kind values use 1).
- Fleet pods live in the controller's namespace, so NetworkPolicies and
  resource quotas for proxies are set there, not in application namespaces.
- `GatewayClass.parametersRef` is not supported (`InvalidParameters`);
  per-Gateway overrides of the template (GEP-1867 `infrastructure`) are a
  later addition.
