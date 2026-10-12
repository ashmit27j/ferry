# Ferry Helm chart

Arrives in milestone M4. Planned contents:

- CRDs (`FerryPolicy`) and least-privilege RBAC.
- Controller Deployment: 2 replicas with Lease-based leader election.
- Proxy Deployment: 3 or more replicas, PodDisruptionBudget, topology spread
  constraints and an HPA.
- A `LoadBalancer` Service annotated for an AWS NLB with IP targets and PROXY
  protocol v2.
- Pods run as non-root on a read-only root filesystem, from a distroless image.
