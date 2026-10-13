# FerryConfigRejected

**Severity:** warning. A proxy rejected a config change in the last 10
minutes, or the controller received a NACK.

## Meaning

A proxy validates every new snapshot (from the controller, or from its
config file in standalone mode) before swapping it in. When validation or
loading fails, it keeps serving the previous snapshot and counts a failure:

```promql
increase(ferry_config_reloads_total{result="failure"}[10m]) > 0        # proxy side
increase(ferry_controller_snapshot_nacks_total[10m]) > 0                # controller side, per gateway
```

Typical causes:

- A certificate or key that does not parse, or a key that does not match
  its certificate.
- A listener port that cannot be bound (taken by another process, or
  below 1024 without the capability).
- Version skew: the controller is newer than the proxy and sends a field the
  proxy does not know (ADR 0033). The proxy NACKs and keeps its snapshot.
- In standalone mode: a YAML edit that does not validate.

## Impact

The fleet serves the last good configuration. Changes since then (new
routes, new certificates, endpoint changes) are not live, and different
proxies may serve different versions. Endpoint changes stalling is the
dangerous part: removed pods keep getting traffic until health checks and
outlier detection catch up.

## Diagnosis

1. **The reason.** The controller records each proxy's last NACK:

   ```sh
   kubectl -n ferry-system port-forward deploy/ferry-controller 18001:18001
   curl -s localhost:18001/debug/proxies | jq '.proxies | to_entries[] | select(.value.last_error != null)'
   ```

   The proxy logs the same reason at `warn`: `rejected snapshot; keeping the
   current one`.

   ```sh
   kubectl -n ferry-system logs deploy/<fleet> | grep -i "rejected snapshot"
   ```

2. **Which versions?** `ferry_config_version` per proxy shows who is behind;
   compare it with the controller's `ferry_controller_snapshot_version` for
   that Gateway. `/debug/proxies` shows each proxy's acked version.

3. **Version skew.** Compare images:

   ```sh
   kubectl -n ferry-system get deploy -o custom-columns=NAME:.metadata.name,IMAGE:.spec.template.spec.containers[0].image
   ```

   During an upgrade the controller rolls first; proxies catch up as the
   controller re-provisions them. A NACK that names an unknown field and
   clears once proxies are on the new image is expected.

4. **Standalone mode.** Run the same file through the CLI:

   ```sh
   ferry check-config --output json /etc/ferry/config.yaml
   ```

## Mitigation

- Bad certificate Secret: fix or revert the Secret. The next snapshot
  replaces the rejected one.
- Port conflict: change the Gateway listener port or free the port.
- Version skew that does not clear: finish the rollout (or roll the
  controller back with `helm rollback`).
- Standalone: fix the file; the proxy retries on the next change.

## Escalation

Rejections lasting more than 30 minutes during an incident that needs a
config change (for example removing a bad backend): page the Ferry on-call.
