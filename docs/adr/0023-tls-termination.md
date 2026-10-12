# ADR 0023: TLS termination with rustls, SNI by certificate names, polled rotation

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M2

## Context

v1 terminates TLS 1.2 and 1.3 with rustls on the aws-lc-rs provider, supports
several certificates per listener chosen by SNI, reads certificates from
Kubernetes Secrets issued by cert-manager, and rotates them without a restart.
In standalone mode certificates are PEM files; the controller (M4) will pass
inline PEM.

## Decision

- **Sources:** each certificate is either a `cert_file`/`key_file` pair or inline
  `cert_pem`/`key_pem`. Private keys are wrapped in `Secret` and never logged.
  Loading uses `CertifiedKey::from_der`, which rejects a key that does not match
  its certificate.
- **SNI:** no per-certificate name list to keep in sync. During the handshake
  the resolver returns the first certificate whose own DNS names (checked with
  `rustls-webpki`, wildcards included) cover the client's SNI, else the first
  certificate. ALPN offers `h2` and `http/1.1`.
- **Order on a connection:** PROXY header (ADR 0017), then the TLS handshake
  (10 s limit), then HTTP. `X-Forwarded-Proto` becomes `https`.
- **Rotation:** every second the proxy re-reads each TLS listener's sources,
  hashes the bytes, and atomically swaps in new certificates when they change.
  New handshakes use them; established connections are unaffected. A read or
  parse failure keeps the old certificates and logs a warning.
- **Reloads are now fallible:** certificates load before any runtime state is
  touched (`prepare_tls`). If any listener's certificates fail, `swap` returns
  an error and the old snapshot keeps serving; the file watcher reports it as a
  rejected config. At startup the same failure stops the proxy from binding.
  Unchanged TLS settings reuse the loaded certificates.
- `min_version` is 1.2 (default) or 1.3.

## Consequences

- cert-manager can renew certificates in a mounted Secret with no Ferry
  involvement; renewal lands within about a second.
- Handshakes parse each candidate certificate to match SNI. That is
  microseconds for a handful of certificates; a listener with hundreds would
  want a precomputed name index.
- aws-lc-rs builds from source (C) but needs neither CMake nor NASM on our
  platforms, and its licences pass `cargo deny`.
