# Ferry runbooks

One page per alert in [`deploy/helm/ferry/files/alerts.yaml`](../../deploy/helm/ferry/files/alerts.yaml).
Every alert's `runbook_url` annotation points here, and
`tests/it/alerts.rs` checks that each alert has a page and that every metric
the rules use exists.

| Alert | Severity | Page |
| --- | --- | --- |
| `FerryHigh5xxRate` | critical / warning | [high-5xx.md](high-5xx.md) |
| `FerryBackendsEjected` | warning | [backends-ejected.md](backends-ejected.md) |
| `FerryNoEligibleBackends` | critical | [backends-ejected.md](backends-ejected.md) |
| `FerryCertificateExpiringSoon` | warning | [certificate-expiry.md](certificate-expiry.md) |
| `FerryCertificateExpiryCritical` | critical | [certificate-expiry.md](certificate-expiry.md) |
| `FerryConfigRejected` | warning | [config-rejected.md](config-rejected.md) |
| `FerryControllerDisconnected` | warning | [controller-disconnected.md](controller-disconnected.md) |
| `FerryControllerDown` | critical | [controller-disconnected.md](controller-disconnected.md) |

## Before you start

- **Proxies keep serving.** A proxy that loses its controller, or rejects a
  snapshot, keeps serving its last good snapshot. Most alerts here are about
  what *will* go wrong, not an outage, except `FerryHigh5xxRate` and
  `FerryNoEligibleBackends`.
- **Where things are.** The controller runs in the release namespace
  (default `ferry-system`) as the Deployment `<release>-controller` (`ferry-controller`
  for a release named `ferry`). Each Gateway's proxies run there too, as the
  Deployment `ferry-<gateway-namespace>-<gateway-name>` (dots become dashes,
  long names are shortened with a hash).
- **Tools.** Each proxy has an admin API on port 19001 (`/ready`, `/config`
  with keys redacted, `/clusters`, `/metrics`). The controller serves
  `/debug/proxies` and `/metrics` on port 18001.

```sh
NS=ferry-system
kubectl -n $NS get pods -l app.kubernetes.io/name=ferry-controller -o wide
kubectl -n $NS get pods -l app.kubernetes.io/name=ferry-proxy -L app.kubernetes.io/instance -o wide
kubectl -n $NS port-forward deploy/ferry-<gw-ns>-<gw-name> 19001:19001   # then curl localhost:19001/clusters
kubectl -n $NS port-forward deploy/ferry-controller 18001:18001          # then curl localhost:18001/debug/proxies
```

- **Dashboards.** Grafana dashboard "Ferry" (uid `ferry-overview`): the SLO
  row first, then traffic, upstreams, resilience and process rows.
