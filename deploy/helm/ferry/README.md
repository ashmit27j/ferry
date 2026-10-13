# Ferry Helm chart

Installs the Ferry Gateway API controller. The controller provisions one
proxy fleet per Gateway of its GatewayClass (ADR 0035).

```sh
helm install ferry deploy/helm/ferry --namespace ferry-system --create-namespace
```

`values.yaml` targets AWS EKS behind an NLB (AWS Load Balancer Controller
required). For a local kind cluster with MetalLB, add
`-f deploy/helm/ferry/values-kind.yaml`; `bench/exit/run-m4.sh` does the whole
thing, including the Gateway API conformance suite.

## What it installs

| Object | Notes |
| --- | --- |
| CRDs (`crds/`) | `FerryPolicy` and the Gateway API v1.6.3 standard channel. Helm installs CRDs once and skips ones that exist (EKS clusters usually have the Gateway API already). |
| `ferry-certgen` Job (hook) | Issues the CA and certificates of the controller-to-proxy stream into `ferry-controller-tls` and `ferry-proxy-tls`, on install and upgrade, renewing within 90 days of expiry. |
| Controller Deployment | 2 replicas, Lease leader election, snapshot gRPC on 18000 (mTLS), `/healthz` `/readyz` `/metrics` `/debug/proxies` on 18001; PodDisruptionBudget. |
| GatewayClass `ferry` | `controllerName: gateway.ferry.dev/controller`. |
| Gateway (optional) | `gateway.create: true` makes `helm install` alone yield a working Gateway. |
| RBAC | Read-only cluster access plus status writes; a namespaced Role for fleets (including their NetworkPolicies) and the Lease; proxies get no API token. |
| NetworkPolicies | Only proxies reach the controller's snapshot port; only `networkPolicy.metricsFrom` reaches the metrics ports. The controller renders each fleet's policy (listener ports open to all sources). |
| PodMonitors (optional) | Proxies (with a `fleet` label per Gateway) and the controller, when kube-prometheus-stack's CRD exists. |
| PrometheusRule (optional) | The alerts in `files/alerts.yaml`, each with a runbook in [`docs/runbooks`](../../../docs/runbooks/README.md). |

Each Gateway then gets, in the release namespace: a proxy Deployment
(`proxy.replicas`, default 3, or an HPA from 3 to 10), a PodDisruptionBudget
(`maxUnavailable: 1`), zone and node topology spread, and a `LoadBalancer`
Service annotated for an NLB with IP targets and PROXY protocol v2. Pods run
as UID 65532 on a read-only root filesystem from a distroless image, with all
capabilities dropped; Gateway ports below 1024 map to container port
`port + 10000`.

## Key values

| Value | Default | Meaning |
| --- | --- | --- |
| `image.repository` / `tag` | `ghcr.io/ashmit27j/ferry` / appVersion | One image for controller, proxy and certgen. |
| `proxy.replicas` | `3` | Per Gateway, when autoscaling is off. |
| `proxy.autoscaling.*` | on, 3-10, 70% CPU | HPA per Gateway. |
| `proxy.service.awsNlb.enabled` | `true` | NLB annotations (IP targets, scheme, deregistration delay). |
| `proxy.proxyProtocol` | `true` | Proxies require a PROXY v2 header; keep in step with the NLB. |
| `proxy.drainDelaySeconds` | `35` | Keep accepting after SIGTERM while the NLB deregisters (more than its 30 s delay). |
| `proxy.accessLog` / `otlpEndpoint` | off / unset | Observability (ADR 0028, 0029). |
| `controller.replicas` | `2` | `1` disables leader election. |
| `gateway.create` | `false` | Create a Gateway named `ferry` with an HTTP listener on 80. |
| `metrics.prometheusRule.enabled` | `true` | Install the alerting rules (needs the PrometheusRule CRD). |
| `metrics.prometheusRule.labels` | `{}` | Labels your Prometheus selects rules by (for example `release: kube-prometheus-stack`). |
| `metrics.prometheusRule.runbookBaseUrl` | the repo's `docs/runbooks` | Where each alert's `runbook_url` points. |
| `networkPolicy.enabled` | `true` | Controller and per-fleet NetworkPolicies. |
| `networkPolicy.metricsFrom` | every namespace | NetworkPolicy peers allowed to scrape 18001 and 19001; narrow to your monitoring namespace. |

## Certificate rotation

The certgen hook renews certificates on `helm upgrade` when they are within
90 days of expiry. The controller and proxies read them at start, so restart
both afterwards:

```sh
kubectl -n ferry-system rollout restart deployment
```
