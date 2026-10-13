# FerryControllerDisconnected and FerryControllerDown

**Severity:** `FerryControllerDisconnected` is a warning: a proxy has had no
snapshot stream to the controller for 5 minutes. `FerryControllerDown` is
critical: Prometheus has scraped no ready controller replica
(`ferry_controller_ready`, 1 once its caches have synced) for 5 minutes.

## Meaning

Proxies stream snapshots from any controller replica over mutual TLS
(ADR 0033). When the stream breaks, a proxy keeps serving its last snapshot
and reconnects with backoff (250 ms, doubling to 10 s). A controller restart
or leader change normally reconnects within seconds, so 5 minutes means
something is persistently wrong.

```promql
max by (fleet, pod) (ferry_controller_connected) == 0     # per proxy
absent(ferry_controller_ready == 1)                       # no ready controller scraped
max(ferry_controller_leader) == 0                         # replicas up, no leader
```

## Impact

Traffic keeps flowing on the last snapshot. Nothing changes until the
stream is back: new routes, certificate renewals, endpoint changes (pods
added or removed by deploys and autoscaling) and status updates all stall.
Removed endpoints keep receiving traffic until health checks eject them.

## Diagnosis

1. **Is the controller running?**

   ```sh
   kubectl -n ferry-system get pods -l app.kubernetes.io/name=ferry-controller -o wide
   kubectl -n ferry-system logs -l app.kubernetes.io/name=ferry-controller --tail=100
   ```

   `/readyz` on 18001 is 503 until its caches have synced; a controller
   that never becomes ready cannot reach the Kubernetes API (RBAC, API
   server trouble).

2. **Can proxies reach it?** From a proxy's logs:

   ```sh
   kubectl -n ferry-system logs deploy/<fleet> | grep -i "controller"
   ```

   - `cannot connect` → Service or NetworkPolicy. Check the
     `ferry-controller` Service has endpoints and the NetworkPolicy admits
     proxy pods on 18000.
   - TLS errors (`certificate expired`, `unknown issuer`) → the stream
     certificates; see [certificate-expiry.md](certificate-expiry.md). A
     `helm upgrade` that renewed certificates needs a controller and proxy
     restart (residual risk R4).

3. **Does the controller see the proxies?**
   `curl localhost:18001/debug/proxies` (port-forwarded) lists connected
   proxies; `ferry_controller_connected_proxies` shows the count per
   Gateway.

4. **Leader election.** Status writes and provisioning need a leader:

   ```sh
   kubectl -n ferry-system get lease ferry-controller -o yaml
   ```

   Followers still serve snapshots, so a missing leader alone does not
   disconnect proxies.

## Mitigation

- Controller crash-looping: check logs, roll back with `helm rollback`.
  Proxies keep serving meanwhile.
- Certificates expired or renewed: restart the controller, then the proxies
  (`kubectl rollout restart`).
- NetworkPolicy blocking: fix `networkPolicy` values in the chart.
- Do **not** restart proxies to "fix" a disconnect while the controller is
  down: a restarted proxy starts with no snapshot and stays unready until it
  reaches a controller. The PodDisruptionBudget limits the damage, but the
  running proxies are the ones holding a working config.

## Escalation

`FerryControllerDown` for more than 15 minutes, or any disconnect during a
deploy that needs endpoint changes: page the Ferry on-call.
