# ADR 0024: Upstream TLS and mutual TLS per cluster

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

v1 makes upstream mTLS optional per backend. Backends may speak TLS with a
public certificate, a private CA, or require a client certificate. Ferry's own
pool (ADR 0011) dials connections, so TLS has to fit there, and health checks
to a TLS-only backend must use TLS as well.

## Decision

- `clusters[].tls` holds `sni` (default: the cluster name), trusted roots
  (`ca_file` or `ca_pem`, otherwise the Mozilla roots from `webpki-roots`), and
  an optional `client_certificate` for mTLS. Ferry builds one rustls
  `ClientConfig` per cluster, with ALPN `h2` for `protocol: http2` and
  `http/1.1` otherwise. The certificate must cover `sni`; there is no option
  to skip verification.
- Pools are keyed by `(address, protocol, TLS fingerprint)`, so plaintext and
  TLS connections, or two different client identities, to one address never
  share a connection. The handshake is part of connecting and counts against
  `connect_timeout`; a failed handshake is a connect failure (safe to retry,
  and an outlier gateway failure).
- HTTP/2 upstreams over TLS get `:scheme https`.
- Health checks to a TLS cluster use the same `ClientConfig`; HTTP probes speak
  whichever protocol ALPN negotiated.
- Like listener certificates (ADR 0023), client configurations are built
  before any runtime state changes, and reused when a cluster's TLS settings
  and protocol are unchanged. A broken CA or client certificate rejects the
  config.

## Consequences

- Backends that require mTLS work. The client identity is per cluster, so two
  clusters can present different certificates to the same service.
- Client certificates and CA files are read when a config is loaded, not
  polled: rotating them needs a config change (in Kubernetes, a Secret update
  makes the controller push a new snapshot, M4). Listener certificates are
  polled (ADR 0023) because cert-manager rotates them in place.
