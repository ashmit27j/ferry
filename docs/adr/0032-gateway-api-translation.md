# ADR 0032: Translate the Gateway API with one pure function

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M4

## Context

The controller turns Gateway API objects (GatewayClass, Gateway, HTTPRoute,
GRPCRoute, ReferenceGrant) plus Services, EndpointSlices, TLS Secrets,
Namespaces and FerryPolicies into the configuration proxies run, and reports
what it decided as status conditions. The conformance suite checks both: the
traffic and the exact condition reasons. Two controller replicas must agree on
the result, and the logic must be testable without a cluster.

The `gateway-api` crate (kube-rs) binds Gateway API v1.5.1 on kube 3.1;
the current releases are v1.6.3 and kube 4.2.

## Decision

- **Types:** generate them from the official v1.6.3 standard-channel CRDs
  with kopium (`crates/ferry-controller/gen/gateway_api.py`), for the five
  kinds Ferry reads. Every generated enum gets an `Unknown` variant
  (`#[serde(other)]`), so a value from a newer API release breaks one field,
  not the whole watch. ReferenceGrant is read at `v1beta1`, served by every
  release Ferry supports.
- **Shape:** `translate(&World, &Settings) -> Translation` is pure: no I/O, no
  clock, no randomness. A `World` holds `Arc`s of every cached object; tests
  build one from YAML manifests (`World::from_manifests`).
- **Listeners:** one Ferry listener per Gateway *port*; Gateway listeners on
  that port contribute routes and certificates. Ports below 1024 bind
  `port + 10000` in the container (`advertised_port` keeps redirects right),
  so proxies need no capabilities. Supported protocols: HTTP and HTTPS
  (TLS mode Terminate). Conflicts follow the spec: a second protocol on a port
  is `ProtocolConflict`, a repeated hostname `HostnameConflict`; the first
  listener wins. Container-port collisions and the admin port are
  `PortUnavailable`.
- **Attachment:** `sectionName` and `port` narrow the candidate listeners,
  `allowedRoutes` (kinds; Same, All or Selector namespaces) filters them, and
  the route attaches to every candidate whose hostname intersects the route's
  (the narrower name wins). Route hostnames on one Ferry listener are the
  union over the listeners it attached to.
- **Precedence ties** go to the oldest route, then namespace/name: routes are
  emitted in that order and the router breaks ties by position (ADR 0009).
- **Backends:** a Service port becomes one cluster per upstream protocol
  (`ns_svc_port`, `_h2` for gRPC or `appProtocol: kubernetes.io/h2c`), whose
  endpoints are the EndpointSlice addresses not marked unready, at the slice
  port named like the Service port. Invalid references (`InvalidKind`,
  `BackendNotFound`, `RefNotPermitted`, ExternalName) keep their weight as
  `invalid_backend_weight`, answered 500; a rule without valid backends gets
  weight 1 of 500s. A Service with no ready endpoints answers 503.
- **gRPC:** a GRPCRoute method match is a path match on `/service/method`
  (exact, service prefix, or an anchored regex for a lone method).
- **Unsupported** filters (RequestMirror, ExtensionRef, CORS, backendRef
  filters) make the route `Accepted=False, reason=UnsupportedValue`.
- The translator validates every config it builds; one that fails is a bug,
  logged and never published (`GatewayOutput::valid`). A proptest feeds it
  random Gateways and routes and requires every config to validate.

## Consequences

- Gateway API semantics live in one module with fast, deterministic tests
  (`translate/tests.rs`), mirroring the conformance manifests.
- Regenerating types for a new Gateway API release is one script run; the
  `Unknown` variants make a mismatched cluster degrade field by field.
- Listener isolation (`GatewayHTTPListenerIsolation`) is not implemented: a
  request whose exact-host listener has no matching rule can fall through
  to a catch-all listener's routes on the same port. Revisit with ListenerSet.
- Per-listener certificate choice is by SNI against each certificate's names,
  not by listener hostname.
