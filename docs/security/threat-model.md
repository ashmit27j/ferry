# Ferry threat model

Status: M5, 2026-10-13. Review at every milestone gate and whenever a change
adds a listener, a parser, a Kubernetes permission or a new trust boundary.

The method is STRIDE per trust boundary. Each threat names its mitigation
and the test or document that shows it. What is not mitigated is listed
under **residual risks**, with an owner milestone.

## System and assets

```
 Internet ──TCP──▶ AWS NLB ──PROXY v2──▶ ferry proxy pods ──HTTP/TLS/mTLS──▶ backend pods
                                            │ ▲
                         admin :19001 ◀─────┘ │ snapshot stream (gRPC, mTLS) :18000
                         (Prometheus)         │
                                        ferry controller pods (2, Lease leader)
                                              │ watch / status / SSA
                                              ▼
                                      Kubernetes API server
```

| Asset | Where it lives | Why it matters |
| --- | --- | --- |
| Listener private keys | Kubernetes TLS Secrets → controller memory → snapshot (inline) → proxy memory | Impersonation of every hostname served |
| Upstream client keys (mTLS) | Snapshot, proxy memory | Impersonation towards backends |
| Snapshot-stream PKI (CA, server and client keys) | Two Secrets in the release namespace | Whoever holds the client key receives snapshots (and so the listener keys); whoever holds the CA can impersonate the controller |
| Client traffic | In flight through the proxy | Confidentiality and integrity of user data |
| Routing intent | Gateway, HTTPRoute, GRPCRoute, ReferenceGrant, FerryPolicy | A forged or hijacked route diverts traffic |
| Controller's Kubernetes credentials | Controller pod service account | Cluster-wide read of Secrets (see R2) |
| Availability of the data plane | Proxy fleet | Ferry is the front door of every service behind it |

## Actors

- **Anonymous client** on the Internet: sends arbitrary bytes to listeners.
- **Backend**: trusted to serve its own route, but may be buggy or
  compromised, so its responses are untrusted input.
- **Route author**: may create HTTPRoutes, GRPCRoutes, ReferenceGrants and
  FerryPolicies in their own namespace (multi-tenant clusters).
- **Cluster operator**: installs the chart and owns the Ferry namespace and
  Gateways. Trusted.
- **In-cluster workload**: any pod on the cluster network (compromised or
  not), which can reach pod IPs unless NetworkPolicy says otherwise.

## Trust boundaries and threats

### B1. Internet → NLB → proxy listener

| STRIDE | Threat | Mitigation | Evidence |
| --- | --- | --- | --- |
| S | Spoofed client address in `X-Forwarded-For` | The real address comes from PROXY v2, which only the NLB can send (a listener with `proxy_protocol: true` drops connections without a valid header). Ferry appends to XFF and never trusts earlier entries | ADR 0017; `tests/it/edge.rs` |
| S | Wrong certificate served for a hostname | SNI resolver matches DNS names; unknown SNI falls back to the first certificate (no key mix-up across listeners) | ADR 0023; `tls.rs` unit tests, `tests/it/tls.rs` |
| T | Request smuggling (CL/TE, conflicting CL, obs-fold) | hyper rejects conflicting lengths and bad TE; CL+TE is decoded by TE and the connection closed; Ferry re-frames the body towards backends | ADR 0016; `tests/it/security.rs` |
| T | Header injection into upstream requests | `http` crate rejects CR/LF/NUL in names and values; hop-by-hop headers stripped; `X-Forwarded-Proto` overwritten; `X-Request-Id` limited to 1–128 visible ASCII | ADR 0016, 0017 |
| I | TLS downgrade or weak ciphers | rustls (aws-lc-rs), TLS 1.2 and 1.3 only, no renegotiation, no compression | ADR 0023 |
| D | Header floods, oversized bodies | 60 KiB / 100 headers (431), body limit (413), header read timeout (408) | ADR 0016; `tests/it/security.rs` |
| D | HTTP/2 Rapid Reset, MadeYouReset, CONTINUATION flood, stream floods | Reset caps then GOAWAY, header list limit, 100 concurrent streams | ADR 0016; `tests/it/h2_attacks.rs` |
| D | Slowloris on HTTP/1.1 | Header read timeout closes connections that do not finish headers | ADR 0016 |
| D | Many idle connections exhaust file descriptors or memory | Listener `max_connections` refuses new connections over the cap; idle WebSocket and other upgraded tunnels close after `tunnel_idle` (M5) | ADR 0037; `tests/it/limits.rs` |
| D | Parser crash on hostile bytes | No panics in library code (lint-enforced); PROXY protocol, config, snapshot, gRPC timeout, traceparent and route parsers fuzzed nightly | ADR 0006; `fuzz/` |
| D | Regex denial of service in route matching | Rust `regex` runs in linear time; compiled size capped (`MAX_REGEX_SIZE`) | `snapshot/matching.rs` |
| E | Memory corruption | `unsafe` is forbidden workspace-wide | ADR 0006 |

### B2. Proxy → backends

| STRIDE | Threat | Mitigation | Evidence |
| --- | --- | --- | --- |
| S | Backend impersonation on the pod network | Optional upstream TLS with CA pinning and SNI, and mTLS with a client certificate | ADR 0024; `tests/it/upstream_tls.rs` |
| T | Response splitting or smuggling from a hostile backend | hyper parses responses strictly; hop-by-hop headers stripped; bodies re-framed downstream | ADR 0016 |
| D | Slow or failing backend exhausts the proxy | Per-route and per-try timeouts, circuit breakers (connections, pending, requests, retries), retry budget, outlier ejection | ADRs 0018–0021 |
| I | Upstream errors leak internals to clients | Local replies have fixed bodies; upstream failure details go to logs and the access log only | ADR 0029 |

### B3. Controller ↔ proxy snapshot stream

| STRIDE | Threat | Mitigation | Evidence |
| --- | --- | --- | --- |
| S | Rogue proxy subscribes to snapshots (and so to listener keys) | Controller requires a client certificate from the certgen CA | `tests/it/snapshots.rs` (`proxies_without_a_trusted_client_certificate_are_refused`) |
| S | Rogue controller pushes config | Proxy verifies the controller's certificate against the CA and its Service name | ADR 0033 |
| T | Malformed or hostile snapshot | Parsed and validated exactly like a file config; failure NACKs and keeps the last good snapshot; 64 MiB message cap | ADR 0033; `snapshot_json` fuzz target |
| D | Controller outage | Proxies keep serving the last snapshot and reconnect with backoff; `ferry_controller_connected` alerts after 5 min | Runbook `controller-disconnected` |
| I | Snapshot read in transit | TLS 1.3 on the stream | ADR 0033 |

### B4. Kubernetes API → controller (route authors and operators)

| STRIDE | Threat | Mitigation | Evidence |
| --- | --- | --- | --- |
| S | Route attaches to a Gateway it may not use | `allowedRoutes` (namespaces, kinds) enforced in translation; status says `NotAllowedByListeners` | Gateway API conformance (core) |
| T | Route in namespace A sends traffic to a Service in B | Cross-namespace `backendRefs` need a ReferenceGrant in B; otherwise `RefNotPermitted` and a 500 for that backend | conformance `HTTPRouteReferenceGrant`; `translate/tests.rs` |
| I | Gateway in A reads a TLS Secret in B | Cross-namespace `certificateRefs` need a ReferenceGrant | conformance `GatewaySecretReferenceGrant*` |
| I | A FerryPolicy changes another tenant's route | `targetRefs` are local to the policy's namespace | ADR 0034 |
| D | Huge or many objects slow translation | Translation is pure and linear (8.5 µs per route); debounced to one pass per 200 ms | `bench/RESULTS.md` (M4) |
| E | Controller credentials abused | Least-privilege RBAC: cluster-wide read of what it translates, status writes only; writes to Deployments, Services, PDBs and HPAs only in the release namespace; proxies get no token | ADR 0035; `templates/rbac.yaml` |

### B5. Cluster network → admin and control ports

| STRIDE | Threat | Mitigation | Evidence |
| --- | --- | --- | --- |
| I | Any pod reads `/config` from a proxy's admin port | Private keys serialise as `<redacted>`; the chart's NetworkPolicy admits only the monitoring namespace to 19001 (M5) | ADR 0030; `templates/networkpolicy.yaml` |
| T | Admin API changes behaviour | Admin is GET-only; the admin address is a process flag a snapshot cannot move | ADR 0030 |
| S | Any pod connects to the controller's gRPC port | mTLS (B3), and the NetworkPolicy admits only proxy pods to 18000 (M5) | `templates/networkpolicy.yaml` |
| I | `/debug/proxies` on 18001 lists proxy names, versions and NACK reasons | No secrets in it; NetworkPolicy limits 18001 to probes and monitoring | `run.rs` |

### B6. Supply chain

| Threat | Mitigation |
| --- | --- |
| Vulnerable or malicious crate | `cargo audit` and `cargo deny` (advisories, licenses, sources, bans) gate every commit; Dependabot weekly |
| Compromised GitHub Action | Actions pinned by commit SHA; minimal `permissions:` per workflow |
| Tampered image | Cosign keyless signature and SPDX SBOM attestation per release; distroless base, non-root, read-only root filesystem |

## Residual risks

| # | Risk | Severity | Why accepted now | Plan |
| --- | --- | --- | --- | --- |
| R1 | All proxies share one client certificate, and any proxy can ask for any Gateway's snapshot. A compromised proxy pod can fetch every Gateway's listener keys | Medium | Proxies run in one namespace managed by one controller; a compromised proxy already holds its own Gateway's keys | Per-Gateway client identities (SPIFFE-style SAN checked against the hello's Gateway), M6 |
| R2 | The controller can read every Secret in the cluster (RBAC cannot express "only kubernetes.io/tls") | Medium | The controller lists with a type field selector, and its pod has no other attack surface than the Kubernetes API and its own mTLS port | Optional namespace allow-list in the chart, M6 |
| R3 | CL+TE requests are decoded by TE and the connection closed, not answered 400 | Low | Matches RFC 9112 §6.1; no desync because the connection closes | Revisit if hyper exposes the conflict (ADR 0016) |
| R4 | Snapshot-stream certificates are read at start; after certgen renews them, pods need a restart | Low | Certgen renews 90 days before expiry; the expiry alert fires 30 days before | Hot reload of stream credentials, M6 |
| R5 | Rate limits are per replica, divided by the controller; uneven client spread lets a client exceed the cluster-wide limit by up to the replica count | Low | Documented approximation (ADR 0022, 0034) | Global limits are post-v1.0 |
| R6 | HTTP/2 connections that keep answering pings stay open until drain | Low | Bounded by `max_connections` (M5) | Connection max age, if needed in M6 |

## Review log

| Date | Change | Findings |
| --- | --- | --- |
| 2026-10-13 | First full model (M5) | Added NetworkPolicies (B5), `max_connections` and `tunnel_idle` (B1), fuzz targets for every parser (B1, B3); R1–R6 accepted |
