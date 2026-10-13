# FerryCertificateExpiringSoon and FerryCertificateExpiryCritical

**Severity:** warning when a certificate Ferry presents expires within 21
days; critical within 7 days.

## Meaning

`ferry_tls_certificate_expiry_timestamp_seconds` is the `notAfter` of each
certificate a proxy presents, read from what it is serving right now:

| `kind` | `owner` | What it is | Who renews it |
| --- | --- | --- | --- |
| `listener` | listener name | A Gateway listener's certificate (from a `kubernetes.io/tls` Secret) | cert-manager, usually 30 days before expiry |
| `upstream` | cluster name | The client certificate for mTLS to a backend (FerryPolicy `backendTLS`) | whoever issues that Secret |
| `controller` | `snapshot-stream` | The proxy's client certificate for the controller stream | `ferry certgen` on `helm upgrade`, 90 days before expiry |

```promql
(min by (fleet, kind, owner, name) (ferry_tls_certificate_expiry_timestamp_seconds) - time()) / 86400   # days left
```

cert-manager renews a 90-day certificate 30 days before it expires, so a
certificate with 21 days left means renewal has failed for over a week.

## Impact

None until expiry. After it:

- `listener`: clients get TLS errors for that hostname.
- `upstream`: the backend rejects Ferry's connections, so the route returns 502.
- `controller`: proxies cannot reconnect to the controller. They keep
  serving their last snapshot, but config changes stop reaching them.

## Diagnosis

1. **Which certificate?** The alert's `kind`, `owner` and `name` labels.
   `name` is the certificate's first DNS name.

2. **Listener certificates.** Find the Secret in the Gateway's listener
   (`spec.listeners[].tls.certificateRefs`), then ask cert-manager why it
   did not renew:

   ```sh
   kubectl -n <ns> get certificate,certificaterequest,order,challenge
   kubectl -n <ns> describe certificate <name>
   kubectl -n cert-manager logs deploy/cert-manager --since=24h | grep -i <name>
   ```

   DNS-01 failures (Route 53 permissions, a deleted hosted zone) are the
   usual cause. Once the Secret holds a new certificate, the controller
   pushes it and proxies swap it in on the next handshake; no restart is
   needed. Confirm the metric moves.

3. **Secret renewed, but the metric did not move?** Check that the
   Gateway's status is `Programmed=True` with `ResolvedRefs=True`, that
   proxies ACK the latest snapshot (`/debug/proxies` on the controller), and
   that no `FerryConfigRejected` alert is firing.

4. **Upstream client certificates.** Renew the Secret named in the
   FerryPolicy's `backendTLS`; the controller pushes it like a listener
   certificate.

5. **Controller stream certificates.** These are read when pods start
   (residual risk R4 in the threat model):

   ```sh
   helm upgrade ferry deploy/helm/ferry -n ferry-system --reuse-values   # certgen hook renews them
   kubectl -n ferry-system rollout restart deploy/ferry-controller
   kubectl -n ferry-system rollout restart deploy -l app.kubernetes.io/name=ferry-proxy
   ```

   Restart the controller first, then the proxies (rolling restarts drop no
   requests).

## Mitigation

- If cert-manager cannot renew in time: issue a certificate by hand and
  write it to the same Secret (`kubectl create secret tls ... --dry-run=client -o yaml | kubectl apply -f -`).
  Ferry picks it up like a renewal.
- Never delete a listener's Secret to "force" renewal: the listener then
  loses its certificate (`ResolvedRefs=False`) and stops serving TLS.

## Escalation

Critical with less than 3 days left and no renewal path: page the platform
on-call (cert-manager and DNS owners).
